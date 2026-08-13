use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentdesktop::identity::enrollment::load_device_identity_for;
use agentdesktop::identity::storage::CredentialStore;
use agentdesktop::organization::OrganizationBootstrap;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
#[cfg(target_os = "macos")]
use toml::Value as TomlValue;

const SCHEMA_VERSION: u8 = 1;
const MAX_CONFIG_BYTES: u64 = 1 << 20;
const MAX_RESOURCES: usize = 128;
const REPORT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const RESCAN_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct DiscoveryRoots {
    platform: &'static str,
    home: PathBuf,
    #[cfg(target_os = "macos")]
    applications: PathBuf,
    managed_claude: PathBuf,
    system_prefixes: Vec<PathBuf>,
    detect_processes: bool,
}

#[derive(Debug, Serialize, PartialEq)]
struct DiscoveryReport {
    schema_version: u8,
    collector_version: &'static str,
    platform: &'static str,
    coverage: Coverage,
    agents: Vec<AgentReport>,
    issues: Vec<CollectionIssue>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Coverage {
    project_scopes: &'static str,
    partial: bool,
}

#[derive(Deserialize)]
struct RescanStatus {
    pending: bool,
}

#[derive(Debug, Serialize, PartialEq)]
struct AgentReport {
    id: &'static str,
    installed: bool,
    version: Option<String>,
    running: &'static str,
    evidence: Vec<&'static str>,
    config_sources: Vec<ConfigSource>,
    mcp_servers: Vec<MCPServer>,
    skills: Vec<NamedResource>,
    plugins: Vec<Plugin>,
}

#[derive(Debug, Serialize, PartialEq)]
struct ConfigSource {
    scope: &'static str,
    source: &'static str,
    format: &'static str,
    status: &'static str,
    sections: Vec<&'static str>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct MCPServer {
    name: String,
    scope: &'static str,
    transport: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct NamedResource {
    name: String,
    scope: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Plugin {
    name: String,
    scope: &'static str,
    state: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CollectionIssue {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'static str>,
    code: &'static str,
}

struct AgentBuilder {
    id: &'static str,
    process_names: &'static [&'static str],
    installed: bool,
    version: Option<String>,
    evidence: BTreeSet<&'static str>,
    config_sources: Vec<ConfigSource>,
    mcp_servers: BTreeSet<MCPServer>,
    skills: BTreeSet<NamedResource>,
    plugins: BTreeSet<Plugin>,
}

enum ParsedConfig {
    Json(JsonValue),
    #[cfg(target_os = "macos")]
    Toml(TomlValue),
}

impl DiscoveryRoots {
    #[cfg(target_os = "macos")]
    fn current() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Self {
            platform: "macos",
            home,
            applications: PathBuf::from("/Applications"),
            managed_claude: PathBuf::from("/Library/Application Support/ClaudeCode"),
            system_prefixes: vec![PathBuf::from("/usr/local"), PathBuf::from("/opt/homebrew")],
            detect_processes: true,
        })
    }

