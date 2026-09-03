mod claude_code;
mod claude_desktop;
mod codex;
mod configuration;
mod history_scan;
mod opencode;
mod vscode;

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{
    AccessCapability, AccessCategory, AccessCoverage, AccessCoverageStatus, AccessDecision,
    AccessEnforcement, AccessFinding, AccessOperation, AccessReport, AccessReportStatus,
    AccessRuleMechanism, AccessRuleRef, AccessSeverity, AccessSource, AccessSourceKind, Agent,
    AgentAccessReport, Discovery,
};
use sha2::{Digest, Sha256};

pub fn assess(discovery: &Discovery, user_home: &Path) -> AccessReport {
    let mut homes = crate::discovery::metadata::user_home_dirs();
    if !homes.iter().any(|home| home == user_home) {
        homes.push(user_home.to_path_buf());
    }
    let mut reports: Vec<_> = discovery
        .agents
        .iter()
        .map(|agent| assess_agent(agent, user_home, &homes))
        .collect();

    reports.sort_by(|left, right| {
        (
            agent_order(&left.kind),
            &left.kind,
            &left.user_home,
            &left.executable,
        )
            .cmp(&(
                agent_order(&right.kind),
                &right.kind,
                &right.user_home,
                &right.executable,
            ))
    });
    AccessReport {
        generated_at_unix_ms: unix_time_ms(),
        status: AccessReportStatus::Ready,
        detail: None,
        agents: reports,
    }
}

fn agent_order(kind: &str) -> u8 {
    match kind {
        "vscode" => 0,
        "claude-code" => 1,
        "claude-desktop" => 2,
        "codex" => 3,
        "opencode" => 4,
        _ => u8::MAX,
    }
}

pub fn unavailable(detail: impl Into<String>) -> AccessReport {
    AccessReport {
        generated_at_unix_ms: unix_time_ms(),
        status: AccessReportStatus::Unavailable,
        detail: Some(detail.into()),
        agents: Vec::new(),
    }
}

fn assess_agent(agent: &Agent, home: &Path, homes: &[PathBuf]) -> AgentAccessReport {
    let mut collected = configuration::inspect(&agent.kind, home);
    add_mcp_capabilities(agent, home, homes, &mut collected);
    collected.extend(history_scan::inspect(&agent.kind, home));
    derive_findings(&mut collected);

    collected.capabilities.sort();
    collected.capabilities.dedup();
    collected.observations.sort();
    collected.findings.sort_by(|left, right| {
        (
            &left.severity,
            &left.category,
            &left.workspace,
            &left.title,
            &left.detail,
        )
            .cmp(&(
                &right.severity,
                &right.category,
                &right.workspace,
                &right.title,
                &right.detail,
            ))
    });
    collected.findings.dedup();

    AgentAccessReport {
        kind: agent.kind.clone(),
        executable: agent.executable.clone(),
        version: agent.version.clone(),
        user_home: home.to_path_buf(),
        capabilities: collected.capabilities,
        observations: collected.observations,
        findings: collected.findings,
        coverage: collected.coverage,
    }
}

