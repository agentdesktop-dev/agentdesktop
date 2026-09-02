use std::{
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCapability, AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision,
    AccessEnforcement, AccessOperation, AccessRuleMechanism, AccessSourceKind,
};
use jsonc_parser::ParseOptions;
use serde_json::Value;

use super::{
    CollectedAccess,
    configuration::default_capability,
    history_scan::{HistoryAdapter, RuntimeCollector, permission_mode, runtime_capability},
    host_pattern, network_rule_ref, plural_suffix, safe_command_identifier, source,
};

pub(super) fn history_adapter(home: &Path) -> HistoryAdapter {
    HistoryAdapter {
        kind: "vscode",
        root: user_root(home).join("workspaceStorage"),
        include_file: Some(is_history_file),
        workspace_for_file: Some(workspace_for_history),
        inspect_runtime,
        coverage_limitation: None,
    }
}

fn is_history_file(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent
            .file_name()
            .is_some_and(|name| name == "chatSessions")
    })
}

fn workspace_for_history(path: &Path) -> Option<PathBuf> {
    let workspace_storage = path.parent()?.parent()?;
    let contents = fs::read(workspace_storage.join("workspace.json")).ok()?;
    let document: Value = serde_json::from_slice(&contents).ok()?;
    let folder = document.get("folder").and_then(Value::as_str)?;
    url::Url::parse(folder).ok()?.to_file_path().ok()
}

fn inspect_runtime(
    object: &serde_json::Map<String, Value>,
    workspace: Option<&Path>,
    collected: &mut RuntimeCollector,
) {
    if let Some(level) = object
        .get("modeInfo")
        .and_then(Value::as_object)
        .and_then(|mode| mode.get("permissionLevel"))
        .and_then(Value::as_str)
    {
        let (decision, enforcement) = permission_mode(level);
        collected.record(runtime_capability(
            AccessCategory::Execution,
            "agent tools",
            vec![AccessOperation::Execute],
            decision,
            enforcement,
            workspace,
            &format!("Recorded VS Code permission level: {level}"),
        ));
    }

    if object.get("terminalCommandId").is_some()
        && let Some(sandboxed) = object.get("isSandboxWrapped").and_then(Value::as_bool)
    {
        collected.record(runtime_capability(
            AccessCategory::Execution,
            "recorded terminal commands",
            vec![AccessOperation::Execute],
            AccessDecision::Allow,
            if sandboxed {
                AccessEnforcement::Sandbox
            } else {
                AccessEnforcement::None
            },
            workspace,
            if sandboxed {
                "VS Code recorded sandbox-wrapped terminal execution"
            } else {
                "VS Code recorded terminal execution without a sandbox wrapper"
            },
        ));
    }
}

pub(super) fn inspect_configuration(home: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let mut inspected = 0;
    for path in settings_paths(home) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = json5::from_str::<Value>(&contents) else {
            continue;
        };
        inspected += 1;
        let editable = jsonc_parser::parse_to_value(&contents, &jsonc_options()).is_ok();
        parse_settings(&document, &path, editable, &mut collected);
    }
    collected.capabilities.extend([
        default_capability(
            AccessCategory::Filesystem,
            "active workspace",
            vec![AccessOperation::Read],
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            "VS Code agent workspace access",
        ),
        default_capability(
            AccessCategory::Filesystem,
            "active workspace",
            vec![AccessOperation::Write],
            AccessDecision::Ask,
            AccessEnforcement::Harness,
            "Write approval depends on the session permission level",
        ),
        default_capability(
            AccessCategory::Execution,
            "shell commands",
            vec![AccessOperation::Execute],
            AccessDecision::Ask,
            AccessEnforcement::Unknown,
            "Terminal containment depends on session and VS Code settings",
        ),
        default_capability(
            AccessCategory::Network,
            "URL tools",
            vec![AccessOperation::Connect],
            AccessDecision::Ask,
            AccessEnforcement::Harness,
            "Unmatched URL tool requests require approval",
        ),
    ]);
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Configuration,
        status: if inspected > 0 {
            AccessCoverageStatus::Partial
        } else {
            AccessCoverageStatus::Unavailable
        },
        detail: if inspected > 0 {
            format!(
                "Inspected {inspected} VS Code settings file{}; profile and workspace settings are not assessed",
                plural_suffix(inspected),
            )
        } else {
            "No readable VS Code settings files were found".to_owned()
        },
    });
    collected
}

fn jsonc_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn parse_settings(document: &Value, path: &Path, editable: bool, collected: &mut CollectedAccess) {
    if let Some(rules) = document
        .get("chat.tools.urls.autoApprove")
        .and_then(Value::as_object)
    {
        for (pattern, value) in rules {
            let Some(resource) = host_pattern(pattern) else {
                continue;
            };
            let editable_rule =
                editable && value.is_boolean() && is_editable_url_pattern(pattern, &resource);
            let rule = editable_rule.then(|| {
                network_rule_ref(AccessRuleMechanism::VscodeUrlAutoApprove, path, pattern)
            });
            let detail = if editable_rule {
                "VS Code URL tool auto-approval"
            } else if !editable {
                "Read-only because this settings file uses syntax Agentdesktop cannot safely rewrite"
            } else if !value.is_boolean() {
                "Advanced: request and response approvals are configured separately. Edit this rule in the configuration file"
            } else {
                "Read-only because this VS Code URL rule includes a path, port, or other advanced pattern"
            };
            collected.capabilities.push(AccessCapability {
                category: AccessCategory::Network,
                resource,
                operations: vec![AccessOperation::Connect],
                decision: approval(value),
                enforcement: AccessEnforcement::Harness,
                workspace: None,
                source: source(AccessSourceKind::Configuration, Some(path.to_path_buf())),
                rule,
                detail: Some(detail.to_owned()),
            });
        }
    }
    if let Some(rules) = document
        .get("chat.tools.terminal.autoApprove")
        .and_then(Value::as_object)
    {
        for (command, value) in rules {
            collected.capabilities.push(AccessCapability {
                category: AccessCategory::Execution,
                resource: safe_command_identifier(command),
                operations: vec![AccessOperation::Execute],
                decision: approval(value),
                enforcement: AccessEnforcement::Harness,
                workspace: None,
                source: source(AccessSourceKind::Configuration, Some(path.to_path_buf())),
                rule: None,
                detail: Some("VS Code terminal auto-approval".to_owned()),
            });
        }
    }
}

