use std::path::Path;

use agentdesktop_agent::access::{network_rule_id, normalize_network_resource};
use agentdesktop_core::{config::DaemonConfig, model::AccessRuleMechanism};
use anyhow::{Context, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstArray, CstInputValue, CstRootNode},
};
use serde_json::Value;

use super::{NetworkRuleDecision, RuleMutation, read_text, write_text};

pub(super) fn daemon_controls_network(config: &DaemonConfig) -> bool {
    let Some(claude) = config.programs.claude_code.as_ref() else {
        return false;
    };
    let settings = &claude.settings;
    let permission_rule = settings
        .get("permissions")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|permissions| {
            ["allow", "ask", "deny"]
                .into_iter()
                .filter_map(|key| permissions.get(key))
        })
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(Value::as_str)
        .any(|rule| rule.starts_with("WebFetch("));
    permission_rule
        || settings
            .get("sandbox")
            .and_then(|sandbox| sandbox.get("network"))
            .is_some()
}

pub(super) fn apply(path: &Path, mutation: &RuleMutation) -> anyhow::Result<()> {
    let contents = read_text(path, "{}\n")?;
    let settings: Value = serde_json::from_str(&contents)
        .with_context(|| format!("parse Claude settings from {}", path.display()))?;
    ensure!(
        settings.is_object(),
        "Claude settings must contain a JSON object"
    );
    let root = CstRootNode::parse(&contents, &ParseOptions::default())
        .map_err(|error| anyhow::anyhow!(error.to_string()))
        .with_context(|| format!("parse Claude settings tree from {}", path.display()))?;

    match mutation {
        RuleMutation::Add { resource, decision } => {
            let rule = format!("WebFetch(domain:{resource})");
            ensure!(
                find_permission_by_resource(&settings, resource).is_none(),
                "A Claude WebFetch rule already covers this host"
            );
            permission_array(&root, permission_list(*decision)?)?
                .append(CstInputValue::String(rule));
        }
        RuleMutation::SetDecision { rule_id, decision } => {
            let mechanism = find_mechanism(&settings, path, rule_id)?;
            match mechanism {
                AccessRuleMechanism::ClaudePermission => {
                    let target_list = permission_list(*decision)?;
                    let (source_list, _, _) = find_permission(&settings, path, rule_id)
                        .context("Claude permission rule changed since the access audit")?;
                    ensure!(
                        source_list != target_list,
                        "Claude permission rule already has this decision"
                    );
                    let (_, _, rule) = remove_permission(&root, &settings, path, rule_id)?;
                    if !permission_contains(&settings, target_list, &rule) {
                        permission_array(&root, target_list)?.append(CstInputValue::String(rule));
                    }
                }
                AccessRuleMechanism::ClaudeSandboxDomain => {
                    let target_list = sandbox_list(*decision)?;
                    let (source_list, _, _) = find_sandbox_domain(&settings, path, rule_id)
                        .context("Claude sandbox domain changed since the access audit")?;
                    ensure!(
                        source_list != target_list,
                        "Claude sandbox domain already has this decision"
                    );
                    let (_, _, resource) = remove_sandbox_domain(&root, &settings, path, rule_id)?;
                    if !sandbox_contains(&settings, target_list, &resource) {
                        sandbox_domain_array(&root, target_list)?
                            .append(CstInputValue::String(resource));
                    }
                }
                AccessRuleMechanism::VscodeUrlAutoApprove => unreachable!(),
            }
        }
        RuleMutation::Remove { rule_id } => match find_mechanism(&settings, path, rule_id)? {
            AccessRuleMechanism::ClaudePermission => {
                remove_permission(&root, &settings, path, rule_id)?;
            }
            AccessRuleMechanism::ClaudeSandboxDomain => {
                remove_sandbox_domain(&root, &settings, path, rule_id)?;
            }
            AccessRuleMechanism::VscodeUrlAutoApprove => unreachable!(),
        },
    }

    write_text(path, &root.to_string())
}

fn permission_list(decision: NetworkRuleDecision) -> anyhow::Result<&'static str> {
    match decision {
        NetworkRuleDecision::Allow => Ok("allow"),
        NetworkRuleDecision::Ask => Ok("ask"),
        NetworkRuleDecision::Deny => Ok("deny"),
    }
}