fn add_mcp_capabilities(
    agent: &Agent,
    home: &Path,
    homes: &[PathBuf],
    collected: &mut CollectedAccess,
) {
    let mut count = 0;
    for server in &agent.mcp_servers {
        if !source_applies_to_home(&server.source, home, homes) {
            continue;
        }
        count += 1;
        let decision = if server.enabled {
            AccessDecision::Unknown
        } else {
            AccessDecision::Deny
        };
        collected.capabilities.push(AccessCapability {
            category: AccessCategory::ExternalService,
            resource: format!("mcp:{}", server.name),
            operations: vec![AccessOperation::Use],
            decision,
            enforcement: AccessEnforcement::Harness,
            workspace: None,
            source: source(AccessSourceKind::Mcp, Some(server.source.clone())),
            rule: None,
            detail: Some(if server.enabled {
                "Configured and enabled; per-tool approval depends on the harness".to_owned()
            } else {
                "Configured but disabled".to_owned()
            }),
        });
        if !server.enabled {
            continue;
        }
        if let Some(url) = server.url.as_deref()
            && let Some(host) = host_from_url(url)
        {
            collected.capabilities.push(AccessCapability {
                category: AccessCategory::Network,
                resource: host,
                operations: vec![AccessOperation::Connect],
                decision: AccessDecision::Allow,
                enforcement: AccessEnforcement::Harness,
                workspace: None,
                source: source(AccessSourceKind::Mcp, Some(server.source.clone())),
                rule: None,
                detail: Some(format!("Endpoint for MCP server {}", server.name)),
            });
        } else if let Some(command) = server.command.as_deref() {
            collected.capabilities.push(AccessCapability {
                category: AccessCategory::Execution,
                resource: safe_command_identifier(command),
                operations: vec![AccessOperation::Execute],
                decision: AccessDecision::Allow,
                enforcement: AccessEnforcement::Harness,
                workspace: None,
                source: source(AccessSourceKind::Mcp, Some(server.source.clone())),
                rule: None,
                detail: Some(format!("Local process for MCP server {}", server.name)),
            });
            collected.findings.push(AccessFinding {
                severity: AccessSeverity::Notice,
                title: "Local MCP process".to_owned(),
                detail: format!(
                    "{} starts a local MCP process whose host access is not described by MCP configuration",
                    server.name
                ),
                category: AccessCategory::ExternalService,
                workspace: None,
                source: Some(source(
                    AccessSourceKind::Mcp,
                    Some(server.source.clone()),
                )),
            });
        }
    }
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::Mcp,
        status: AccessCoverageStatus::Partial,
        detail: format!(
            "Inspected {count} discovered MCP server{}; project-scoped, custom, or agent-specific definitions may be omitted",
            plural_suffix(count)
        ),
    });
}

