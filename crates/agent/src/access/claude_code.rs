use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCapability, AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision,
    AccessEnforcement, AccessFinding, AccessOperation, AccessRuleMechanism, AccessSeverity,
    AccessSourceKind,
};
use serde_json::Value;

use super::{
    CollectedAccess,
    configuration::{capability, default_capability},
    history_scan::{HistoryAdapter, RuntimeCollector, permission_mode, runtime_capability},
    host_pattern, network_rule_ref, plural_suffix, safe_command_identifier, source,
};

pub(super) fn history_adapter(home: &Path) -> HistoryAdapter {
    HistoryAdapter {
        kind: "claude-code",
        root: config_dir(home).join("projects"),
        include_file: None,
        workspace_for_file: None,
        inspect_runtime,
        coverage_limitation: None,
    }
}

pub(super) fn inspect_runtime(
    object: &serde_json::Map<String, Value>,
    workspace: Option<&Path>,
    collected: &mut RuntimeCollector,
) {
    if let Some(mode) = object.get("permissionMode").and_then(Value::as_str) {
        let (decision, enforcement) = permission_mode(mode);
        collected.record(runtime_capability(
            AccessCategory::Execution,
            "tool calls",
            vec![AccessOperation::Execute],
            decision,
            enforcement,
            workspace,
            &format!("Recorded Claude permission mode: {mode}"),
        ));
    }
}

pub(super) fn inspect_configuration(home: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let state_path = home.join(".claude.json");
    let project_roots = inspect_state(&state_path, &mut collected);
    let mut settings = BTreeSet::new();
    settings.insert((config_dir(home).join("settings.json"), None, true));
    settings.extend(
        managed_settings()
            .into_iter()
            .map(|path| (path, None, false)),
    );
    for workspace in project_roots {
        settings.insert((
            workspace.join(".claude/settings.json"),
            Some(workspace.clone()),
            true,
        ));
        settings.insert((
            workspace.join(".claude/settings.local.json"),
            Some(workspace),
            true,
        ));
    }

    let mut inspected = usize::from(state_path.is_file());
    for (path, workspace, editable) in settings {
        let Ok(contents) = fs::read(&path) else {
            continue;
        };
        let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
            continue;
        };
        inspected += 1;
        parse_settings(
            &document,
            &path,
            workspace.as_deref(),
            editable,
            &mut collected,
        );
    }
    collected.capabilities.extend([
        default_capability(
            AccessCategory::Filesystem,
            "session working directory",
            vec![AccessOperation::Read],
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            "Claude reads files in its working directory without prompting",
        ),
        default_capability(
            AccessCategory::Filesystem,
            "session working directory",
            vec![AccessOperation::Write],
            AccessDecision::Ask,
            AccessEnforcement::Harness,
            "Write approval depends on the active permission mode",
        ),
        default_capability(
            AccessCategory::Network,
            "WebFetch domains",
            vec![AccessOperation::Connect],
            AccessDecision::Ask,
            AccessEnforcement::Harness,
            "Unmatched WebFetch domains require approval",
        ),
    ]);
    if !collected.capabilities.iter().any(|capability| {
        capability.category == AccessCategory::Execution
            && capability.enforcement == AccessEnforcement::Sandbox
    }) {
        collected.capabilities.push(default_capability(
            AccessCategory::Execution,
            "shell subprocesses",
            vec![AccessOperation::Execute],
            AccessDecision::Ask,
            AccessEnforcement::None,
            "Claude shell sandbox is disabled unless enabled by a session override",
        ));
        collected.findings.push(AccessFinding {
            severity: AccessSeverity::Warning,
            title: "Shell sandbox not configured".to_owned(),
            detail: "Claude shell subprocesses are approval-gated but no configured OS sandbox boundary was found. Enable the Claude sandbox for stronger containment"
                .to_owned(),
            category: AccessCategory::Execution,
            workspace: None,
            source: None,
        });
    }
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Configuration,
        status: if inspected > 0 {
            AccessCoverageStatus::Partial
        } else {
            AccessCoverageStatus::Unavailable
        },
        detail: if inspected > 0 {
            format!(
                "Inspected {inspected} Claude configuration file{}; session and plugin settings may add access",
                plural_suffix(inspected),
            )
        } else {
            "No readable Claude configuration files were found".to_owned()
        },
    });
    collected
}