    #[cfg(target_os = "windows")]
    fn current() -> Result<Self> {
        let home = env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .context("USERPROFILE is not set")?;
        let user_npm = env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("npm");
        let managed_claude = env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("ClaudeCode");
        Ok(Self {
            platform: "windows",
            home,
            managed_claude,
            system_prefixes: vec![user_npm],
            detect_processes: false,
        })
    }
}

impl AgentBuilder {
    fn new(id: &'static str, process_names: &'static [&'static str]) -> Self {
        Self {
            id,
            process_names,
            installed: false,
            version: None,
            evidence: BTreeSet::new(),
            config_sources: Vec::new(),
            mcp_servers: BTreeSet::new(),
            skills: BTreeSet::new(),
            plugins: BTreeSet::new(),
        }
    }

    fn evidence(&mut self, evidence: &'static str) {
        self.installed |= evidence != "configuration";
        self.evidence.insert(evidence);
    }

    fn finish(mut self, detect_processes: bool) -> AgentReport {
        self.config_sources.sort_by_key(|source| source.source);
        AgentReport {
            id: self.id,
            installed: self.installed,
            version: self.version,
            running: if !detect_processes {
                "unknown"
            } else if self
                .process_names
                .iter()
                .any(|name| process_is_running(name))
            {
                "detected"
            } else {
                "not_detected"
            },
            evidence: self.evidence.into_iter().collect(),
            config_sources: self.config_sources,
            mcp_servers: self.mcp_servers.into_iter().take(MAX_RESOURCES).collect(),
            skills: self.skills.into_iter().take(MAX_RESOURCES).collect(),
            plugins: self.plugins.into_iter().take(MAX_RESOURCES).collect(),
        }
    }
}

pub async fn run_reporter(bootstrap: OrganizationBootstrap, identity_root: PathBuf) {
    let roots = match DiscoveryRoots::current() {
        Ok(roots) => roots,
        Err(_) => return,
    };
    let mut last_report = None;
    loop {
        let periodic_due =
            last_report.is_none_or(|reported: Instant| reported.elapsed() >= REPORT_INTERVAL);
        let forced = !periodic_due
            && rescan_pending(&bootstrap, &identity_root)
                .await
                .unwrap_or(false);
        if (periodic_due || forced)
            && upload_current(&bootstrap, &identity_root, &roots)
                .await
                .is_ok()
        {
            last_report = Some(Instant::now());
        }
        tokio::time::sleep(RESCAN_POLL_INTERVAL).await;
    }
}

async fn upload_current(
    bootstrap: &OrganizationBootstrap,
    identity_root: &Path,
    roots: &DiscoveryRoots,
) -> Result<()> {
    let client = discovery_client(bootstrap, identity_root)?;
    let endpoint = bootstrap
        .identity
        .enrollment_url
        .join("v1/device-reports/current")?;
    let response = client.put(endpoint).json(&collect(roots)).send().await?;
    if !response.status().is_success() {
        bail!("discovery authority rejected the report")
    }
    Ok(())
}

async fn rescan_pending(bootstrap: &OrganizationBootstrap, identity_root: &Path) -> Result<bool> {
    let client = discovery_client(bootstrap, identity_root)?;
    let endpoint = bootstrap
        .identity
        .enrollment_url
        .join("v1/device-reports/current/rescan")?;
    let response = client.get(endpoint).send().await?;
    if !response.status().is_success() {
        bail!("discovery authority rejected the rescan check")
    }
    Ok(response.json::<RescanStatus>().await?.pending)
}

fn discovery_client(
    bootstrap: &OrganizationBootstrap,
    identity_root: &Path,
) -> Result<reqwest::Client> {
    if !identity_root.join("credential-storage").is_file() {
        bail!("managed credential storage is not initialized");
    }
    let store = CredentialStore::load(identity_root)?;
    let identity =
        load_device_identity_for(&bootstrap.identity.issuer, &bootstrap.gateway.url, &store)?;
    let mut client = reqwest::Client::builder().identity(identity);
    if let Some(path) = env::var_os("SSL_CERT_FILE").filter(|path| !path.is_empty()) {
        for certificate in reqwest::Certificate::from_pem_bundle(&fs::read(path)?)? {
            client = client.add_root_certificate(certificate);
        }
    }
    Ok(client.build()?)
}

fn collect(roots: &DiscoveryRoots) -> DiscoveryReport {
    let mut issues = BTreeSet::new();
    let mut agents = vec![collect_claude(roots, &mut issues)];
    #[cfg(target_os = "macos")]
    if roots.platform == "macos" {
        agents.extend([
            collect_claude_desktop(roots, &mut issues),
            collect_codex(roots, &mut issues),
            collect_openclaw(roots, &mut issues),
            collect_vscode(roots, &mut issues),
        ]);
    }
    agents.sort_by_key(|agent| agent.id);
    DiscoveryReport {
        schema_version: SCHEMA_VERSION,
        collector_version: env!("CARGO_PKG_VERSION"),
        platform: roots.platform,
        coverage: Coverage {
            project_scopes: "not_scanned",
            partial: issues.iter().any(|issue| issue.code != "symlink_skipped"),
        },
        agents,
        issues: issues.into_iter().collect(),
    }
}

#[cfg(target_os = "macos")]
fn collect_claude_desktop(
    roots: &DiscoveryRoots,
    issues: &mut BTreeSet<CollectionIssue>,
) -> AgentReport {
    let mut agent = AgentBuilder::new("claude-desktop", &["Claude"]);
    let application = roots.applications.join("Claude.app");
    let user_application = roots.home.join("Applications/Claude.app");
    for candidate in [&application, &user_application] {
        if candidate.is_dir() {
            agent.evidence("application");
            if agent.version.is_none() {
                agent.version = application_version(candidate);
            }
        }
    }
    inspect_claude_desktop_extensions(
        &mut agent,
        issues,
        &roots
            .home
            .join("Library/Application Support/Claude/extensions-installations.json"),
    );
    agent.finish(roots.detect_processes)
}

fn collect_claude(roots: &DiscoveryRoots, issues: &mut BTreeSet<CollectionIssue>) -> AgentReport {
    let mut agent = AgentBuilder::new("claude-code", &["claude"]);
    detect_executable(&mut agent, roots, "claude");
    agent.version = version_from_known_symlink(
        &roots.home.join(".local/bin/claude"),
        &roots.home.join(".local/share/claude/versions"),
    )
    .or_else(|| npm_package_version(roots, "@anthropic-ai/claude-code"));
    inspect_json(
        &mut agent,
        issues,
        &roots.home.join(".claude/settings.json"),
        "claude-settings",
        "user",
    );
    inspect_json(
        &mut agent,
        issues,
        &roots.home.join(".claude.json"),
        "claude-user-config",
        "user",
    );
    inspect_json(
        &mut agent,
        issues,
        &roots.managed_claude.join("managed-mcp.json"),
        "claude-managed-mcp",
        "managed",
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".claude/skills"),
        "user",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    agent.finish(roots.detect_processes)
}

#[cfg(target_os = "macos")]
fn collect_codex(roots: &DiscoveryRoots, issues: &mut BTreeSet<CollectionIssue>) -> AgentReport {
    let mut agent = AgentBuilder::new("codex-cli", &["codex"]);
    detect_executable(&mut agent, roots, "codex");
    agent.version = npm_package_version(roots, "@openai/codex");
    let codex_home = roots.home.join(".codex");
    inspect_toml(
        &mut agent,
        issues,
        &codex_home.join("config.toml"),
        "codex-config",
        "user",
    );
    collect_named_directories(
        &mut agent.skills,
        &codex_home.join("skills"),
        "user",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".agents/skills"),
        "shared",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    agent.finish(roots.detect_processes)
}

#[cfg(target_os = "macos")]
fn collect_openclaw(roots: &DiscoveryRoots, issues: &mut BTreeSet<CollectionIssue>) -> AgentReport {
    let mut agent = AgentBuilder::new("openclaw", &["openclaw"]);
    detect_executable(&mut agent, roots, "openclaw");
    agent.version = npm_package_version(roots, "openclaw");
    let state = roots.home.join(".openclaw");
    let config = state.join("openclaw.json");
    inspect_json(&mut agent, issues, &config, "openclaw-config", "user");
    collect_named_directories(
        &mut agent.skills,
        &state.join("skills"),
        "user",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".agents/skills"),
        "shared",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_named_directories(
        &mut agent.plugins,
        &state.join("extensions"),
        "user",
        None,
        issues,
        agent.id,
    );
    agent.finish(roots.detect_processes)
}

#[cfg(target_os = "macos")]
fn collect_vscode(roots: &DiscoveryRoots, issues: &mut BTreeSet<CollectionIssue>) -> AgentReport {
    let mut agent = AgentBuilder::new("vscode-copilot", &["Visual Studio Code"]);
    if roots.applications.join("Visual Studio Code.app").is_dir()
        || roots
            .home
            .join("Applications/Visual Studio Code.app")
            .is_dir()
    {
        agent.evidence("application");
    }
    let extensions = roots.home.join(".vscode/extensions");
    if let Ok(entries) = safe_directory_entries(&extensions, issues, agent.id) {
        for entry in entries.into_iter().rev() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("github.copilot-") || name.starts_with("github.copilot-chat-") {
                agent.evidence("extension");
                if agent.version.is_none() {
                    agent.version = package_manifest_version(&entry.path().join("package.json"));
                }
                insert_plugin(&mut agent.plugins, name, "user", "enabled");
            }
        }
    }
    let code_user = roots.home.join("Library/Application Support/Code/User");
    inspect_json(
        &mut agent,
        issues,
        &code_user.join("mcp.json"),
        "vscode-user-mcp",
        "user",
    );
    if let Ok(profiles) = safe_directory_entries(&code_user.join("profiles"), issues, agent.id) {
        for profile in profiles.into_iter().take(32) {
            inspect_json(
                &mut agent,
                issues,
                &profile.path().join("mcp.json"),
                "vscode-profile-mcp",
                "user",
            );
        }
    }
    inspect_json(
        &mut agent,
        issues,
        &roots.home.join(".copilot/mcp-config.json"),
        "copilot-mcp",
        "user",
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".copilot/skills"),
        "user",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".claude/skills"),
        "shared",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_named_directories(
        &mut agent.skills,
        &roots.home.join(".agents/skills"),
        "shared",
        Some("SKILL.md"),
        issues,
        agent.id,
    );
    collect_nested_plugins(
        &mut agent.plugins,
        &roots.home.join(".copilot/installed-plugins"),
        issues,
        agent.id,
    );
    agent.finish(roots.detect_processes)
}

