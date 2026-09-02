use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use agentdesktop_agent::{access::normalize_network_resource, secure_fs};
use agentdesktop_core::{
    config::DaemonConfig,
    model::{AccessCategory, AccessReport, AccessRuleMechanism, AccessSourceKind},
};
use anyhow::{Context, ensure};
use serde::Deserialize;

mod claude;
mod vscode;

#[cfg(test)]
use agentdesktop_agent::access::network_rule_id;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NetworkRuleDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NetworkRuleOperation {
    Add {
        resource: String,
        decision: NetworkRuleDecision,
    },
    SetDecision {
        rule_id: String,
        decision: NetworkRuleDecision,
    },
    Remove {
        rule_id: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRuleChangeRequest {
    pub agent_kind: String,
    #[serde(flatten)]
    pub operation: NetworkRuleOperation,
}

pub fn apply(
    config: &DaemonConfig,
    report: &AccessReport,
    user_home: &Path,
    request: &NetworkRuleChangeRequest,
) -> anyhow::Result<()> {
    ensure!(
        config.controller.is_none(),
        "Network rules cannot change organization-managed settings"
    );
    ensure!(
        report
            .agents
            .iter()
            .any(|agent| agent.kind == request.agent_kind),
        "The selected agent is no longer installed"
    );
    if request.agent_kind == "claude-code" {
        ensure!(
            !claude::daemon_controls_network(config),
            "Claude network rules are controlled by the local Agentdesktop daemon configuration"
        );
    }

    match &request.operation {
        NetworkRuleOperation::Add { resource, decision } => {
            let resource = validate_resource(resource)?;
            let path = user_settings_path(&request.agent_kind, user_home)?;
            ensure_add_path(&path, user_home)?;
            match request.agent_kind.as_str() {
                "vscode" => vscode::apply(
                    &path,
                    &RuleMutation::Add {
                        resource,
                        decision: *decision,
                    },
                ),
                "claude-code" => claude::apply(
                    &path,
                    &RuleMutation::Add {
                        resource,
                        decision: *decision,
                    },
                ),
                _ => anyhow::bail!("Network rule editing is not supported for this agent"),
            }
        }
        NetworkRuleOperation::SetDecision { rule_id, decision } => {
            let rule = resolve_rule(report, &request.agent_kind, rule_id)?;
            ensure_existing_path(&rule.path, user_home)?;
            apply_existing_rule(
                &rule,
                &RuleMutation::SetDecision {
                    rule_id: rule_id.clone(),
                    decision: *decision,
                },
            )
        }
        NetworkRuleOperation::Remove { rule_id } => {
            let rule = resolve_rule(report, &request.agent_kind, rule_id)?;
            ensure_existing_path(&rule.path, user_home)?;
            apply_existing_rule(
                &rule,
                &RuleMutation::Remove {
                    rule_id: rule_id.clone(),
                },
            )
        }
    }
}

#[derive(Clone, Debug)]
enum RuleMutation {
    Add {
        resource: String,
        decision: NetworkRuleDecision,
    },
    SetDecision {
        rule_id: String,
        decision: NetworkRuleDecision,
    },
    Remove {
        rule_id: String,
    },
}

#[derive(Clone, Debug)]
struct ResolvedRule {
    mechanism: AccessRuleMechanism,
    path: PathBuf,
}

fn resolve_rule(
    report: &AccessReport,
    agent_kind: &str,
    rule_id: &str,
) -> anyhow::Result<ResolvedRule> {
    let mut matches = report
        .agents
        .iter()
        .filter(|agent| agent.kind == agent_kind)
        .flat_map(|agent| &agent.capabilities)
        .filter(|capability| capability.category == AccessCategory::Network)
        .filter_map(|capability| capability.rule.as_ref().map(|rule| (capability, rule)))
        .filter(|(_, rule)| rule.id == rule_id);
    let Some((capability, rule)) = matches.next() else {
        anyhow::bail!("This network rule no longer exists or is not editable");
    };
    ensure!(
        matches.next().is_none(),
        "Network rule identity is ambiguous"
    );
    ensure!(
        capability.source.kind == AccessSourceKind::Configuration,
        "Only configured network rules can be edited"
    );
    let path = capability
        .source
        .path
        .clone()
        .context("Editable network rule has no configuration source")?;
    Ok(ResolvedRule {
        mechanism: rule.mechanism.clone(),
        path,
    })
}

fn apply_existing_rule(rule: &ResolvedRule, mutation: &RuleMutation) -> anyhow::Result<()> {
    match rule.mechanism {
        AccessRuleMechanism::VscodeUrlAutoApprove => vscode::apply(&rule.path, mutation),
        AccessRuleMechanism::ClaudePermission | AccessRuleMechanism::ClaudeSandboxDomain => {
            claude::apply(&rule.path, mutation)
        }
    }
}

fn validate_resource(resource: &str) -> anyhow::Result<String> {
    let value = resource.trim().to_ascii_lowercase();
    ensure!(!value.is_empty(), "Enter a host or wildcard domain");
    ensure!(value.len() <= 255, "Network rule is too long");
    let normalized = normalize_network_resource(&value)
        .context("Enter a host such as api.example.com or *.example.com")?;
    ensure!(
        normalized == value,
        "Enter only a host or leading wildcard, without a URL path, port, or credentials"
    );
    if value == "*" {
        return Ok(value);
    }
    let host = value.strip_prefix("*.").unwrap_or(&value);
    ensure!(!host.is_empty(), "Wildcard rule must include a domain");
    for label in host.split('.') {
        ensure!(
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
                && !label.starts_with('-')
                && !label.ends_with('-'),
            "Network rule contains an invalid host label"
        );
    }
    Ok(value)
}

fn ensure_existing_path(path: &Path, user_home: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect network settings at {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "Network settings must be a regular file"
    );
    let canonical_home = user_home
        .canonicalize()
        .with_context(|| format!("resolve user home {}", user_home.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("resolve network settings {}", path.display()))?;
    ensure!(
        canonical_path.starts_with(canonical_home),
        "Only settings within the current user home can be edited"
    );
    Ok(())
}

fn ensure_add_path(path: &Path, user_home: &Path) -> anyhow::Result<()> {
    ensure!(
        path.starts_with(user_home),
        "Network settings path is outside the current user home"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "Network settings must be a regular file"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect network settings at {}", path.display()));
        }
    }
    let canonical_home = user_home
        .canonicalize()
        .with_context(|| format!("resolve user home {}", user_home.display()))?;
    let mut ancestor = path
        .parent()
        .context("Network settings path has no parent")?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("Network settings path has no existing parent")?;
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .with_context(|| format!("resolve network settings parent {}", ancestor.display()))?;
    ensure!(
        canonical_ancestor.starts_with(canonical_home),
        "Network settings directory resolves outside the current user home"
    );
    Ok(())
}

fn user_settings_path(agent_kind: &str, user_home: &Path) -> anyhow::Result<PathBuf> {
    match agent_kind {
        "claude-code" => {
            let directory = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        user_home.join(path)
                    }
                })
                .unwrap_or_else(|| user_home.join(".claude"));
            Ok(directory.join("settings.json"))
        }
        "vscode" => {
            #[cfg(target_os = "macos")]
            let path = user_home.join("Library/Application Support/Code/User/settings.json");
            #[cfg(target_os = "linux")]
            let path = user_home.join(".config/Code/User/settings.json");
            #[cfg(windows)]
            let path = user_home.join("AppData/Roaming/Code/User/settings.json");
            Ok(path)
        }
        _ => anyhow::bail!("Network rule editing is not supported for this agent"),
    }
}

