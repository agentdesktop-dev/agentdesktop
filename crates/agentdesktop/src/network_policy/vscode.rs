use std::path::Path;

use agentdesktop_agent::access::{network_rule_id, normalize_network_resource};
use agentdesktop_core::model::AccessRuleMechanism;
use anyhow::{Context, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstRootNode},
};

use super::{NetworkRuleDecision, RuleMutation, read_text, write_text};

const URL_RULES: &str = "chat.tools.urls.autoApprove";

pub(super) fn apply(path: &Path, mutation: &RuleMutation) -> anyhow::Result<()> {
    let contents = read_text(path, "{}\n")?;
    let root = CstRootNode::parse(&contents, &jsonc_options())
        .map_err(|error| anyhow::anyhow!(error.to_string()))
        .with_context(|| format!("parse VS Code settings from {}", path.display()))?;
    let settings = root
        .object_value_or_create()
        .context("VS Code settings must contain a JSON object")?;
    let rules = settings
        .object_value_or_create(URL_RULES)
        .context("VS Code URL auto-approval settings must be an object")?;

    match mutation {
        RuleMutation::Add { resource, decision } => {
            ensure!(
                !rules.properties().iter().any(|property| {
                    property
                        .name()
                        .and_then(|name| name.decoded_value().ok())
                        .and_then(|name| normalize_network_resource(&name))
                        .as_deref()
                        == Some(resource)
                }),
                "A VS Code URL rule already covers this host"
            );
            rules.append(resource, CstInputValue::Bool(decision_value(*decision)?));
        }
        RuleMutation::SetDecision { rule_id, decision } => {
            let property = find_rule(&rules, path, rule_id)?;
            property.set_value(CstInputValue::Bool(decision_value(*decision)?));
        }
        RuleMutation::Remove { rule_id } => {
            find_rule(&rules, path, rule_id)?.remove();
        }
    }

    write_text(path, &root.to_string())
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

pub(super) fn decision_value(decision: NetworkRuleDecision) -> anyhow::Result<bool> {
    match decision {
        NetworkRuleDecision::Allow => Ok(true),
        NetworkRuleDecision::Ask => Ok(false),
        NetworkRuleDecision::Deny => anyhow::bail!("VS Code URL rules support Allow or Ask"),
    }
}

fn find_rule(
    rules: &jsonc_parser::cst::CstObject,
    path: &Path,
    rule_id: &str,
) -> anyhow::Result<jsonc_parser::cst::CstObjectProp> {
    let mechanism = AccessRuleMechanism::VscodeUrlAutoApprove;
    let mut matches = rules.properties().into_iter().filter(|property| {
        property
            .value()
            .and_then(|value| value.as_boolean_lit())
            .is_some()
            && property
                .name()
                .and_then(|name| name.decoded_value().ok())
                .is_some_and(|name| network_rule_id(&mechanism, path, &name) == rule_id)
    });
    let property = matches
        .next()
        .context("VS Code URL rule changed since the access audit")?;
    ensure!(
        matches.next().is_none(),
        "VS Code URL rule identity is ambiguous"
    );
    Ok(property)
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

    use super::*;

    static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-network-policy-vscode-{name}-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn edits_jsonc_without_losing_comments() {
        let directory = temporary_directory("jsonc");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{
  // Keep this comment.
  "editor.fontSize": 14,
  "chat.tools.urls.autoApprove": {
    "*.example.com": true,
    "api.example.com": false
  }
}
"#,
        )
        .unwrap();
        let id = network_rule_id(
            &AccessRuleMechanism::VscodeUrlAutoApprove,
            &path,
            "*.example.com",
        );

        apply(
            &path,
            &RuleMutation::SetDecision {
                rule_id: id.clone(),
                decision: NetworkRuleDecision::Ask,
            },
        )
        .unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("// Keep this comment."));
        assert!(updated.contains(r#""*.example.com": false"#));
        assert!(updated.contains(r#""editor.fontSize": 14"#));

        apply(&path, &RuleMutation::Remove { rule_id: id }).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(!updated.contains("*.example.com"));
        assert!(updated.contains("api.example.com"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn edits_origin_url_rule_without_rewriting_its_key() {
        let directory = temporary_directory("origin-url");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{
  "chat.tools.urls.autoApprove": {
    "https://api.example.com": true
  }
}
"#,
        )
        .unwrap();
        let id = network_rule_id(
            &AccessRuleMechanism::VscodeUrlAutoApprove,
            &path,
            "https://api.example.com",
        );

        apply(
            &path,
            &RuleMutation::SetDecision {
                rule_id: id,
                decision: NetworkRuleDecision::Ask,
            },
        )
        .unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains(r#""https://api.example.com": false"#));
        assert!(!updated.contains(r#""api.example.com": false"#));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn refuses_to_rewrite_json5_only_settings() {
        let directory = temporary_directory("json5");
        let path = directory.join("settings.json");
        fs::create_dir_all(&directory).unwrap();
        let original = "{ 'editor.fontSize': 14 }\n";
        fs::write(&path, original).unwrap();

        let error = apply(
            &path,
            &RuleMutation::Add {
                resource: "api.example.com".to_owned(),
                decision: NetworkRuleDecision::Ask,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("parse VS Code settings"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn rejects_deny_decision() {
        assert!(decision_value(NetworkRuleDecision::Deny).is_err());
    }
}