fn detect_executable(agent: &mut AgentBuilder, roots: &DiscoveryRoots, name: &str) {
    let executable_names = if roots.platform == "windows" {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.ps1"),
        ]
    } else {
        vec![name.to_owned()]
    };
    let mut candidates = executable_names
        .iter()
        .map(|name| roots.home.join(".local/bin").join(name))
        .collect::<Vec<_>>();
    for prefix in &roots.system_prefixes {
        candidates.extend(executable_names.iter().map(|name| {
            if roots.platform == "windows" {
                prefix.join(name)
            } else {
                prefix.join("bin").join(name)
            }
        }));
    }
    if candidates
        .iter()
        .any(|path| fs::symlink_metadata(path).is_ok())
    {
        agent.evidence("executable");
    }
}

fn npm_package_version(roots: &DiscoveryRoots, package: &str) -> Option<String> {
    let user_root = roots.home.join(".local/lib/node_modules");
    std::iter::once(user_root)
        .chain(roots.system_prefixes.iter().map(|prefix| {
            if roots.platform == "windows" {
                prefix.join("node_modules")
            } else {
                prefix.join("lib/node_modules")
            }
        }))
        .map(|root| root.join(package).join("package.json"))
        .find_map(|manifest| package_manifest_version(&manifest))
}