fn inspect_state(path: &Path, collected: &mut CollectedAccess) -> Vec<PathBuf> {
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        return Vec::new();
    };
    let mut projects = Vec::new();
    let Some(configured) = document.get("projects").and_then(Value::as_object) else {
        return projects;
    };
    for (workspace, state) in configured {
        let workspace = PathBuf::from(workspace);
        if state.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true) {
            projects.push(workspace.clone());
            let source = source(AccessSourceKind::Configuration, Some(path.to_path_buf()));
            collected.capabilities.extend([
                AccessCapability {
                    category: AccessCategory::Filesystem,
                    resource: workspace.display().to_string(),
                    operations: vec![AccessOperation::Read],
                    decision: AccessDecision::Allow,
                    enforcement: AccessEnforcement::Harness,
                    workspace: Some(workspace.clone()),
                    source: source.clone(),
                    rule: None,
                    detail: Some("Trusted Claude workspace".to_owned()),
                },
                AccessCapability {
                    category: AccessCategory::Filesystem,
                    resource: workspace.display().to_string(),
                    operations: vec![AccessOperation::Write],
                    decision: AccessDecision::Ask,
                    enforcement: AccessEnforcement::Harness,
                    workspace: Some(workspace.clone()),
                    source,
                    rule: None,
                    detail: Some(
                        "Trusted workspace; edit gating depends on permission mode".to_owned(),
                    ),
                },
            ]);
            if let Some(rules) = state.get("allowedTools").and_then(Value::as_array) {
                for rule in rules.iter().filter_map(Value::as_str) {
                    parse_rule(
                        rule,
                        AccessDecision::Allow,
                        None,
                        path,
                        Some(&workspace),
                        false,
                        collected,
                    );
                }
            }
        }
    }
    projects
}

fn parse_settings(
    document: &Value,
    path: &Path,
    workspace: Option<&Path>,
    editable: bool,
    collected: &mut CollectedAccess,
) {
    if let Some(permissions) = document.get("permissions").and_then(Value::as_object) {
        for (key, decision) in [
            ("allow", AccessDecision::Allow),
            ("ask", AccessDecision::Ask),
            ("deny", AccessDecision::Deny),
        ] {
            if let Some(rules) = permissions.get(key).and_then(Value::as_array) {
                for rule in rules.iter().filter_map(Value::as_str) {
                    parse_rule(
                        rule,
                        decision.clone(),
                        Some(key),
                        path,
                        workspace,
                        editable,
                        collected,
                    );
                }
            }
        }
        if let Some(directories) = permissions
            .get("additionalDirectories")
            .and_then(Value::as_array)
        {
            for directory in directories.iter().filter_map(Value::as_str) {
                collected.capabilities.extend([
                    capability(
                        AccessCategory::Filesystem,
                        directory,
                        vec![AccessOperation::Read],
                        AccessDecision::Allow,
                        AccessEnforcement::Harness,
                        path,
                        workspace,
                        "Claude additional directory",
                    ),
                    capability(
                        AccessCategory::Filesystem,
                        directory,
                        vec![AccessOperation::Write],
                        AccessDecision::Ask,
                        AccessEnforcement::Harness,
                        path,
                        workspace,
                        "Write access depends on the active permission mode",
                    ),
                ]);
            }
        }
        if permissions.get("defaultMode").and_then(Value::as_str) == Some("bypassPermissions") {
            collected.findings.push(AccessFinding {
                severity: AccessSeverity::Critical,
                title: "Claude permission checks bypassed".to_owned(),
                detail: "The configured default mode skips normal tool permission prompts. Choose a mode that requires review"
                    .to_owned(),
                category: AccessCategory::Execution,
                workspace: workspace.map(Path::to_path_buf),
                source: Some(source(
                    AccessSourceKind::Configuration,
                    Some(path.to_path_buf()),
                )),
            });
        }
    }

    if let Some(environment) = document.get("env").and_then(Value::as_object) {
        for name in environment.keys() {
            collected.capabilities.push(capability(
                AccessCategory::Credential,
                name,
                vec![AccessOperation::Use],
                AccessDecision::Unknown,
                AccessEnforcement::None,
                path,
                workspace,
                "Environment variable configured; value omitted",
            ));
        }
    }
    if let Some(sandbox) = document.get("sandbox").and_then(Value::as_object) {
        parse_sandbox(sandbox, path, workspace, editable, collected);
    }
}

