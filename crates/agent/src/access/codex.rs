use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision, AccessEnforcement,
    AccessOperation, AccessSourceKind,
};

use super::{
    CollectedAccess,
    configuration::{capability, default_capability},
    history_scan::{HistoryAdapter, RuntimeCollector, runtime_capability},
    host_from_url, plural_suffix,
};

pub(super) fn history_adapter(home: &Path) -> HistoryAdapter {
    HistoryAdapter {
        kind: "codex",
        root: home_dir(home).join("sessions"),
        include_file: None,
        workspace_for_file: None,
        inspect_runtime,
        coverage_limitation: None,
    }
}

fn inspect_runtime(
    object: &serde_json::Map<String, serde_json::Value>,
    workspace: Option<&Path>,
    collected: &mut RuntimeCollector,
) {
    if !object.contains_key("sandbox_policy") {
        return;
    }
    if let Some(policy) = object
        .get("approval_policy")
        .and_then(serde_json::Value::as_str)
    {
        collected.record(runtime_capability(
            AccessCategory::Execution,
            "shell commands",
            vec![AccessOperation::Execute],
            match policy {
                "on-request" | "untrusted" => AccessDecision::Ask,
                "never" => AccessDecision::Allow,
                _ => AccessDecision::Unknown,
            },
            AccessEnforcement::Harness,
            workspace,
            &format!("Recorded Codex approval policy: {policy}"),
        ));
    }
    if let Some(sandbox) = object
        .get("sandbox_policy")
        .and_then(serde_json::Value::as_object)
        && let Some(mode) = sandbox.get("mode").and_then(serde_json::Value::as_str)
    {
        match mode {
            "read-only" => collected.extend([
                runtime_capability(
                    AccessCategory::Filesystem,
                    "workspace",
                    vec![AccessOperation::Read],
                    AccessDecision::Allow,
                    AccessEnforcement::Sandbox,
                    workspace,
                    "Recorded Codex read-only sandbox",
                ),
                runtime_capability(
                    AccessCategory::Filesystem,
                    "workspace",
                    vec![AccessOperation::Write],
                    AccessDecision::Deny,
                    AccessEnforcement::Sandbox,
                    workspace,
                    "Recorded Codex read-only sandbox",
                ),
            ]),
            "workspace-write" => collected.record(runtime_capability(
                AccessCategory::Filesystem,
                "workspace",
                vec![AccessOperation::Read, AccessOperation::Write],
                AccessDecision::Allow,
                AccessEnforcement::Sandbox,
                workspace,
                "Recorded Codex workspace-write sandbox",
            )),
            "danger-full-access" => collected.extend([
                runtime_capability(
                    AccessCategory::Filesystem,
                    "*",
                    vec![AccessOperation::Read, AccessOperation::Write],
                    AccessDecision::Allow,
                    AccessEnforcement::None,
                    workspace,
                    "Recorded Codex danger-full-access sandbox mode",
                ),
                runtime_capability(
                    AccessCategory::Execution,
                    "*",
                    vec![AccessOperation::Execute],
                    AccessDecision::Allow,
                    AccessEnforcement::None,
                    workspace,
                    "Recorded Codex danger-full-access sandbox mode",
                ),
            ]),
            _ => {}
        }
        if let Some(network) = sandbox
            .get("network_access")
            .and_then(serde_json::Value::as_bool)
        {
            collected.record(runtime_capability(
                AccessCategory::Network,
                "*",
                vec![AccessOperation::Connect],
                if network {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                },
                AccessEnforcement::Sandbox,
                workspace,
                "Recorded Codex command network policy",
            ));
        }
    }
}

pub(super) fn inspect_configuration(home: &Path) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    let mut settings = BTreeSet::from([
        (PathBuf::from("/etc/codex/config.toml"), None),
        (PathBuf::from("/etc/codex/managed_config.toml"), None),
        (home_dir(home).join("config.toml"), None),
    ]);
    let mut inspected = 0;
    let mut cursor = 0;
    while let Some((path, workspace)) = settings.iter().nth(cursor).cloned() {
        cursor += 1;
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = toml::from_str::<toml::Value>(&contents) else {
            continue;
        };
        inspected += 1;
        parse_settings(&document, &path, workspace.as_deref(), &mut collected);
        if workspace.is_none()
            && let Some(projects) = document.get("projects").and_then(toml::Value::as_table)
        {
            for (project, value) in projects {
                if value.get("trust_level").and_then(toml::Value::as_str) == Some("trusted") {
                    let workspace = PathBuf::from(project);
                    settings.insert((workspace.join(".codex/config.toml"), Some(workspace)));
                }
            }
        }
    }
    if !collected.capabilities.iter().any(|capability| {
        capability.category == AccessCategory::Execution
            || capability.category == AccessCategory::Filesystem
    }) {
        collected.capabilities.push(default_capability(
            AccessCategory::Execution,
            "shell commands",
            vec![AccessOperation::Execute],
            AccessDecision::Unknown,
            AccessEnforcement::Unknown,
            "Effective Codex approval and sandbox modes are selected per session",
        ));
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
                "Inspected {inspected} Codex configuration file{}; named profiles and requirements are not assessed",
                plural_suffix(inspected),
            )
        } else {
            "No readable Codex configuration files were found".to_owned()
        },
    });
    collected
}

