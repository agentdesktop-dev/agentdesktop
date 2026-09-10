use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision, AccessEnforcement,
    AccessOperation, AccessSourceKind,
};
use serde_json::Value;

use super::{
    CollectedAccess,
    configuration::{capability, default_capability},
    host_from_url, host_pattern, plural_suffix, safe_command_identifier,
};

pub(super) fn inspect_configuration(home: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let mut inspected = 0;
    let mut permission_coverage = PermissionCoverage::default();
    for path in config_paths(home) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = json5::from_str::<Value>(&contents) else {
            continue;
        };
        inspected += 1;
        permission_coverage.extend(parse_permissions(&document, &path, &mut collected));
        parse_mcp(&document, &path, &mut collected);
    }
    add_defaults(permission_coverage, &mut collected);
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Configuration,
        status: if inspected > 0 {
            AccessCoverageStatus::Partial
        } else {
            AccessCoverageStatus::Unavailable
        },
        detail: if inspected > 0 {
            format!(
                "Inspected {inspected} OpenCode configuration file{}; custom and agent-specific configuration may add access",
                plural_suffix(inspected),
            )
        } else {
            "No readable OpenCode configuration files were found; applied documented defaults"
                .to_owned()
        },
    });
    collected
}

#[derive(Default)]
struct PermissionCoverage {
    global: bool,
    read: bool,
    glob: bool,
    grep: bool,
    edit: bool,
    execution: bool,
    webfetch: bool,
    websearch: bool,
    external_directory: bool,
}

impl PermissionCoverage {
    fn extend(&mut self, other: Self) {
        self.global |= other.global;
        self.read |= other.read;
        self.glob |= other.glob;
        self.grep |= other.grep;
        self.edit |= other.edit;
        self.execution |= other.execution;
        self.webfetch |= other.webfetch;
        self.websearch |= other.websearch;
        self.external_directory |= other.external_directory;
    }
}

#[cfg(test)]
fn inspect_document(document: &Value, path: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let coverage = parse_permissions(document, path, &mut collected);
    add_defaults(coverage, &mut collected);
    collected
}

fn add_defaults(coverage: PermissionCoverage, collected: &mut CollectedAccess) {
    if coverage.global {
        return;
    }
    if !coverage.read || !coverage.glob || !coverage.grep {
        collected.capabilities.push(default_capability(
            AccessCategory::Filesystem,
            "workspace",
            vec![AccessOperation::Read],
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            "Unconfigured OpenCode read and search tools retain their default workspace access",
        ));
    }
    if !coverage.edit {
        collected.capabilities.push(default_capability(
            AccessCategory::Filesystem,
            "workspace",
            vec![AccessOperation::Write],
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            "OpenCode permits workspace writes by default",
        ));
    }
    if !coverage.execution {
        collected.capabilities.push(default_capability(
            AccessCategory::Execution,
            "*",
            vec![AccessOperation::Execute],
            AccessDecision::Allow,
            AccessEnforcement::None,
            "OpenCode permits shell operations by default",
        ));
    }
    let mut web_operations = Vec::new();
    if !coverage.webfetch {
        web_operations.push(AccessOperation::Connect);
    }
    if !coverage.websearch {
        web_operations.push(AccessOperation::Use);
    }
    if !web_operations.is_empty() {
        collected.capabilities.push(default_capability(
            AccessCategory::Network,
            "web tools",
            web_operations,
            AccessDecision::Allow,
            AccessEnforcement::Harness,
            "OpenCode permits web tools by default",
        ));
    }
    if !coverage.external_directory {
        collected.capabilities.push(default_capability(
            AccessCategory::Filesystem,
            "outside workspace",
            vec![AccessOperation::Read, AccessOperation::Write],
            AccessDecision::Ask,
            AccessEnforcement::Harness,
            "OpenCode asks before external directory access by default",
        ));
    }
}