fn package_manifest_version(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES
    {
        return None;
    }
    let package: JsonValue = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    safe_version(package.get("version")?.as_str()?)
}

#[cfg(target_os = "macos")]
fn application_version(application: &Path) -> Option<String> {
    let info = application.join("Contents/Info.plist");
    let metadata = fs::symlink_metadata(&info).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES
    {
        return None;
    }
    let value = plist::Value::from_file(info).ok()?;
    safe_version(
        value
            .as_dictionary()?
            .get("CFBundleShortVersionString")?
            .as_string()?,
    )
}

fn version_from_known_symlink(link: &Path, versions_root: &Path) -> Option<String> {
    if !fs::symlink_metadata(link).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = fs::canonicalize(link).ok()?;
    let versions_root = fs::canonicalize(versions_root).ok()?;
    if target.parent()? != versions_root {
        return None;
    }
    safe_version(target.file_name()?.to_str()?)
}

fn safe_version(value: &str) -> Option<String> {
    let value = value.strip_prefix('v').unwrap_or(value);
    if value.is_empty()
        || value.len() > 64
        || !value.as_bytes()[0].is_ascii_digit()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-' | b'_'))
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(target_os = "macos")]
fn process_is_running(name: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "windows")]
fn process_is_running(_name: &str) -> bool {
    false
}

fn inspect_json(
    agent: &mut AgentBuilder,
    issues: &mut BTreeSet<CollectionIssue>,
    path: &Path,
    source: &'static str,
    scope: &'static str,
) {
    inspect_config(agent, issues, path, source, scope, "json", |contents| {
        json5::from_str(contents)
            .map(ParsedConfig::Json)
            .map_err(|_| ())
    });
}

#[cfg(target_os = "macos")]
fn inspect_toml(
    agent: &mut AgentBuilder,
    issues: &mut BTreeSet<CollectionIssue>,
    path: &Path,
    source: &'static str,
    scope: &'static str,
) {
    inspect_config(agent, issues, path, source, scope, "toml", |contents| {
        toml::from_str::<TomlValue>(contents)
            .map(ParsedConfig::Toml)
            .map_err(|_| ())
    });
}

#[cfg(target_os = "macos")]
fn inspect_claude_desktop_extensions(
    agent: &mut AgentBuilder,
    issues: &mut BTreeSet<CollectionIssue>,
    path: &Path,
) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => return,
    };
    agent.evidence("extension");
    let (status, parsed) = if metadata.file_type().is_symlink() || !metadata.is_file() {
        issues.insert(CollectionIssue {
            agent_id: Some(agent.id),
            code: "symlink_skipped",
        });
        ("symlink_skipped", None)
    } else if metadata.len() > MAX_CONFIG_BYTES {
        issues.insert(CollectionIssue {
            agent_id: Some(agent.id),
            code: "oversized_config",
        });
        ("oversized", None)
    } else {
        match fs::read(path)
            .ok()
            .and_then(|contents| serde_json::from_slice::<JsonValue>(&contents).ok())
        {
            Some(parsed) => ("parsed", Some(parsed)),
            None => {
                issues.insert(CollectionIssue {
                    agent_id: Some(agent.id),
                    code: "invalid_config",
                });
                ("invalid", None)
            }
        }
    };
    agent.config_sources.push(ConfigSource {
        scope: "user",
        source: "claude-desktop-extensions",
        format: "json",
        status,
        sections: if parsed.is_some() {
            vec!["mcp"]
        } else {
            Vec::new()
        },
    });
    let Some(extensions) = parsed
        .as_ref()
        .and_then(|value| value.get("extensions"))
        .and_then(JsonValue::as_object)
    else {
        return;
    };
    let mut entries = extensions.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(id, _)| id.as_str());
    if entries.len() > MAX_RESOURCES {
        issues.insert(CollectionIssue {
            agent_id: Some(agent.id),
            code: "entry_limit_reached",
        });
    }
    for (id, extension) in entries.into_iter().take(MAX_RESOURCES) {
        let manifest = extension.get("manifest").and_then(JsonValue::as_object);
        let name = manifest
            .and_then(|value| value.get("display_name").or_else(|| value.get("name")))
            .and_then(JsonValue::as_str)
            .or(Some(id.as_str()))
            .and_then(safe_name);
        let Some(name) = name else { continue };
        let transport = manifest
            .and_then(|value| value.get("server"))
            .and_then(|value| value.get("mcp_config"))
            .map(|value| transport(ConfigValue::Json(value)))
            .unwrap_or("unknown");
        agent.mcp_servers.insert(MCPServer {
            name,
            scope: "user",
            transport,
        });
    }
}