fn parse_settings(
    document: &toml::Value,
    path: &Path,
    workspace: Option<&Path>,
    collected: &mut CollectedAccess,
) {
    if let Some(policy) = document
        .get("approval_policy")
        .and_then(toml::Value::as_str)
    {
        let decision = match policy {
            "on-request" | "untrusted" => AccessDecision::Ask,
            _ => AccessDecision::Unknown,
        };
        collected.capabilities.push(capability(
            AccessCategory::Execution,
            "shell commands",
            vec![AccessOperation::Execute],
            decision,
            AccessEnforcement::Harness,
            path,
            workspace,
            &format!("Codex approval policy: {policy}"),
        ));
    }
    if let Some(mode) = document.get("sandbox_mode").and_then(toml::Value::as_str) {
        match mode {
            "read-only" => collected.capabilities.extend([
                capability(
                    AccessCategory::Filesystem,
                    "workspace",
                    vec![AccessOperation::Read],
                    AccessDecision::Allow,
                    AccessEnforcement::Sandbox,
                    path,
                    workspace,
                    "Codex read-only sandbox",
                ),
                capability(
                    AccessCategory::Filesystem,
                    "workspace",
                    vec![AccessOperation::Write],
                    AccessDecision::Deny,
                    AccessEnforcement::Sandbox,
                    path,
                    workspace,
                    "Codex read-only sandbox",
                ),
            ]),
            "workspace-write" => collected.capabilities.push(capability(
                AccessCategory::Filesystem,
                "workspace",
                vec![AccessOperation::Read, AccessOperation::Write],
                AccessDecision::Allow,
                AccessEnforcement::Sandbox,
                path,
                workspace,
                "Codex workspace-write sandbox",
            )),
            "danger-full-access" => collected.capabilities.extend([
                capability(
                    AccessCategory::Filesystem,
                    "*",
                    vec![AccessOperation::Read, AccessOperation::Write],
                    AccessDecision::Allow,
                    AccessEnforcement::None,
                    path,
                    workspace,
                    "Codex danger-full-access mode",
                ),
                capability(
                    AccessCategory::Execution,
                    "*",
                    vec![AccessOperation::Execute],
                    AccessDecision::Allow,
                    AccessEnforcement::None,
                    path,
                    workspace,
                    "Codex danger-full-access mode",
                ),
            ]),
            _ => {}
        }
    }
    if let Some(sandbox) = document
        .get("sandbox_workspace_write")
        .and_then(toml::Value::as_table)
    {
        if let Some(allowed) = sandbox.get("network_access").and_then(toml::Value::as_bool) {
            collected.capabilities.push(capability(
                AccessCategory::Network,
                "*",
                vec![AccessOperation::Connect],
                if allowed {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                },
                AccessEnforcement::Sandbox,
                path,
                workspace,
                "Codex workspace-write network policy",
            ));
        }
        if let Some(roots) = sandbox
            .get("writable_roots")
            .and_then(toml::Value::as_array)
        {
            for root in roots.iter().filter_map(toml::Value::as_str) {
                collected.capabilities.push(capability(
                    AccessCategory::Filesystem,
                    root,
                    vec![AccessOperation::Write],
                    AccessDecision::Allow,
                    AccessEnforcement::Sandbox,
                    path,
                    workspace,
                    "Additional Codex writable root",
                ));
            }
        }
    }
    if let Some(mode) = document.get("web_search").and_then(toml::Value::as_str) {
        collected.capabilities.push(capability(
            AccessCategory::Network,
            "hosted web search",
            vec![AccessOperation::Use],
            if mode == "disabled" {
                AccessDecision::Deny
            } else {
                AccessDecision::Allow
            },
            AccessEnforcement::Harness,
            path,
            workspace,
            &format!("Codex web search mode: {mode}"),
        ));
    }
    if let Some(providers) = document
        .get("model_providers")
        .and_then(toml::Value::as_table)
    {
        for (name, provider) in providers {
            let Some(base_url) = provider.get("base_url").and_then(toml::Value::as_str) else {
                continue;
            };
            let Some(host) = host_from_url(base_url) else {
                continue;
            };
            collected.capabilities.push(capability(
                AccessCategory::Network,
                &host,
                vec![AccessOperation::Connect],
                AccessDecision::Allow,
                AccessEnforcement::Harness,
                path,
                workspace,
                &format!("Codex model provider {name}"),
            ));
        }
    }
}

fn home_dir(home: &Path) -> PathBuf {
    if crate::discovery::metadata::home_dir().as_deref() == Some(home)
        && let Some(configured) = std::env::var_os("CODEX_HOME")
    {
        return PathBuf::from(configured);
    }
    home.join(".codex")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::model::{
        AccessCategory, AccessDecision, AccessEnforcement, AccessSeverity,
    };
    use serde_json::json;

    use super::{inspect_runtime, parse_settings};
    use crate::access::{CollectedAccess, history_scan::RuntimeCollector};

    #[test]
    fn flags_uncontained_full_access() {
        let mut collected = CollectedAccess::default();
        let document: toml::Value =
            toml::from_str("sandbox_mode = \"danger-full-access\"").unwrap();
        parse_settings(&document, Path::new("config.toml"), None, &mut collected);
        crate::access::derive_findings(&mut collected);

        assert!(collected.findings.iter().any(|finding| {
            finding.severity == AccessSeverity::Critical
                && finding.title == "Uncontained command execution"
        }));
    }

    #[test]
    fn reads_runtime_sandbox_policy() {
        let value = json!({
            "approval_policy": "on-request",
            "sandbox_policy": {
                "mode": "workspace-write",
                "network_access": false
            }
        });
        let mut runtime = RuntimeCollector::default();
        inspect_runtime(
            value.as_object().unwrap(),
            Some(Path::new("/workspace")),
            &mut runtime,
        );
        let (capabilities, limited) = runtime.finish();

        assert!(!limited);
        assert!(capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Filesystem
                && capability.enforcement == AccessEnforcement::Sandbox
        }));
        assert!(capabilities.iter().any(|capability| {
            capability.category == AccessCategory::Network
                && capability.decision == AccessDecision::Deny
        }));
    }
}