fn add_fallback_permissions(
    decision: AccessDecision,
    path: &Path,
    coverage: &PermissionCoverage,
    collected: &mut CollectedAccess,
) {
    if !coverage.read || !coverage.glob || !coverage.grep {
        let missing = [
            (!coverage.read).then_some("read"),
            (!coverage.glob).then_some("glob"),
            (!coverage.grep).then_some("grep"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        collected.capabilities.push(capability(
            AccessCategory::Filesystem,
            &format!("workspace ({missing})"),
            vec![AccessOperation::Read],
            decision.clone(),
            AccessEnforcement::Harness,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
    if !coverage.edit {
        collected.capabilities.push(capability(
            AccessCategory::Filesystem,
            "workspace",
            vec![AccessOperation::Write],
            decision.clone(),
            AccessEnforcement::Harness,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
    if !coverage.external_directory {
        collected.capabilities.push(capability(
            AccessCategory::Filesystem,
            "outside workspace",
            vec![AccessOperation::Read, AccessOperation::Write],
            decision.clone(),
            AccessEnforcement::Harness,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
    if !coverage.execution {
        collected.capabilities.push(capability(
            AccessCategory::Execution,
            "*",
            vec![AccessOperation::Execute],
            decision.clone(),
            AccessEnforcement::None,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
    if !coverage.webfetch {
        collected.capabilities.push(capability(
            AccessCategory::Network,
            "web fetch",
            vec![AccessOperation::Connect],
            decision.clone(),
            AccessEnforcement::Harness,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
    if !coverage.websearch {
        collected.capabilities.push(capability(
            AccessCategory::Network,
            "hosted web search",
            vec![AccessOperation::Use],
            decision,
            AccessEnforcement::Harness,
            path,
            None,
            "OpenCode wildcard fallback permission",
        ));
    }
}

fn parse_permissions(
    document: &Value,
    path: &Path,
    collected: &mut CollectedAccess,
) -> PermissionCoverage {
    let Some(permission) = document.get("permission") else {
        return PermissionCoverage::default();
    };
    if let Some(decision) = permission.as_str().and_then(access_decision) {
        let coverage = PermissionCoverage {
            global: true,
            ..PermissionCoverage::default()
        };
        add_fallback_permissions(decision, path, &coverage, collected);
        return coverage;
    }
    let Some(permission) = permission.as_object() else {
        return PermissionCoverage::default();
    };
    let mut coverage = PermissionCoverage::default();
    let fallback = permission
        .get("*")
        .and_then(Value::as_str)
        .and_then(access_decision);
    for (name, value) in permission {
        let (category, operations) = match name.as_str() {
            "*" => continue,
            "bash" => {
                coverage.execution = true;
                (AccessCategory::Execution, vec![AccessOperation::Execute])
            }
            "read" => {
                coverage.read = true;
                (AccessCategory::Filesystem, vec![AccessOperation::Read])
            }
            "glob" => {
                coverage.glob = true;
                (AccessCategory::Filesystem, vec![AccessOperation::Read])
            }
            "grep" => {
                coverage.grep = true;
                (AccessCategory::Filesystem, vec![AccessOperation::Read])
            }
            "edit" => {
                coverage.edit = true;
                (AccessCategory::Filesystem, vec![AccessOperation::Write])
            }
            "external_directory" => (
                {
                    coverage.external_directory = true;
                    AccessCategory::Filesystem
                },
                vec![AccessOperation::Read, AccessOperation::Write],
            ),
            "webfetch" => {
                coverage.webfetch = true;
                (AccessCategory::Network, vec![AccessOperation::Connect])
            }
            "websearch" => {
                coverage.websearch = true;
                (AccessCategory::Network, vec![AccessOperation::Use])
            }
            _ => continue,
        };
        if let Some(decision) = value.as_str().and_then(access_decision) {
            collected.capabilities.push(capability(
                category,
                "*",
                operations,
                decision,
                AccessEnforcement::Harness,
                path,
                None,
                "OpenCode permission",
            ));
        } else if let Some(rules) = value.as_object() {
            for (resource, decision) in rules {
                let Some(decision) = decision.as_str().and_then(access_decision) else {
                    continue;
                };
                let resource = match category {
                    AccessCategory::Execution => safe_command_identifier(resource),
                    AccessCategory::Network if operations.contains(&AccessOperation::Connect) => {
                        host_pattern(resource).unwrap_or_else(|| "URL pattern".to_owned())
                    }
                    _ => resource.to_owned(),
                };
                collected.capabilities.push(capability(
                    category.clone(),
                    &resource,
                    operations.clone(),
                    decision,
                    AccessEnforcement::Harness,
                    path,
                    None,
                    "OpenCode permission rule",
                ));
            }
        }
    }
    if let Some(decision) = fallback {
        add_fallback_permissions(decision, path, &coverage, collected);
        coverage.global = true;
    }
    coverage
}

fn parse_mcp(document: &Value, path: &Path, collected: &mut CollectedAccess) {
    let Some(servers) = document.get("mcp").and_then(Value::as_object) else {
        return;
    };
    for (name, server) in servers {
        let enabled = server.get("enabled").and_then(Value::as_bool) != Some(false);
        collected.capabilities.push(capability(
            AccessCategory::ExternalService,
            &format!("mcp:{name}"),
            vec![AccessOperation::Use],
            if enabled {
                AccessDecision::Unknown
            } else {
                AccessDecision::Deny
            },
            AccessEnforcement::Harness,
            path,
            None,
            if enabled {
                "Configured OpenCode MCP server"
            } else {
                "Disabled OpenCode MCP server"
            },
        ));
        if !enabled {
            continue;
        }
        if let Some(url) = server.get("url").and_then(Value::as_str)
            && let Some(host) = host_from_url(url)
        {
            collected.capabilities.push(capability(
                AccessCategory::Network,
                &host,
                vec![AccessOperation::Connect],
                AccessDecision::Allow,
                AccessEnforcement::Harness,
                path,
                None,
                &format!("Endpoint for OpenCode MCP server {name}"),
            ));
        }
    }
}

fn access_decision(value: &str) -> Option<AccessDecision> {
    match value {
        "allow" => Some(AccessDecision::Allow),
        "ask" => Some(AccessDecision::Ask),
        "deny" => Some(AccessDecision::Deny),
        _ => None,
    }
}

fn config_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = BTreeSet::from([
        home.join(".config/opencode/opencode.json"),
        home.join(".config/opencode/opencode.jsonc"),
        crate::reconcile::default_open_code_managed_config_path(),
    ]);
    #[cfg(target_os = "macos")]
    paths.extend([
        PathBuf::from("/Library/Application Support/opencode/opencode.json"),
        PathBuf::from("/Library/Application Support/opencode/opencode.jsonc"),
    ]);
    #[cfg(target_os = "linux")]
    paths.extend([
        PathBuf::from("/etc/opencode/opencode.json"),
        PathBuf::from("/etc/opencode/opencode.jsonc"),
    ]);
    #[cfg(windows)]
    if let Some(program_data) = std::env::var_os("ProgramData") {
        paths.insert(PathBuf::from(program_data).join("opencode/opencode.json"));
    }
    if let Ok(current) = std::env::current_dir() {
        for ancestor in current.ancestors() {
            paths.insert(ancestor.join("opencode.json"));
            paths.insert(ancestor.join("opencode.jsonc"));
        }
    }
    paths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::model::{
        AccessCategory, AccessDecision, AccessOperation, AccessSourceKind,
    };
    use serde_json::json;

    use super::{inspect_document, parse_permissions};
    use crate::access::CollectedAccess;

    #[test]
    fn permission_patterns_omit_command_arguments_and_url_paths() {
        let mut collected = CollectedAccess::default();
        parse_permissions(
            &json!({
                "permission": {
                    "bash": {
                        "curl -H 'Authorization: secret' https://api.example.com/private": "allow"
                    },
                    "webfetch": {
                        "https://docs.example.com/private?token=secret": "allow"
                    }
                }
            }),
            Path::new("opencode.json"),
            &mut collected,
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Execution && capability.resource == "curl"
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Network
                && capability.resource == "docs.example.com"
        }));
        assert!(!format!("{:?}", collected.capabilities).contains("secret"));
    }

    #[test]
    fn global_permission_applies_to_all_access_categories() {
        let collected =
            inspect_document(&json!({ "permission": "deny" }), Path::new("opencode.json"));

        for category in [
            AccessCategory::Filesystem,
            AccessCategory::Execution,
            AccessCategory::Network,
        ] {
            assert!(collected.capabilities.iter().any(|capability| {
                capability.category == category && capability.decision == AccessDecision::Deny
            }));
        }
    }

    #[test]
    fn partial_permissions_keep_unconfigured_defaults() {
        let collected = inspect_document(
            &json!({ "permission": { "bash": "deny" } }),
            Path::new("opencode.json"),
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Execution
                && capability.decision == AccessDecision::Deny
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Filesystem
                && capability.resource == "workspace"
                && capability.decision == AccessDecision::Allow
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Network
                && capability.resource == "web tools"
                && capability.decision == AccessDecision::Allow
        }));
    }

    #[test]
    fn independent_tools_keep_their_own_defaults() {
        let collected = inspect_document(
            &json!({
                "permission": {
                    "read": "deny",
                    "webfetch": "deny"
                }
            }),
            Path::new("opencode.json"),
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Filesystem
                && capability.source.kind == AccessSourceKind::Default
                && capability.decision == AccessDecision::Allow
        }));
        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Network
                && capability.source.kind == AccessSourceKind::Default
                && capability.operations == [AccessOperation::Use]
                && capability.decision == AccessDecision::Allow
        }));
    }

    #[test]
    fn specific_permission_overrides_wildcard_fallback() {
        let collected = inspect_document(
            &json!({
                "permission": {
                    "*": "allow",
                    "bash": "deny"
                }
            }),
            Path::new("opencode.json"),
        );

        assert!(collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Execution
                && capability.decision == AccessDecision::Deny
        }));
        assert!(!collected.capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Execution
                && capability.decision == AccessDecision::Allow
        }));
    }
}