pub(super) fn sandbox_list(decision: NetworkRuleDecision) -> anyhow::Result<&'static str> {
    match decision {
        NetworkRuleDecision::Allow => Ok("allowedDomains"),
        NetworkRuleDecision::Deny => Ok("deniedDomains"),
        NetworkRuleDecision::Ask => anyhow::bail!("Claude sandbox domains support Allow or Deny"),
    }
}

fn permission_array(root: &CstRootNode, list: &str) -> anyhow::Result<CstArray> {
    root.object_value()
        .context("Claude settings must be an object")?
        .object_value_or_create("permissions")
        .context("Claude permissions must be an object")?
        .array_value_or_create(list)
        .context("Claude permission rules must be an array")
}

fn sandbox_domain_array(root: &CstRootNode, list: &str) -> anyhow::Result<CstArray> {
    root.object_value()
        .context("Claude settings must be an object")?
        .object_value_or_create("sandbox")
        .context("Claude sandbox must be an object")?
        .object_value_or_create("network")
        .context("Claude sandbox network settings must be an object")?
        .array_value_or_create(list)
        .context("Claude sandbox domain rules must be an array")
}

fn permission_contains(settings: &Value, list: &str, rule: &str) -> bool {
    settings
        .get("permissions")
        .and_then(|permissions| permissions.get(list))
        .and_then(Value::as_array)
        .is_some_and(|rules| rules.iter().any(|value| value.as_str() == Some(rule)))
}

fn sandbox_contains(settings: &Value, list: &str, resource: &str) -> bool {
    settings
        .get("sandbox")
        .and_then(|sandbox| sandbox.get("network"))
        .and_then(|network| network.get(list))
        .and_then(Value::as_array)
        .is_some_and(|rules| rules.iter().any(|value| value.as_str() == Some(resource)))
}

fn find_permission_by_resource(settings: &Value, resource: &str) -> Option<()> {
    let permissions = settings.get("permissions")?.as_object()?;
    for list in ["allow", "ask", "deny"] {
        let Some(rules) = permissions.get(list).and_then(Value::as_array) else {
            continue;
        };
        for rule in rules {
            let Some(rule) = rule.as_str() else {
                continue;
            };
            let Some(domain) = rule
                .strip_prefix("WebFetch(domain:")
                .and_then(|value| value.strip_suffix(')'))
            else {
                continue;
            };
            if normalize_network_resource(domain).as_deref() == Some(resource) {
                return Some(());
            }
        }
    }
    None
}

fn find_mechanism(
    settings: &Value,
    path: &Path,
    rule_id: &str,
) -> anyhow::Result<AccessRuleMechanism> {
    if find_permission(settings, path, rule_id).is_some() {
        return Ok(AccessRuleMechanism::ClaudePermission);
    }
    if find_sandbox_domain(settings, path, rule_id).is_some() {
        return Ok(AccessRuleMechanism::ClaudeSandboxDomain);
    }
    anyhow::bail!("Claude network rule changed since the access audit")
}

fn find_permission(
    settings: &Value,
    path: &Path,
    rule_id: &str,
) -> Option<(String, usize, String)> {
    let permissions = settings.get("permissions")?.as_object()?;
    let mechanism = AccessRuleMechanism::ClaudePermission;
    for list in ["allow", "ask", "deny"] {
        let Some(rules) = permissions.get(list).and_then(Value::as_array) else {
            continue;
        };
        for (index, value) in rules.iter().enumerate() {
            let Some(rule) = value.as_str() else {
                continue;
            };
            let identity = format!("permissions.{list}\0{rule}");
            if network_rule_id(&mechanism, path, &identity) == rule_id {
                return Some((list.to_owned(), index, rule.to_owned()));
            }
        }
    }
    None
}

fn remove_permission(
    root: &CstRootNode,
    settings: &Value,
    path: &Path,
    rule_id: &str,
) -> anyhow::Result<(String, usize, String)> {
    let found = find_permission(settings, path, rule_id)
        .context("Claude permission rule changed since the access audit")?;
    permission_array(root, &found.0)?
        .elements()
        .get(found.1)
        .cloned()
        .context("Claude permission rule changed since the access audit")?
        .remove();
    Ok(found)
}