fn inspect_config(
    agent: &mut AgentBuilder,
    issues: &mut BTreeSet<CollectionIssue>,
    path: &Path,
    source: &'static str,
    scope: &'static str,
    format: &'static str,
    parse: impl FnOnce(&str) -> std::result::Result<ParsedConfig, ()>,
) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => return,
    };
    agent.evidence("configuration");
    let (status, sections, parsed) = if metadata.file_type().is_symlink() || !metadata.is_file() {
        issues.insert(CollectionIssue {
            agent_id: Some(agent.id),
            code: "symlink_skipped",
        });
        ("symlink_skipped", Vec::new(), None)
    } else if metadata.len() > MAX_CONFIG_BYTES {
        issues.insert(CollectionIssue {
            agent_id: Some(agent.id),
            code: "oversized_config",
        });
        ("oversized", Vec::new(), None)
    } else {
        match fs::read_to_string(path)
            .ok()
            .and_then(|contents| parse(&contents).ok())
        {
            Some(parsed) => ("parsed", safe_sections(&parsed), Some(parsed)),
            None => {
                issues.insert(CollectionIssue {
                    agent_id: Some(agent.id),
                    code: "invalid_config",
                });
                ("invalid", Vec::new(), None)
            }
        }
    };
    agent.config_sources.push(ConfigSource {
        scope,
        source,
        format,
        status,
        sections,
    });
    if let Some(parsed) = parsed {
        extract_resources(agent, &parsed, scope);
    }
}

fn safe_sections(config: &ParsedConfig) -> Vec<&'static str> {
    let mut sections = BTreeSet::new();
    for (name, present) in [
        ("agents", has_path(config, &["agents"])),
        (
            "mcp",
            has_any_path(
                config,
                &[
                    &["mcpServers"],
                    &["mcp_servers"],
                    &["servers"],
                    &["mcp", "servers"],
                ],
            ),
        ),
        ("models", has_any_path(config, &[&["model"], &["models"]])),
        (
            "plugins",
            has_any_path(config, &[&["plugins"], &["enabledPlugins"]]),
        ),
        (
            "providers",
            has_any_path(
                config,
                &[&["provider"], &["providers"], &["model_provider"]],
            ),
        ),
        (
            "skills",
            has_any_path(config, &[&["skills"], &["skills", "entries"]]),
        ),
    ] {
        if present {
            sections.insert(name);
        }
    }
    sections.into_iter().collect()
}

fn extract_resources(agent: &mut AgentBuilder, config: &ParsedConfig, scope: &'static str) {
    for path in [
        &["mcpServers"][..],
        &["mcp_servers"][..],
        &["servers"][..],
        &["mcp", "servers"][..],
    ] {
        for (name, value) in object_entries(config, path) {
            if let Some(name) = safe_name(name) {
                agent.mcp_servers.insert(MCPServer {
                    name,
                    scope,
                    transport: transport(value),
                });
            }
        }
    }
    for path in [
        &["enabledPlugins"][..],
        &["plugins"][..],
        &["plugins", "entries"][..],
    ] {
        for (name, value) in object_entries(config, path) {
            if let Some(name) = safe_name(name) {
                let state = if boolean_field(value, "enabled") == Some(true)
                    || value_bool(value) == Some(true)
                {
                    "enabled"
                } else {
                    "configured"
                };
                agent.plugins.insert(Plugin {
                    name,
                    scope: "user",
                    state,
                });
            }
        }
    }
    for path in [&["skills"][..], &["skills", "entries"][..]] {
        for (name, _) in object_entries(config, path) {
            if let Some(name) = safe_name(name) {
                agent.skills.insert(NamedResource {
                    name,
                    scope: "user",
                });
            }
        }
    }
}

fn has_any_path(config: &ParsedConfig, paths: &[&[&str]]) -> bool {
    paths.iter().any(|path| has_path(config, path))
}

fn has_path(config: &ParsedConfig, path: &[&str]) -> bool {
    match config {
        ParsedConfig::Json(value) => json_at(value, path).is_some(),
        #[cfg(target_os = "macos")]
        ParsedConfig::Toml(value) => toml_at(value, path).is_some(),
    }
}