fn source_applies_to_home(source: &Path, home: &Path, homes: &[PathBuf]) -> bool {
    let Ok(source) = source.canonicalize() else {
        return false;
    };
    let home = home.canonicalize().ok();
    let owner = homes
        .iter()
        .filter_map(|candidate| candidate.canonicalize().ok())
        .filter(|candidate| source.starts_with(candidate))
        .max_by_key(|candidate| candidate.components().count());
    match (owner.as_ref(), home.as_ref()) {
        (Some(owner), Some(home)) => owner == home,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn derive_findings(collected: &mut CollectedAccess) {
    let mut wildcard_network_rules =
        BTreeMap::<(Option<PathBuf>, AccessSource), Vec<String>>::new();
    for capability in &collected.capabilities {
        if matches!(capability.source.kind, AccessSourceKind::History) {
            continue;
        }
        if capability.category == AccessCategory::Network
            && capability.decision == AccessDecision::Allow
        {
            if capability.resource == "*" {
                collected.findings.push(AccessFinding {
                    severity: AccessSeverity::Warning,
                    title: "All network destinations allowed".to_owned(),
                    detail: "This setting allows connections to any network destination. Restrict it to required hosts or require approval"
                        .to_owned(),
                    category: AccessCategory::Network,
                    workspace: capability.workspace.clone(),
                    source: Some(capability.source.clone()),
                });
            } else if capability.resource.starts_with("*.") {
                wildcard_network_rules
                    .entry((capability.workspace.clone(), capability.source.clone()))
                    .or_default()
                    .push(capability.resource.clone());
            }
        }
        if capability.category == AccessCategory::Execution
            && capability.resource == "*"
            && capability.decision == AccessDecision::Allow
            && matches!(
                capability.enforcement,
                AccessEnforcement::None | AccessEnforcement::Unknown
            )
        {
            collected.findings.push(AccessFinding {
                severity: AccessSeverity::Critical,
                title: "Uncontained command execution".to_owned(),
                detail: "Commands can run without a declared sandbox boundary. Require approval or configure a sandbox"
                    .to_owned(),
                category: AccessCategory::Execution,
                workspace: capability.workspace.clone(),
                source: Some(capability.source.clone()),
            });
        }
    }
    for ((workspace, source), mut resources) in wildcard_network_rules {
        resources.sort();
        resources.dedup();
        let (title, detail) = if resources.len() == 1 {
            (
                "Wildcard network rule".to_owned(),
                format!(
                    "{} allows every matching subdomain, not one exact host",
                    resources[0]
                ),
            )
        } else {
            (
                format!("{} wildcard network rules", resources.len()),
                format!(
                    "{} wildcard domain rules each allow every matching subdomain; review the Network rules",
                    resources.len()
                ),
            )
        };
        collected.findings.push(AccessFinding {
            severity: AccessSeverity::Warning,
            title,
            detail,
            category: AccessCategory::Network,
            workspace,
            source: Some(source),
        });
    }
}

pub(super) fn source(kind: AccessSourceKind, path: Option<PathBuf>) -> AccessSource {
    AccessSource { kind, path }
}

pub fn network_rule_id(
    mechanism: &AccessRuleMechanism,
    path: &Path,
    native_identity: &str,
) -> String {
    let mechanism = match mechanism {
        AccessRuleMechanism::VscodeUrlAutoApprove => "vscode-url-auto-approve",
        AccessRuleMechanism::ClaudePermission => "claude-permission",
        AccessRuleMechanism::ClaudeSandboxDomain => "claude-sandbox-domain",
    };
    let mut hasher = Sha256::new();
    for value in [
        "agentdesktop-network-rule-v1",
        mechanism,
        &path.to_string_lossy(),
        native_identity,
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    let mut id = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

pub(super) fn network_rule_ref(
    mechanism: AccessRuleMechanism,
    path: &Path,
    native_identity: &str,
) -> AccessRuleRef {
    AccessRuleRef {
        id: network_rule_id(&mechanism, path, native_identity),
        mechanism,
    }
}

pub(super) fn host_from_url(value: &str) -> Option<String> {
    url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

pub(super) fn host_pattern(value: &str) -> Option<String> {
    let value = value.trim();
    let without_scheme = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?
        .trim();
    let host = authority
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(host, _)| host))
        .unwrap_or_else(|| authority.split(':').next().unwrap_or(authority));
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

pub fn normalize_network_resource(value: &str) -> Option<String> {
    host_pattern(value)
}

pub(super) fn safe_command_identifier(command: &str) -> String {
    let command = command.trim();
    if command.is_empty() {
        return "unknown command".to_owned();
    }
    if command.starts_with('/') && command.contains('^') {
        return "regular-expression command rule".to_owned();
    }
    let mut words = command.split_whitespace().filter(|word| {
        !word.contains('=')
            && word
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "._/-:@".contains(character))
    });
    let Some(program) = words.next() else {
        return "command rule".to_owned();
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let include_subcommand = matches!(
        program,
        "cargo" | "docker" | "git" | "go" | "kubectl" | "npm" | "pnpm" | "yarn"
    );
    if include_subcommand && let Some(subcommand) = words.next() {
        return format!("{program} {subcommand}");
    }
    program.to_owned()
}

pub(super) fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Default)]
pub(super) struct CollectedAccess {
    pub capabilities: Vec<AccessCapability>,
    pub observations: Vec<agentdesktop_core::model::AccessObservation>,
    pub findings: Vec<AccessFinding>,
    pub coverage: Vec<AccessCoverage>,
}

impl CollectedAccess {
    fn extend(&mut self, other: Self) {
        self.capabilities.extend(other.capabilities);
        self.observations.extend(other.observations);
        self.findings.extend(other.findings);
        self.coverage.extend(other.coverage);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use agentdesktop_core::model::{
        AccessCapability, AccessCategory, AccessDecision, AccessEnforcement, AccessOperation,
        AccessReportStatus, AccessSourceKind, Agent, Discovery, McpServer,
    };

    use super::{
        CollectedAccess, agent_order, assess, assess_agent, derive_findings, host_pattern,
        safe_command_identifier, source, unavailable,
    };

    #[test]
    fn groups_wildcard_network_findings_by_workspace() {
        let mut collected = CollectedAccess {
            capabilities: ["*.amazon.com", "*.apple.com", "*.cilium.io"]
                .into_iter()
                .map(|resource| AccessCapability {
                    category: AccessCategory::Network,
                    resource: resource.to_owned(),
                    operations: vec![AccessOperation::Connect],
                    decision: AccessDecision::Allow,
                    enforcement: AccessEnforcement::Harness,
                    workspace: Some(PathBuf::from("/workspace")),
                    source: source(AccessSourceKind::Configuration, None),
                    rule: None,
                    detail: None,
                })
                .collect(),
            ..CollectedAccess::default()
        };

        derive_findings(&mut collected);

        assert_eq!(collected.findings.len(), 1);
        assert_eq!(collected.findings[0].title, "3 wildcard network rules");
        assert_eq!(
            collected.findings[0].workspace.as_deref(),
            Some(Path::new("/workspace"))
        );
    }

    #[test]
    fn keeps_a_single_wildcard_finding_specific() {
        let mut collected = CollectedAccess::default();
        collected.capabilities.push(AccessCapability {
            category: AccessCategory::Network,
            resource: "*.example.com".to_owned(),
            operations: vec![AccessOperation::Connect],
            decision: AccessDecision::Allow,
            enforcement: AccessEnforcement::Harness,
            workspace: None,
            source: source(AccessSourceKind::Configuration, None),
            rule: None,
            detail: None,
        });

        derive_findings(&mut collected);

        assert_eq!(collected.findings.len(), 1);
        assert_eq!(collected.findings[0].title, "Wildcard network rule");
        assert!(collected.findings[0].detail.contains("*.example.com"));
    }

    #[test]
    fn historical_capabilities_do_not_create_current_findings() {
        let mut collected = CollectedAccess::default();
        collected.capabilities.push(AccessCapability {
            category: AccessCategory::Execution,
            resource: "*".to_owned(),
            operations: vec![AccessOperation::Execute],
            decision: AccessDecision::Allow,
            enforcement: AccessEnforcement::None,
            workspace: Some(PathBuf::from("/workspace")),
            source: source(AccessSourceKind::History, None),
            rule: None,
            detail: Some("Recorded full-access session".to_owned()),
        });

        derive_findings(&mut collected);

        assert!(collected.findings.is_empty());
    }

    #[test]
    fn reduces_urls_and_commands_without_retaining_arguments() {
        assert_eq!(
            host_pattern("https://*.example.com/private?token=secret").as_deref(),
            Some("*.example.com")
        );
        assert_eq!(
            safe_command_identifier("TOKEN=secret kubectl get pods --token secret"),
            "kubectl get"
        );
        assert_eq!(
            safe_command_identifier(r"/^curl https:\/\/secret.example/"),
            "regular-expression command rule"
        );
    }

    #[test]
    fn assessment_is_scoped_to_the_supplied_user_home() {
        let report = assess(
            &Discovery {
                agents: vec![Agent {
                    kind: "unsupported-test-agent".to_owned(),
                    executable: PathBuf::from("/usr/local/bin/test-agent"),
                    version: None,
                    mcp_servers: Vec::new(),
                    skills: Vec::new(),
                }],
                model_runtimes: Vec::new(),
            },
            Path::new("/Users/alice"),
        );

        assert_eq!(report.status, AccessReportStatus::Ready);
        assert_eq!(report.agents.len(), 1);
        assert_eq!(report.agents[0].user_home, Path::new("/Users/alice"));
    }

    #[cfg(unix)]
    #[test]
    fn assessment_excludes_mcp_sources_owned_by_another_user() {
        let root =
            std::env::temp_dir().join(format!("agentdesktop-mcp-owners-{}", std::process::id()));
        let alice = root.join("alice");
        let bob = root.join("bob");
        let bob_source = bob.join(".config/agent/mcp.json");
        let linked_source = alice.join("linked-mcp.json");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(bob_source.parent().unwrap()).unwrap();
        fs::create_dir_all(&alice).unwrap();
        fs::write(&bob_source, "{}\n").unwrap();
        symlink(&bob_source, &linked_source).unwrap();
        let agent = Agent {
            kind: "unsupported-test-agent".to_owned(),
            executable: PathBuf::from("/usr/local/bin/test-agent"),
            version: None,
            mcp_servers: vec![McpServer {
                name: "foreign".to_owned(),
                transport: "stdio".to_owned(),
                command: Some("foreign-server".to_owned()),
                url: None,
                enabled: true,
                source: linked_source,
            }],
            skills: Vec::new(),
        };

        let report = assess_agent(&agent, &alice, &[alice.clone(), bob]);

        let _ = fs::remove_dir_all(&root);
        assert!(report.capabilities.is_empty());
        assert!(report.findings.is_empty());
    }

    #[test]
    fn unavailable_report_contains_no_user_data() {
        let report = unavailable("caller identity is unavailable");

        assert_eq!(report.status, AccessReportStatus::Unavailable);
        assert!(report.agents.is_empty());
    }

    #[test]
    fn supported_agents_have_a_stable_display_order() {
        let mut kinds = [
            "opencode",
            "codex",
            "claude-desktop",
            "claude-code",
            "vscode",
        ];
        kinds.sort_by_key(|kind| agent_order(kind));

        assert_eq!(
            kinds,
            [
                "vscode",
                "claude-code",
                "claude-desktop",
                "codex",
                "opencode"
            ]
        );
    }
}