fn parse_rule(
    rule: &str,
    decision: AccessDecision,
    list: Option<&str>,
    path: &Path,
    workspace: Option<&Path>,
    editable: bool,
    collected: &mut CollectedAccess,
) {
    let (tool, specifier) = rule
        .split_once('(')
        .map(|(tool, rest)| (tool, rest.strip_suffix(')').unwrap_or(rest)))
        .unwrap_or((rule, "*"));
    let (category, operations, resource) = match tool {
        "Read" => (
            AccessCategory::Filesystem,
            vec![AccessOperation::Read],
            specifier.to_owned(),
        ),
        "Edit" | "Write" => (
            AccessCategory::Filesystem,
            vec![AccessOperation::Write],
            specifier.to_owned(),
        ),
        "Bash" | "PowerShell" => (
            AccessCategory::Execution,
            vec![AccessOperation::Execute],
            if specifier == "*" {
                "*".to_owned()
            } else {
                safe_command_identifier(specifier)
            },
        ),
        "WebFetch" => (
            AccessCategory::Network,
            vec![AccessOperation::Connect],
            host_pattern(specifier.strip_prefix("domain:").unwrap_or(specifier))
                .unwrap_or_else(|| specifier.to_owned()),
        ),
        tool if tool.starts_with("mcp__") => (
            AccessCategory::ExternalService,
            vec![AccessOperation::Use],
            tool.to_owned(),
        ),
        _ => return,
    };
    let mut capability = capability(
        category,
        &resource,
        operations,
        decision,
        AccessEnforcement::Harness,
        path,
        workspace,
        "Claude permission rule",
    );
    if editable
        && tool == "WebFetch"
        && let Some(list) = list
        && rule == format!("WebFetch(domain:{resource})")
    {
        capability.rule = Some(network_rule_ref(
            AccessRuleMechanism::ClaudePermission,
            path,
            &format!("permissions.{list}\0{rule}"),
        ));
    }
    collected.capabilities.push(capability);
}

fn parse_sandbox(
    sandbox: &serde_json::Map<String, Value>,
    path: &Path,
    workspace: Option<&Path>,
    editable: bool,
    collected: &mut CollectedAccess,
) {
    if sandbox.get("enabled").and_then(Value::as_bool) == Some(true) {
        collected.capabilities.push(capability(
            AccessCategory::Execution,
            "shell subprocesses",
            vec![AccessOperation::Execute],
            AccessDecision::Allow,
            AccessEnforcement::Sandbox,
            path,
            workspace,
            "Claude Bash sandbox enabled",
        ));
    }
    if sandbox
        .get("allowUnsandboxedCommands")
        .and_then(Value::as_bool)
        == Some(true)
    {
        collected.findings.push(AccessFinding {
            severity: AccessSeverity::Warning,
            title: "Unsandboxed retries allowed".to_owned(),
            detail: "Claude may request approval to retry a blocked command outside the sandbox. Disable unsandboxed command retries for strict containment"
                .to_owned(),
            category: AccessCategory::Execution,
            workspace: workspace.map(Path::to_path_buf),
            source: Some(source(
                AccessSourceKind::Configuration,
                Some(path.to_path_buf()),
            )),
        });
    }
    if let Some(filesystem) = sandbox.get("filesystem").and_then(Value::as_object) {
        if filesystem.get("disabled").and_then(Value::as_bool) == Some(true) {
            collected.findings.push(AccessFinding {
                severity: AccessSeverity::Critical,
                title: "Filesystem isolation disabled".to_owned(),
                detail: "Sandboxed commands retain unrestricted host filesystem reach. Enable filesystem isolation"
                    .to_owned(),
                category: AccessCategory::Filesystem,
                workspace: workspace.map(Path::to_path_buf),
                source: Some(source(
                    AccessSourceKind::Configuration,
                    Some(path.to_path_buf()),
                )),
            });
        }
        for (key, operation, decision) in [
            ("allowRead", AccessOperation::Read, AccessDecision::Allow),
            ("denyRead", AccessOperation::Read, AccessDecision::Deny),
            ("allowWrite", AccessOperation::Write, AccessDecision::Allow),
            ("denyWrite", AccessOperation::Write, AccessDecision::Deny),
        ] {
            for resource in string_array(filesystem.get(key)) {
                collected.capabilities.push(capability(
                    AccessCategory::Filesystem,
                    resource,
                    vec![operation.clone()],
                    decision.clone(),
                    AccessEnforcement::Sandbox,
                    path,
                    workspace,
                    "Claude sandbox filesystem rule",
                ));
            }
        }
    }
    if let Some(network) = sandbox.get("network").and_then(Value::as_object) {
        for (key, decision) in [
            ("allowedDomains", AccessDecision::Allow),
            ("deniedDomains", AccessDecision::Deny),
        ] {
            for resource in string_array(network.get(key)) {
                let mut capability = capability(
                    AccessCategory::Network,
                    resource,
                    vec![AccessOperation::Connect],
                    decision.clone(),
                    AccessEnforcement::Sandbox,
                    path,
                    workspace,
                    "Claude sandbox network rule",
                );
                if editable && host_pattern(resource).as_deref() == Some(resource) {
                    capability.rule = Some(network_rule_ref(
                        AccessRuleMechanism::ClaudeSandboxDomain,
                        path,
                        &format!("sandbox.network.{key}\0{resource}"),
                    ));
                }
                collected.capabilities.push(capability);
            }
        }
    }
}