fn object_entries<'a>(config: &'a ParsedConfig, path: &[&str]) -> Vec<(&'a str, ConfigValue<'a>)> {
    match config {
        ParsedConfig::Json(value) => json_at(value, path)
            .and_then(JsonValue::as_object)
            .map_or_else(Vec::new, |object| {
                object
                    .iter()
                    .map(|(name, value)| (name.as_str(), ConfigValue::Json(value)))
                    .collect()
            }),
        #[cfg(target_os = "macos")]
        ParsedConfig::Toml(value) => toml_at(value, path)
            .and_then(TomlValue::as_table)
            .map_or_else(Vec::new, |table| {
                table
                    .iter()
                    .map(|(name, value)| (name.as_str(), ConfigValue::Toml(value)))
                    .collect()
            }),
    }
}

#[derive(Clone, Copy)]
enum ConfigValue<'a> {
    Json(&'a JsonValue),
    #[cfg(target_os = "macos")]
    Toml(&'a TomlValue),
}

fn json_at<'a>(mut value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

#[cfg(target_os = "macos")]
fn toml_at<'a>(mut value: &'a TomlValue, path: &[&str]) -> Option<&'a TomlValue> {
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn transport(value: ConfigValue<'_>) -> &'static str {
    let explicit = string_field(value, "type").or_else(|| string_field(value, "transport"));
    match explicit.as_deref() {
        Some("sse") => "sse",
        Some("http") | Some("streamable-http") => "http",
        Some("stdio") => "stdio",
        _ if has_field(value, "command") => "stdio",
        _ if has_field(value, "url") => "http",
        _ => "unknown",
    }
}

fn has_field(value: ConfigValue<'_>, field: &str) -> bool {
    match value {
        ConfigValue::Json(value) => value.get(field).is_some(),
        #[cfg(target_os = "macos")]
        ConfigValue::Toml(value) => value.get(field).is_some(),
    }
}

fn string_field(value: ConfigValue<'_>, field: &str) -> Option<String> {
    match value {
        ConfigValue::Json(value) => value.get(field)?.as_str().map(str::to_owned),
        #[cfg(target_os = "macos")]
        ConfigValue::Toml(value) => value.get(field)?.as_str().map(str::to_owned),
    }
}

fn boolean_field(value: ConfigValue<'_>, field: &str) -> Option<bool> {
    match value {
        ConfigValue::Json(value) => value.get(field)?.as_bool(),
        #[cfg(target_os = "macos")]
        ConfigValue::Toml(value) => value.get(field)?.as_bool(),
    }
}

fn value_bool(value: ConfigValue<'_>) -> Option<bool> {
    match value {
        ConfigValue::Json(value) => value.as_bool(),
        #[cfg(target_os = "macos")]
        ConfigValue::Toml(value) => value.as_bool(),
    }
}

fn collect_named_directories<T>(
    output: &mut BTreeSet<T>,
    root: &Path,
    scope: &'static str,
    marker: Option<&str>,
    issues: &mut BTreeSet<CollectionIssue>,
    agent_id: &'static str,
) where
    T: Ord + FromNamedResource,
{
    if let Ok(entries) = safe_directory_entries(root, issues, agent_id) {
        for entry in entries.into_iter().take(MAX_RESOURCES) {
            if marker.is_some_and(|marker| !entry.path().join(marker).is_file()) {
                continue;
            }
            if let Some(name) = safe_name(&entry.file_name().to_string_lossy()) {
                output.insert(T::from_name(name, scope));
            }
        }
    }
}

trait FromNamedResource {
    fn from_name(name: String, scope: &'static str) -> Self;
}
impl FromNamedResource for NamedResource {
    fn from_name(name: String, scope: &'static str) -> Self {
        Self { name, scope }
    }
}
impl FromNamedResource for Plugin {
    fn from_name(name: String, scope: &'static str) -> Self {
        Self {
            name,
            scope,
            state: "unknown",
        }
    }
}

#[cfg(target_os = "macos")]
fn collect_nested_plugins(
    output: &mut BTreeSet<Plugin>,
    root: &Path,
    issues: &mut BTreeSet<CollectionIssue>,
    agent_id: &'static str,
) {
    if let Ok(groups) = safe_directory_entries(root, issues, agent_id) {
        for group in groups.into_iter().take(32) {
            if let Ok(entries) = safe_directory_entries(&group.path(), issues, agent_id) {
                for entry in entries.into_iter().take(MAX_RESOURCES) {
                    if let Some(name) = safe_name(&entry.file_name().to_string_lossy()) {
                        insert_plugin(output, name, "user", "enabled");
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn insert_plugin(
    output: &mut BTreeSet<Plugin>,
    name: String,
    scope: &'static str,
    state: &'static str,
) {
    output.insert(Plugin { name, scope, state });
}

fn safe_directory_entries(
    root: &Path,
    issues: &mut BTreeSet<CollectionIssue>,
    agent_id: &'static str,
) -> Result<Vec<fs::DirEntry>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        issues.insert(CollectionIssue {
            agent_id: Some(agent_id),
            code: "symlink_skipped",
        });
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(fs::DirEntry::file_name);
    if entries.len() > MAX_RESOURCES {
        issues.insert(CollectionIssue {
            agent_id: Some(agent_id),
            code: "entry_limit_reached",
        });
        entries.truncate(MAX_RESOURCES);
    }
    Ok(entries)
}

fn safe_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{DiscoveryRoots, collect};
    use std::fs;

    #[cfg(target_os = "macos")]
    #[test]
    fn collection_is_deterministic_and_never_serializes_secrets_or_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::create_dir_all(home.join(".local/share/claude/versions")).unwrap();
        fs::write(
            home.join(".local/share/claude/versions/2.1.4"),
            "SECRET_EXECUTABLE_CONTENT",
        )
        .unwrap();
        fs::write(home.join(".local/bin/codex"), "SECRET_CODEX_BINARY").unwrap();
        fs::write(home.join(".local/bin/openclaw"), "SECRET_OPENCLAW_BINARY").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            home.join(".local/share/claude/versions/2.1.4"),
            home.join(".local/bin/claude"),
        )
        .unwrap();
        fs::create_dir_all(home.join(".claude/skills/review-pr")).unwrap();
        fs::write(
            home.join(".claude/skills/review-pr/SKILL.md"),
            "SECRET_SKILL_CONTENT",
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"github":{"command":"SECRET_COMMAND","env":{"TOKEN":"SECRET_TOKEN"}}},"apiKey":"SECRET_API_KEY"}"#,
        ).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'SECRET_CODEX_COMMAND'\n",
        )
        .unwrap();
        let prefix = temporary.path().join("prefix");
        fs::create_dir_all(prefix.join("lib/node_modules/@openai/codex")).unwrap();
        fs::write(
            prefix.join("lib/node_modules/@openai/codex/package.json"),
            r#"{"version":"0.42.0","token":"SECRET_CODEX_PACKAGE"}"#,
        )
        .unwrap();
        fs::create_dir_all(prefix.join("lib/node_modules/@anthropic-ai/claude-code")).unwrap();
        fs::write(
            prefix.join("lib/node_modules/@anthropic-ai/claude-code/package.json"),
            r#"{"version":"2.1.4"}"#,
        )
        .unwrap();
        fs::create_dir_all(prefix.join("lib/node_modules/openclaw")).unwrap();
        fs::write(
            prefix.join("lib/node_modules/openclaw/package.json"),
            r#"{"version":"1.8.2","token":"SECRET_OPENCLAW_PACKAGE"}"#,
        )
        .unwrap();
        fs::create_dir_all(home.join(".vscode/extensions/github.copilot-1.402.0")).unwrap();
        fs::write(
            home.join(".vscode/extensions/github.copilot-1.402.0/package.json"),
            r#"{"version":"1.402.0","token":"SECRET_COPILOT_PACKAGE"}"#,
        )
        .unwrap();
        fs::create_dir_all(home.join("Library/Application Support/Claude")).unwrap();
        fs::write(
            home.join("Library/Application Support/Claude/extensions-installations.json"),
            r#"{"extensions":{"local.excalidraw":{"version":"0.3.2","manifest":{"display_name":"Excalidraw","server":{"mcp_config":{"command":"SECRET_EXTENSION_COMMAND","args":["SECRET_EXTENSION_ARG"]}}},"secret":"SECRET_EXTENSION_TOKEN"}}}"#,
        )
        .unwrap();
        let roots = DiscoveryRoots {
            platform: "macos",
            home: home.clone(),
            applications: temporary.path().join("Applications"),
            managed_claude: temporary.path().join("managed"),
            system_prefixes: vec![prefix],
            detect_processes: false,
        };
        let first = serde_json::to_string(&collect(&roots)).unwrap();
        let second = serde_json::to_string(&collect(&roots)).unwrap();
        assert_eq!(first, second);
        assert!(
            first.contains("claude-code")
                && first.contains("github")
                && first.contains("review-pr")
                && first.contains("docs")
                && first.contains("2.1.4")
                && first.contains("0.42.0")
                && first.contains("1.8.2")
                && first.contains("1.402.0")
                && first.contains("claude-desktop")
                && first.contains("Excalidraw"),
            "report = {first}"
        );
        for secret in [
            "SECRET_COMMAND",
            "SECRET_TOKEN",
            "SECRET_API_KEY",
            "SECRET_SKILL_CONTENT",
            "SECRET_CODEX_COMMAND",
            "SECRET_EXECUTABLE_CONTENT",
            "SECRET_CODEX_BINARY",
            "SECRET_OPENCLAW_BINARY",
            "SECRET_CODEX_PACKAGE",
            "SECRET_OPENCLAW_PACKAGE",
            "SECRET_COPILOT_PACKAGE",
            "SECRET_EXTENSION_COMMAND",
            "SECRET_EXTENSION_ARG",
            "SECRET_EXTENSION_TOKEN",
            home.to_string_lossy().as_ref(),
        ] {
            assert!(!first.contains(secret), "report leaked {secret}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn collection_skips_symlinked_configuration() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let secret = temporary.path().join("secret.json");
        fs::write(&secret, r#"{"mcpServers":{"secret-server":{}}}"#).unwrap();
        symlink(&secret, home.join(".claude.json")).unwrap();
        let report = collect(&DiscoveryRoots {
            platform: "macos",
            home,
            #[cfg(target_os = "macos")]
            applications: temporary.path().join("Applications"),
            managed_claude: temporary.path().join("managed"),
            system_prefixes: Vec::new(),
            detect_processes: false,
        });
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(encoded.contains("symlink_skipped"));
        assert!(!encoded.contains("secret-server"));
    }

    #[test]
    fn collection_reports_malformed_and_oversized_sources_without_scanning_projects() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), "SECRET_MALFORMED_VALUE").unwrap();
        fs::write(
            home.join(".claude.json"),
            vec![b'x'; (super::MAX_CONFIG_BYTES + 1) as usize],
        )
        .unwrap();
        let project = temporary.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join(".mcp.json"),
            r#"{"mcpServers":{"SECRET_PROJECT_SERVER":{}}}"#,
        )
        .unwrap();
        let report = collect(&DiscoveryRoots {
            platform: "macos",
            home,
            #[cfg(target_os = "macos")]
            applications: temporary.path().join("Applications"),
            managed_claude: temporary.path().join("managed"),
            system_prefixes: Vec::new(),
            detect_processes: false,
        });
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(encoded.contains("invalid_config") && encoded.contains("oversized_config"));
        assert!(
            !encoded.contains("SECRET_MALFORMED_VALUE")
                && !encoded.contains("SECRET_PROJECT_SERVER")
        );
    }

    #[test]
    fn windows_collection_reports_claude_code_from_fixed_user_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("Users/developer");
        let user_npm = home.join("AppData/Roaming/npm");
        let managed_claude = temporary.path().join("ProgramData/ClaudeCode");
        fs::create_dir_all(user_npm.join("node_modules/@anthropic-ai/claude-code")).unwrap();
        fs::write(user_npm.join("claude.cmd"), "SECRET_LAUNCHER").unwrap();
        fs::write(
            user_npm.join("node_modules/@anthropic-ai/claude-code/package.json"),
            r#"{"version":"2.1.4","token":"SECRET_PACKAGE"}"#,
        )
        .unwrap();
        fs::create_dir_all(home.join(".claude/skills/review-pr")).unwrap();
        fs::write(
            home.join(".claude/skills/review-pr/SKILL.md"),
            "SECRET_SKILL",
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"github":{"command":"SECRET_COMMAND"}}}"#,
        )
        .unwrap();
        fs::create_dir_all(&managed_claude).unwrap();
        fs::write(
            managed_claude.join("managed-mcp.json"),
            r#"{"mcpServers":{"organization":{"url":"SECRET_URL"}}}"#,
        )
        .unwrap();

        let report = collect(&DiscoveryRoots {
            platform: "windows",
            home,
            #[cfg(target_os = "macos")]
            applications: temporary.path().join("Applications"),
            managed_claude,
            system_prefixes: vec![user_npm],
            detect_processes: false,
        });

        assert_eq!(report.platform, "windows");
        assert_eq!(report.agents.len(), 1);
        let claude = &report.agents[0];
        assert_eq!(claude.id, "claude-code");
        assert!(claude.installed);
        assert_eq!(claude.version.as_deref(), Some("2.1.4"));
        assert_eq!(claude.running, "unknown");
        assert!(claude.evidence.contains(&"executable"));
        assert!(claude.evidence.contains(&"configuration"));
        assert!(
            claude
                .mcp_servers
                .iter()
                .any(|server| server.name == "github")
        );
        assert!(
            claude
                .mcp_servers
                .iter()
                .any(|server| server.name == "organization")
        );
        assert!(claude.skills.iter().any(|skill| skill.name == "review-pr"));

        let encoded = serde_json::to_string(&report).unwrap();
        for secret in [
            "SECRET_LAUNCHER",
            "SECRET_PACKAGE",
            "SECRET_SKILL",
            "SECRET_COMMAND",
            "SECRET_URL",
        ] {
            assert!(!encoded.contains(secret), "report leaked {secret}");
        }
    }
}