pub(super) fn read_text(path: &Path, default: &str) -> anyhow::Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(default.to_owned()),
        Err(error) => {
            Err(error).with_context(|| format!("read network settings from {}", path.display()))
        }
    }
}

pub(super) fn write_text(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create network settings directory {}", parent.display()))?;
    }
    secure_fs::atomic_write(path, contents.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use agentdesktop_core::model::{
        AccessCapability, AccessDecision, AccessEnforcement, AccessOperation, AccessRuleRef,
        AccessSource, AgentAccessReport,
    };

    use super::*;

    static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-network-policy-{name}-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn deserializes_network_rule_change_contract() {
        let set: NetworkRuleChangeRequest = serde_json::from_value(serde_json::json!({
            "agentKind": "vscode",
            "operation": "setDecision",
            "ruleId": "rule-1",
            "decision": "ask"
        }))
        .unwrap();
        assert!(matches!(
            set.operation,
            NetworkRuleOperation::SetDecision {
                rule_id,
                decision: NetworkRuleDecision::Ask
            } if rule_id == "rule-1"
        ));

        let add: NetworkRuleChangeRequest = serde_json::from_value(serde_json::json!({
            "agentKind": "claude-code",
            "operation": "add",
            "resource": "api.example.com",
            "decision": "deny"
        }))
        .unwrap();
        assert!(matches!(
            add.operation,
            NetworkRuleOperation::Add {
                resource,
                decision: NetworkRuleDecision::Deny
            } if resource == "api.example.com"
        ));
    }

    #[test]
    fn applies_add_request_to_vscode_user_settings() {
        let home = temporary_directory("vscode-add");
        fs::create_dir_all(&home).unwrap();
        let config = agentdesktop_core::config::parse_daemon("programs: {}\n").unwrap();
        let report = AccessReport {
            generated_at_unix_ms: 0,
            status: agentdesktop_core::model::AccessReportStatus::Ready,
            detail: None,
            agents: vec![AgentAccessReport {
                kind: "vscode".to_owned(),
                executable: PathBuf::from("code"),
                version: None,
                user_home: home.clone(),
                capabilities: Vec::new(),
                observations: Vec::new(),
                findings: Vec::new(),
                coverage: Vec::new(),
            }],
        };
        let request = NetworkRuleChangeRequest {
            agent_kind: "vscode".to_owned(),
            operation: NetworkRuleOperation::Add {
                resource: "api.example.com".to_owned(),
                decision: NetworkRuleDecision::Allow,
            },
        };

        apply(&config, &report, &home, &request).unwrap();

        let path = user_settings_path("vscode", &home).unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains(r#""api.example.com": true"#));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_network_changes_for_controller_managed_devices() {
        let home = temporary_directory("managed");
        fs::create_dir_all(&home).unwrap();
        let config = agentdesktop_core::config::parse_daemon(
            r#"
controller:
  address: https://controller.example.com
"#,
        )
        .unwrap();
        let report = AccessReport {
            generated_at_unix_ms: 0,
            status: agentdesktop_core::model::AccessReportStatus::Ready,
            detail: None,
            agents: vec![AgentAccessReport {
                kind: "vscode".to_owned(),
                executable: PathBuf::from("code"),
                version: None,
                user_home: home.clone(),
                capabilities: Vec::new(),
                observations: Vec::new(),
                findings: Vec::new(),
                coverage: Vec::new(),
            }],
        };
        let request = NetworkRuleChangeRequest {
            agent_kind: "vscode".to_owned(),
            operation: NetworkRuleOperation::Add {
                resource: "api.example.com".to_owned(),
                decision: NetworkRuleDecision::Ask,
            },
        };

        let error = apply(&config, &report, &home, &request).unwrap_err();

        assert!(error.to_string().contains("organization-managed"));
        assert!(!user_settings_path("vscode", &home).unwrap().exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_claude_changes_when_the_local_daemon_controls_network_policy() {
        let home = temporary_directory("daemon-managed-claude");
        fs::create_dir_all(&home).unwrap();
        let config = agentdesktop_core::config::parse_daemon(
            r#"
programs:
  claudeCode:
    permissions:
      allow: ["WebFetch(domain:docs.example.com)"]
"#,
        )
        .unwrap();
        let report = AccessReport {
            generated_at_unix_ms: 0,
            status: agentdesktop_core::model::AccessReportStatus::Ready,
            detail: None,
            agents: vec![AgentAccessReport {
                kind: "claude-code".to_owned(),
                executable: PathBuf::from("claude"),
                version: None,
                user_home: home.clone(),
                capabilities: Vec::new(),
                observations: Vec::new(),
                findings: Vec::new(),
                coverage: Vec::new(),
            }],
        };
        let request = NetworkRuleChangeRequest {
            agent_kind: "claude-code".to_owned(),
            operation: NetworkRuleOperation::Add {
                resource: "api.example.com".to_owned(),
                decision: NetworkRuleDecision::Ask,
            },
        };

        let error = apply(&config, &report, &home, &request).unwrap_err();

        assert!(error.to_string().contains("daemon configuration"));
        assert!(!user_settings_path("claude-code", &home).unwrap().exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_urls_and_accepts_hosts_and_leading_wildcards() {
        assert_eq!(
            validate_resource("API.Example.com").unwrap(),
            "api.example.com"
        );
        assert_eq!(validate_resource("*.example.com").unwrap(), "*.example.com");
        assert!(validate_resource("https://example.com/path").is_err());
        assert!(validate_resource("example.com:443").is_err());
        assert!(validate_resource("foo.*.example.com").is_err());
    }

    #[test]
    fn resolves_only_fresh_editable_rule_handles() {
        let path = PathBuf::from("/Users/developer/settings.json");
        let mechanism = AccessRuleMechanism::VscodeUrlAutoApprove;
        let id = network_rule_id(&mechanism, &path, "*.example.com");
        let report = AccessReport {
            generated_at_unix_ms: 0,
            status: agentdesktop_core::model::AccessReportStatus::Ready,
            detail: None,
            agents: vec![AgentAccessReport {
                kind: "vscode".to_owned(),
                executable: PathBuf::from("/Applications/Visual Studio Code.app"),
                version: None,
                user_home: PathBuf::from("/Users/developer"),
                capabilities: vec![AccessCapability {
                    category: AccessCategory::Network,
                    resource: "*.example.com".to_owned(),
                    operations: vec![AccessOperation::Connect],
                    decision: AccessDecision::Allow,
                    enforcement: AccessEnforcement::Harness,
                    workspace: None,
                    source: AccessSource {
                        kind: AccessSourceKind::Configuration,
                        path: Some(path.clone()),
                    },
                    rule: Some(AccessRuleRef {
                        id: id.clone(),
                        mechanism: mechanism.clone(),
                    }),
                    detail: None,
                }],
                observations: Vec::new(),
                findings: Vec::new(),
                coverage: Vec::new(),
            }],
        };

        let resolved = resolve_rule(&report, "vscode", &id).unwrap();
        assert_eq!(resolved.path, path);
        assert_eq!(resolved.mechanism, mechanism);
        assert!(resolve_rule(&report, "vscode", "stale").is_err());
    }
}