fn managed_settings() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let root = PathBuf::from("/Library/Application Support/ClaudeCode");
    #[cfg(target_os = "linux")]
    let root = PathBuf::from("/etc/claude-code");
    #[cfg(windows)]
    let root = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Program Files"))
        .join("ClaudeCode");

    let mut paths = BTreeSet::from([root.join("managed-settings.json")]);
    if let Ok(entries) = fs::read_dir(root.join("managed-settings.d")) {
        paths.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        }));
    }
    paths.into_iter().collect()
}

fn config_dir(home: &Path) -> PathBuf {
    if crate::discovery::metadata::home_dir().as_deref() == Some(home)
        && let Some(configured) = std::env::var_os("CLAUDE_CONFIG_DIR")
        && !configured.is_empty()
    {
        return PathBuf::from(configured);
    }
    home.join(".claude")
}

fn string_array(value: Option<&Value>) -> impl Iterator<Item = &str> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use agentdesktop_core::model::{
        AccessCategory, AccessDecision, AccessEnforcement, AccessRuleMechanism, AccessSourceKind,
    };
    use serde_json::json;

    use super::{config_dir, inspect_runtime, inspect_state, parse_settings};
    use crate::access::{CollectedAccess, history_scan::RuntimeCollector};

    #[test]
    fn omits_environment_values_and_reads_sandbox_rules() {
        let mut collected = CollectedAccess::default();
        parse_settings(
            &json!({
                "env": { "DEPLOY_TOKEN": "super-secret" },
                "permissions": {
                    "allow": ["WebFetch(domain:docs.example.com)"]
                },
                "sandbox": {
                    "enabled": true,
                    "filesystem": { "denyRead": ["~/.ssh"] },
                    "network": { "allowedDomains": ["docs.example.com"] }
                }
            }),
            Path::new("settings.json"),
            Some(Path::new("/workspace")),
            true,
            &mut collected,
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Credential
                && capability.resource == "DEPLOY_TOKEN"
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.resource == "~/.ssh" && capability.decision == AccessDecision::Deny
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.enforcement == AccessEnforcement::Sandbox
                && capability.resource == "docs.example.com"
        }));
        let editable_network_rules: Vec<_> = collected
            .capabilities
            .iter()
            .filter(|capability| capability.resource == "docs.example.com")
            .filter_map(|capability| capability.rule.as_ref())
            .collect();
        assert_eq!(editable_network_rules.len(), 2);
        assert!(editable_network_rules.iter().any(|rule| {
            rule.mechanism == AccessRuleMechanism::ClaudePermission && rule.id.len() == 64
        }));
        assert!(editable_network_rules.iter().any(|rule| {
            rule.mechanism == AccessRuleMechanism::ClaudeSandboxDomain && rule.id.len() == 64
        }));
        assert_ne!(editable_network_rules[0].id, editable_network_rules[1].id);
        assert!(!format!("{:?}", collected.capabilities).contains("super-secret"));
    }

    #[test]
    fn only_returns_trusted_project_roots() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-claude-projects-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "projects": {
                    "/workspace/trusted": { "hasTrustDialogAccepted": true },
                    "/workspace/untrusted": { "hasTrustDialogAccepted": false }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let roots = inspect_state(&path, &mut CollectedAccess::default());

        let _ = std::fs::remove_file(path);
        assert_eq!(roots, [PathBuf::from("/workspace/trusted")]);
    }

    #[test]
    fn config_directory_defaults_within_the_assessed_home() {
        let home = Path::new("/not-the-current-user");

        assert_eq!(config_dir(home), home.join(".claude"));
    }

    #[test]
    fn records_dangerous_past_modes_as_history_evidence() {
        let value = json!({ "permissionMode": "bypassPermissions" });
        let mut runtime = RuntimeCollector::default();
        inspect_runtime(
            value.as_object().unwrap(),
            Some(Path::new("/workspace")),
            &mut runtime,
        );
        let (capabilities, limited) = runtime.finish();

        assert!(!limited);
        assert!(capabilities.iter().any(|capability| {
            capability.source.kind == AccessSourceKind::History
                && capability.decision == AccessDecision::Allow
        }));
    }
}
