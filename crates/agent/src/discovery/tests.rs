use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde_json::json;

use super::{context::ScanContext, discover_harnesses};

struct Fixture {
    root: PathBuf,
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "discovery-fixture-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let root = directory.join("scan");
        fs::create_dir_all(root.join("home/install/bin")).unwrap();
        fs::create_dir_all(root.join("project/nested")).unwrap();
        Self { root, directory }
    }

    fn context(&self) -> ScanContext {
        ScanContext::isolated(self.root.join("home"), self.root.join("project/nested"))
            .with_search_path(vec![self.root.join("home/install/bin")])
    }

    fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn install(&self, name: &str) -> PathBuf {
        self.write(
            &format!("home/install/bin/{name}"),
            "not an executable; discovery must only read it",
        )
    }

    fn json(&self, relative: &str, value: serde_json::Value) -> PathBuf {
        self.write(relative, serde_json::to_vec(&value).unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn whole_registry_is_isolated_ordered_and_keeps_unknown_versions() {
    let fixture = Fixture::new();
    let context = fixture.context();
    assert!(discover_harnesses(&context).is_empty());
    for name in ["codex", "opencode", "claude", "claude-desktop", "code"] {
        fixture.install(name);
    }
    let agents = discover_harnesses(&context);
    let kinds: Vec<_> = agents.iter().map(|agent| agent.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "codex",
            "opencode",
            "claude-code",
            "claude-desktop",
            "vscode"
        ]
    );
    assert_eq!(kinds.iter().collect::<BTreeSet<_>>().len(), agents.len());
    for agent in agents {
        assert!(
            agent
                .executable
                .starts_with(fixture.root.join("home/install/bin"))
        );
        assert!(agent.version.is_none());
        assert!(agent.mcp_servers.is_empty());
        assert!(agent.skills.is_empty());
    }
}

#[test]
fn whole_adapters_keep_native_formats_and_sanitize_inventory() {
    let fixture = Fixture::new();
    for name in ["codex", "opencode", "claude", "claude-desktop", "code"] {
        fixture.install(name);
    }
    fixture.write(
        "home/install/bin/opencode",
        "--user-agent=opencode/1.18.11 --end",
    );
    fixture.json(
        "home/node_modules/@openai/codex/package.json",
        json!({"name":"@openai/codex","version":"0.129.0"}),
    );
    fixture.json(
        "home/install/bin/resources/app/package.json",
        json!({"version":"1.100.0"}),
    );
    let url = "https://USER_SENTINEL:PASS_SENTINEL@example.com/PATH_SENTINEL/mcp?token=QUERY_SENTINEL#FRAGMENT_SENTINEL";
    let server = json!({"type":"sse", "url":url,"headers":{"Authorization":"HEADER_SENTINEL"},"enabled":false});
    fixture.json("home/.claude.json", json!({"mcpServers":{"docs":server},"projects":{"ignored":{"mcpServers":{"private":{"command":"IGNORE_SENTINEL"}}}}}));
    fixture.json(
        "home/.config/Claude/claude_desktop_config.json",
        json!({"mcpServers":{"docs":server}}),
    );
    fixture.write(
        "project/.vscode/mcp.json",
        format!("{{ // native JSON5\n servers: {{docs: {},}},}}", server),
    );
    fixture.write("home/.codex/config.toml", format!("[mcp_servers.docs]\nurl = '{url}'\nenabled = false\n[mcp_servers.local]\ncommand = 'npx'\nargs = ['ARG_SENTINEL']\n[mcp_servers.local.env]\nTOKEN = 'ENV_SENTINEL'\n"));
    let skill = "---\nname: fixture-skill\n---\nBODY_SENTINEL";
    fixture.write("home/.claude/skills/example/SKILL.md", skill);
    fixture.write("home/.agents/skills/example/SKILL.md", skill);
    let agents = discover_harnesses(&fixture.context());
    assert_eq!(agents.len(), 5);
    for agent in &agents {
        if agent.kind == "opencode" {
            assert_eq!(agent.version.as_deref(), Some("1.18.11"));
            assert!(agent.mcp_servers.is_empty());
            assert!(agent.skills.is_empty());
            continue;
        }
        let remote = agent
            .mcp_servers
            .iter()
            .find(|server| server.name == "docs")
            .unwrap();
        assert_eq!(remote.url.as_deref(), Some("https://example.com/"));
        assert!(remote.source.starts_with(&fixture.root));
        assert_eq!(
            remote.enabled,
            matches!(agent.kind.as_str(), "claude-code" | "claude-desktop")
        );
        assert_eq!(
            remote.transport,
            if agent.kind == "codex" { "http" } else { "sse" }
        );
        if agent.kind == "claude-desktop" {
            assert!(agent.skills.is_empty());
        } else {
            assert!(!agent.skills.is_empty());
        }
    }
    assert_eq!(agents[0].version.as_deref(), Some("0.129.0"));
    assert_eq!(agents[4].version.as_deref(), Some("1.100.0"));
    assert!(!serde_json::to_string(&agents).unwrap().contains("SENTINEL"));
}

#[test]
fn whole_adapters_keep_undisclosable_endpoints_and_duplicate_sources() {
    let fixture = Fixture::new();
    for name in ["codex", "claude", "claude-desktop", "code"] {
        fixture.install(name);
    }
    let endpoints = [
        (
            "remote",
            "https://USER_SENTINEL:PASS_SENTINEL@example.com/PATH_SENTINEL/mcp?token=QUERY_SENTINEL#FRAGMENT_SENTINEL",
            Some("https://example.com/"),
        ),
        (
            "localhost",
            "http://localhost:8080/PATH_SENTINEL",
            Some("http://localhost:8080/"),
        ),
        ("socket", "unix:///tmp/SOCKET_SENTINEL.sock", None),
        ("pipe", "pipe:///PIPE_SENTINEL", None),
        ("named-pipe", r"\\.\pipe\PIPE_SENTINEL", None),
        ("variable", "${env:URL_SENTINEL}", None),
        (
            "template-path",
            "https://example.com/${input:PATH_SENTINEL}",
            None,
        ),
        ("template-host", "https://${env:HOST_SENTINEL}/mcp", None),
        ("invalid", "https://example.com:PORT_SENTINEL/mcp", None),
    ];
    let mut entries = serde_json::Map::new();
    for (name, url, _) in endpoints {
        entries.insert(
            name.to_owned(),
            json!({
                "type": "streamable-http", "url": url,
                "headers": {"Authorization": "HEADER_SENTINEL"},
                "args": ["ARG_SENTINEL"], "env": {"TOKEN": "ENV_SENTINEL"}
            }),
        );
    }
    entries.insert(
        "local".to_owned(),
        json!({"command": " node ", "args": ["ARG_SENTINEL"]}),
    );
    entries.insert("malformed-scalar".to_owned(), json!(false));
    entries.insert("malformed-array".to_owned(), json!([]));
    entries.insert(
        "malformed-fields".to_owned(),
        json!({"command": 7, "url": false, "type": []}),
    );
    entries.insert("malformed-empty".to_owned(), json!({}));

    let json = serde_json::to_vec(&json!({"mcpServers": entries})).unwrap();
    let toml = toml::to_string(&json!({"mcp_servers": entries})).unwrap();
    let sources = [
        ("codex", ".codex/config.toml", toml.as_bytes()),
        ("claude-code", ".claude.json", json.as_slice()),
        (
            "claude-desktop",
            ".config/Claude/claude_desktop_config.json",
            json.as_slice(),
        ),
        ("vscode", ".copilot/mcp-config.json", json.as_slice()),
    ];
    for (_, relative, contents) in sources {
        fixture.write(&format!("home/{relative}"), contents);
        fixture.write(&format!("other/{relative}"), contents);
    }
    let context = fixture.context().with_home(fixture.root.join("other"));
    let agents = discover_harnesses(&context);
    assert_eq!(agents.len(), 4);
    for agent in &agents {
        let (_, relative, _) = sources
            .iter()
            .find(|(kind, _, _)| *kind == agent.kind)
            .unwrap();
        let expected_sources = [
            fixture.root.join("home").join(relative),
            fixture.root.join("other").join(relative),
        ];
        let servers = &agent.mcp_servers;
        assert_eq!(servers.len(), 2 * (endpoints.len() + 1), "{}", agent.kind);
        let source_order = servers
            .iter()
            .map(|server| &server.source)
            .collect::<Vec<_>>();
        assert!(source_order.windows(2).all(|pair| pair[0] <= pair[1]));
        for (name, _, origin) in endpoints {
            let registrations = servers
                .iter()
                .filter(|server| server.name == name)
                .collect::<Vec<_>>();
            assert_eq!(registrations.len(), 2);
            for (server, source) in registrations.iter().zip(&expected_sources) {
                assert_eq!(&server.source, source);
                assert_eq!(server.url.as_deref(), origin, "{}: {name}", agent.kind);
                assert_eq!(server.transport, "http");
                assert_eq!(server.command, None);
                assert!(server.enabled);
                if origin.is_none() {
                    assert!(serde_json::to_value(server).unwrap().get("url").is_none());
                }
            }
        }
        let locals = servers
            .iter()
            .filter(|server| server.name == "local")
            .collect::<Vec<_>>();
        assert_eq!(locals.len(), 2);
        for (server, source) in locals.iter().zip(&expected_sources) {
            assert_eq!(&server.source, source);
            assert_eq!(server.command.as_deref(), Some(" node "));
            assert_eq!(server.transport, "stdio");
            assert_eq!(server.url, None);
        }
    }
    let serialized = serde_json::to_string(&agents).unwrap();
    assert!(!serialized.contains("SENTINEL"));
    for field in ["\"args\"", "\"env\"", "\"headers\""] {
        assert!(!serialized.contains(field));
    }
}

#[test]
fn whole_adapters_preserve_native_enablement_semantics() {
    let fixture = Fixture::new();
    for name in ["codex", "claude", "claude-desktop", "code"] {
        fixture.install(name);
    }
    for (flags, claude, vscode, codex) in [
        (json!({}), true, true, true),
        (json!({"enabled": false}), true, false, false),
        (json!({"disabled": true}), false, false, true),
        (
            json!({"disabled": true, "enabled": true}),
            false,
            false,
            true,
        ),
        (
            json!({"disabled": false, "enabled": false}),
            true,
            false,
            false,
        ),
        (
            json!({"disabled": "true", "enabled": "false"}),
            true,
            true,
            true,
        ),
        (json!({"disabled": null, "enabled": null}), true, true, true),
    ] {
        let mut entry = flags;
        entry["command"] = json!("cmd");
        fixture.json(
            "home/.claude.json",
            json!({"mcpServers": {"server": entry}}),
        );
        fixture.json(
            "home/.config/Claude/claude_desktop_config.json",
            json!({"mcpServers": {"server": entry}}),
        );
        fixture.json(
            "project/.vscode/mcp.json",
            json!({"servers": {"server": entry}}),
        );
        // TOML has no null values; missing flags have the corresponding native default.
        entry
            .as_object_mut()
            .unwrap()
            .retain(|_, value| !value.is_null());
        fixture.write(
            "home/.codex/config.toml",
            toml::to_string(&json!({"mcp_servers": {"server": entry}})).unwrap(),
        );
        let agents = discover_harnesses(&fixture.context());
        assert_eq!(agents.len(), 4);
        for agent in agents {
            assert_eq!(agent.mcp_servers.len(), 1);
            let server = &agent.mcp_servers[0];
            let expected = match agent.kind.as_str() {
                "codex" => codex,
                "vscode" => vscode,
                "claude-code" | "claude-desktop" => claude,
                other => panic!("unexpected harness {other}"),
            };
            assert_eq!(server.name, "server");
            assert_eq!(server.command.as_deref(), Some("cmd"));
            assert_eq!(server.enabled, expected, "{}: {entry}", agent.kind);
        }
    }
}

#[test]
fn codex_override_preserves_other_users_and_source_provenance() {
    let fixture = Fixture::new();
    fixture.install("codex");
    let mut expected = Vec::new();
    for (relative, command) in [
        ("home/.codex/config.toml", "default"),
        ("other/.codex/config.toml", "other-user"),
        ("override/config.toml", "override"),
        ("project/.codex/config.toml", "project"),
    ] {
        expected.push(fixture.write(
            relative,
            format!("[mcp_servers.same]\ncommand = '{command}'\n"),
        ));
    }
    expected.sort();
    let context = fixture
        .context()
        .with_home(fixture.root.join("other"))
        .with_override("CODEX_HOME", fixture.root.join("override"));
    let agents = discover_harnesses(&context);
    let servers = &agents[0].mcp_servers;
    assert_eq!(
        servers
            .iter()
            .map(|server| server.source.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(servers.iter().all(|server| server.name == "same"));
}

#[test]
fn malformed_files_and_entries_do_not_hide_valid_sources() {
    let fixture = Fixture::new();
    fixture.install("code");
    fixture.install("claude");
    fixture.write("home/.claude.json", "{ broken");
    fixture.write("project/nested/.vscode/mcp.json", "{ broken");
    let portable = fixture.json(
        "home/.copilot/mcp-config.json",
        json!({"mcpServers":{
            "bad":null,"bad-url":{"url":"file:///private"},"local":{"command":"node"}
        }}),
    );
    let expected = fixture.json(
        "project/.vscode/mcp.json",
        json!({"servers":{"parent":{"command":"npx"}}}),
    );
    let agents = discover_harnesses(&fixture.context());
    assert_eq!(agents.len(), 2);
    assert!(agents[0].mcp_servers.is_empty());
    assert_eq!(agents[1].mcp_servers.len(), 3);
    let native = agents[1]
        .mcp_servers
        .iter()
        .find(|server| server.name == "bad-url")
        .unwrap();
    assert_eq!(native.url, None);
    assert_eq!(native.transport, "http");
    assert_eq!(native.source, portable);
    assert!(
        agents[1]
            .mcp_servers
            .iter()
            .any(|server| server.name == "local" && server.source == portable)
    );
    assert!(
        agents[1]
            .mcp_servers
            .iter()
            .all(|server| server.name != "bad")
    );
    assert!(
        agents[1]
            .mcp_servers
            .iter()
            .any(|server| server.source == expected)
    );
}

#[test]
fn overlapping_skill_roots_and_symlink_cycles_are_safe() {
    let fixture = Fixture::new();
    fixture.install("claude");
    let directory = fixture.root.join("home/.claude/skills/example");
    let skill = fixture.write(
        "home/.claude/skills/example/SKILL.md",
        "---\nname: example\n---\nbody",
    );
    fixture.json(
        "home/.claude/plugins/installed_plugins.json",
        json!({"plugins":{"example":[{"installPath":directory}]}}),
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(&directory, directory.join("cycle")).unwrap();
    let agents = discover_harnesses(&fixture.context());
    assert_eq!(agents[0].skills.len(), 1);
    assert_eq!(agents[0].skills[0].path, skill);
    assert_eq!(agents[0].skills[0].front_matter["name"], "example");
}

#[test]
fn executable_identity_can_be_filtered_without_running_candidates() {
    let fixture = Fixture::new();
    let first = fixture.install("tool");
    let second = fixture.write("fallback/tool", "native release");
    let context = fixture.context();
    let accepted = context
        .executable_candidates("tool")
        .into_iter()
        .chain([second.clone()])
        .find(|candidate| {
            candidate.is_file()
                && candidate.parent() == Some(fixture.root.join("fallback").as_path())
        });
    assert_eq!(accepted, Some(second));
    assert_eq!(
        context.find_executable("tool", std::iter::empty::<PathBuf>()),
        Some(first)
    );
    assert!(
        context
            .current_dir_ancestors(Path::new(".agents/skills"))
            .iter()
            .all(|path| path.starts_with(&fixture.root))
    );
}

#[test]
fn fixture_versions_do_not_read_ambient_parent_packages() {
    let fixture = Fixture::new();
    fixture.install("codex");
    fixture.install("code");
    fs::write(
        fixture.directory.join("package.json"),
        r#"{"name":"@openai/codex","version":"PARENT_SENTINEL"}"#,
    )
    .unwrap();
    let agents = discover_harnesses(&fixture.context());
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|agent| agent.version.is_none()));
}

#[test]
fn vscode_named_profiles_preserve_duplicate_names_and_native_flags() {
    let fixture = Fixture::new();
    fixture.install("code");
    #[cfg(target_os = "macos")]
    let user = "home/Library/Application Support/Code/User";
    #[cfg(target_os = "linux")]
    let user = "home/.config/Code/User";
    #[cfg(windows)]
    let user = "home/AppData/Roaming/Code/User";
    let mut expected = Vec::new();
    for relative in [
        "mcp.json",
        "profiles/work/mcp.json",
        "profiles/personal/mcp.json",
    ] {
        expected.push(fixture.json(
            &format!("{user}/{relative}"),
            json!({"servers":{"same":{"command":"node","enabled":false}}}),
        ));
    }
    fixture.write(&format!("{user}/profiles/broken/mcp.json"), "invalid json");
    expected.sort();
    let agents = discover_harnesses(&fixture.context());
    let servers = &agents[0].mcp_servers;
    assert_eq!(
        servers
            .iter()
            .map(|server| server.source.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        servers
            .iter()
            .all(|server| !server.enabled && server.name == "same")
    );
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn native_user_installations_are_found_without_path_entries() {
    let fixture = Fixture::new();
    #[cfg(target_os = "macos")]
    let relatives = [
        "home/Applications/Claude.app/Contents/MacOS/Claude",
        "home/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
    ];
    #[cfg(windows)]
    let relatives = [
        "home/AppData/Local/Programs/Claude/Claude.exe",
        "home/AppData/Local/Programs/Microsoft VS Code/bin/code.cmd",
    ];
    for relative in relatives {
        fixture.write(relative, "not executable");
    }
    let agents = discover_harnesses(&fixture.context().with_search_path(Vec::new()));
    assert_eq!(
        agents
            .iter()
            .map(|agent| agent.kind.as_str())
            .collect::<Vec<_>>(),
        ["claude-desktop", "vscode"]
    );
}