fn is_editable_url_pattern(pattern: &str, resource: &str) -> bool {
    let pattern = pattern.trim();
    pattern.eq_ignore_ascii_case(resource)
        || pattern.eq_ignore_ascii_case(&format!("https://{resource}"))
        || pattern.eq_ignore_ascii_case(&format!("http://{resource}"))
}

fn approval(value: &Value) -> AccessDecision {
    if value.as_bool() == Some(true)
        || value
            .as_object()
            .is_some_and(|object| object.values().any(|value| value.as_bool() == Some(true)))
    {
        AccessDecision::Allow
    } else {
        AccessDecision::Ask
    }
}

fn settings_paths(home: &Path) -> Vec<PathBuf> {
    vec![user_root(home).join("settings.json")]
}

#[cfg(target_os = "macos")]
fn user_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Code/User")
}

#[cfg(target_os = "linux")]
fn user_root(home: &Path) -> PathBuf {
    home.join(".config/Code/User")
}

#[cfg(windows)]
fn user_root(home: &Path) -> PathBuf {
    home.join("AppData/Roaming/Code/User")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::model::{
        AccessCategory, AccessDecision, AccessEnforcement, AccessRuleMechanism, AccessSourceKind,
    };
    use serde_json::json;

    use super::{inspect_runtime, parse_settings};
    use crate::access::{CollectedAccess, history_scan::RuntimeCollector};

    #[test]
    fn reduces_url_and_command_rules() {
        let mut collected = CollectedAccess::default();
        parse_settings(
            &json!({
                "chat.tools.urls.autoApprove": {
                    "https://*.example.com/private?token=secret": true,
                    "api.example.com": true
                },
                "chat.tools.terminal.autoApprove": {
                    "TOKEN=secret kubectl get pods": true
                }
            }),
            Path::new("settings.json"),
            true,
            &mut collected,
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Network && capability.resource == "*.example.com"
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.resource == "*.example.com" && capability.rule.is_none()
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.resource == "api.example.com"
                && capability.rule.as_ref().is_some_and(|rule| {
                    rule.mechanism == AccessRuleMechanism::VscodeUrlAutoApprove
                        && rule.id.len() == 64
                })
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Execution && capability.resource == "kubectl get"
        }));
        assert!(!format!("{:?}", collected.capabilities).contains("secret"));
    }

    #[test]
    fn json5_only_sources_do_not_expose_edit_handles() {
        let mut collected = CollectedAccess::default();
        parse_settings(
            &json!({
                "chat.tools.urls.autoApprove": {
                    "api.example.com": true
                }
            }),
            Path::new("settings.json"),
            false,
            &mut collected,
        );

        assert!(
            collected
                .capabilities
                .iter()
                .all(|capability| capability.rule.is_none())
        );
    }

    #[test]
    fn simple_origin_url_rules_expose_edit_handles() {
        let mut collected = CollectedAccess::default();
        parse_settings(
            &json!({
                "chat.tools.urls.autoApprove": {
                    "https://api.example.com": true,
                    "https://*.example.org": false,
                    "https://request.example.net": { "approveRequest": true },
                    "https://response.example.net": { "approveResponse": true },
                    "https://api.example.net/private": true,
                    "https://advanced.example.com": { "allow": true }
                }
            }),
            Path::new("settings.json"),
            true,
            &mut collected,
        );

        for resource in ["api.example.com", "*.example.org"] {
            assert!(collected.capabilities.iter().any(|capability| {
                capability.resource == resource && capability.rule.is_some()
            }));
        }
        for resource in [
            "request.example.net",
            "response.example.net",
            "api.example.net",
            "advanced.example.com",
        ] {
            assert!(collected.capabilities.iter().any(|capability| {
                capability.resource == resource && capability.rule.is_none()
            }));
        }
        assert!(collected.capabilities.iter().any(|capability| {
            capability.resource == "request.example.net"
                && capability
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.starts_with("Advanced:"))
        }));
    }

    #[test]
    fn records_permission_and_terminal_containment() {
        let value = json!({
            "modeInfo": { "permissionLevel": "allowAll" },
            "terminalCommandId": "terminal-1",
            "isSandboxWrapped": false
        });
        let mut runtime = RuntimeCollector::default();
        inspect_runtime(
            value.as_object().unwrap(),
            Some(Path::new("/workspace")),
            &mut runtime,
        );
        let (capabilities, limited) = runtime.finish();

        assert!(!limited);
        assert!(capabilities.iter().all(|capability| {
            capability.source.kind == AccessSourceKind::History
                && capability.decision == AccessDecision::Allow
                && capability.enforcement == AccessEnforcement::None
        }));
        assert_eq!(capabilities.len(), 2);
    }
}