fn find_sandbox_domain(
    settings: &Value,
    path: &Path,
    rule_id: &str,
) -> Option<(String, usize, String)> {
    let network = settings.get("sandbox")?.get("network")?.as_object()?;
    let mechanism = AccessRuleMechanism::ClaudeSandboxDomain;
    for list in ["allowedDomains", "deniedDomains"] {
        let Some(rules) = network.get(list).and_then(Value::as_array) else {
            continue;
        };
        for (index, value) in rules.iter().enumerate() {
            let Some(resource) = value.as_str() else {
                continue;
            };
            let identity = format!("sandbox.network.{list}\0{resource}");
            if network_rule_id(&mechanism, path, &identity) == rule_id {
                return Some((list.to_owned(), index, resource.to_owned()));
            }
        }
    }
    None
}

fn remove_sandbox_domain(
    root: &CstRootNode,
    settings: &Value,
    path: &Path,
    rule_id: &str,
) -> anyhow::Result<(String, usize, String)> {
    let found = find_sandbox_domain(settings, path, rule_id)
        .context("Claude sandbox domain changed since the access audit")?;
    sandbox_domain_array(root, &found.0)?
        .elements()
        .get(found.1)
        .cloned()
        .context("Claude sandbox domain changed since the access audit")?
        .remove();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use agentdesktop_agent::access::network_rule_id;
    use agentdesktop_core::model::AccessRuleMechanism;
    use serde_json::Value;

    use super::*;

    static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-network-policy-claude-{name}-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn moves_permission_rule_without_losing_other_rules() {
        let directory = temporary_directory("permission");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{
        "zeta": {"keep": true},
        "permissions": {
                "allow": ["WebFetch(domain:*.example.com)", "Bash(cargo test)"],
                "deny": ["Read(.env)"]
        },
        "alpha": 1
}
"#,
        )
        .unwrap();
        let native = "permissions.allow\0WebFetch(domain:*.example.com)";
        let id = network_rule_id(&AccessRuleMechanism::ClaudePermission, &path, native);

        apply(
            &path,
            &RuleMutation::SetDecision {
                rule_id: id,
                decision: NetworkRuleDecision::Ask,
            },
        )
        .unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        let settings: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["Bash(cargo test)"])
        );
        assert_eq!(
            settings["permissions"]["ask"],
            serde_json::json!(["WebFetch(domain:*.example.com)"])
        );
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(["Read(.env)"])
        );
        assert!(updated.contains("    \"zeta\": {\"keep\": true},"));
        assert!(updated.contains("    \"alpha\": 1"));
        assert!(updated.find("\"zeta\"") < updated.find("\"permissions\""));
        assert!(updated.find("\"permissions\"") < updated.find("\"alpha\""));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn moves_and_removes_sandbox_domain() {
        let directory = temporary_directory("sandbox");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{
  "sandbox": {
    "network": {
      "allowedDomains": ["*.example.com", "api.example.com"],
      "deniedDomains": ["blocked.example.com"]
    }
  }
}
"#,
        )
        .unwrap();
        let allowed_id = network_rule_id(
            &AccessRuleMechanism::ClaudeSandboxDomain,
            &path,
            "sandbox.network.allowedDomains\0*.example.com",
        );

        apply(
            &path,
            &RuleMutation::SetDecision {
                rule_id: allowed_id,
                decision: NetworkRuleDecision::Deny,
            },
        )
        .unwrap();
        let denied_id = network_rule_id(
            &AccessRuleMechanism::ClaudeSandboxDomain,
            &path,
            "sandbox.network.deniedDomains\0*.example.com",
        );
        apply(&path, &RuleMutation::Remove { rule_id: denied_id }).unwrap();

        let settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            settings["sandbox"]["network"]["allowedDomains"],
            serde_json::json!(["api.example.com"])
        );
        assert_eq!(
            settings["sandbox"]["network"]["deniedDomains"],
            serde_json::json!(["blocked.example.com"])
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn rejects_ask_for_sandbox_domain() {
        assert!(sandbox_list(NetworkRuleDecision::Ask).is_err());
    }

    #[test]
    fn rejects_noop_decision_without_removing_the_rule() {
        let directory = temporary_directory("noop");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        let original = r#"{
  "permissions": {
    "allow": ["WebFetch(domain:api.example.com)"]
  }
}
"#;
        fs::write(&path, original).unwrap();
        let id = network_rule_id(
            &AccessRuleMechanism::ClaudePermission,
            &path,
            "permissions.allow\0WebFetch(domain:api.example.com)",
        );

        let error = apply(
            &path,
            &RuleMutation::SetDecision {
                rule_id: id,
                decision: NetworkRuleDecision::Allow,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("already has this decision"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(directory);
    }
}
