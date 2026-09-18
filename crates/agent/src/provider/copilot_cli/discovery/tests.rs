use super::*;

fn executable(root: &Path) -> PathBuf {
    let executable = root.join("bin/copilot");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn discovers_only_safe_metadata_in_isolated_roots_without_running_copilot() {
    let root = tempfile::tempdir().unwrap();
    let executable = executable(root.path());
    write(
        root.path(),
        "package.json",
        r#"{"name":"@github/copilot","version":"1.0.85"}"#,
    );
    write(
        root.path(),
        "copilot/config.json",
        r#"{"token":"AUTH-SECRET","mcpServers":{"not-in-inventory":{"command":"secret-command"}}}"#,
    );
    write(
        root.path(),
        "copilot/mcp-config.json",
        r#"{
        "mcpServers": {
            "remote": {"type":"http", "url":"https://name:URL-SECRET@example.com/path-token?key=QUERY-SECRET#fragment", "headers":{"Authorization":"HEADER-SECRET"}},
            "local": {"type":"local", "command":"npx", "args":["ARG-SECRET"], "env":{"TOKEN":"ENV-SECRET"}, "disabled":true}
        }
    }"#,
    );
    write(
        root.path(),
        "copilot/skills/one/SKILL.md",
        "---\nname: one\ndescription: Safe skill metadata\n---\nBODY-SECRET",
    );
    let context = ScanContext {
        executables: vec![executable.clone()],
        config_roots: vec![root.path().join("copilot")],
        boundary: Some(root.path().to_owned()),
    };
    let agent = discover_in(&context).unwrap();
    assert_eq!(agent.kind, "copilot-cli");
    assert_eq!(agent.version.as_deref(), Some("1.0.85"));
    assert_eq!(agent.executable, executable);
    assert_eq!(agent.mcp_servers.len(), 2);
    let remote = agent
        .mcp_servers
        .iter()
        .find(|server| server.name == "remote")
        .unwrap();
    assert_eq!(remote.url.as_deref(), Some("https://example.com"));
    let local = agent
        .mcp_servers
        .iter()
        .find(|server| server.name == "local")
        .unwrap();
    assert_eq!(local.transport, "stdio");
    assert!(!local.enabled);
    assert_eq!(agent.skills.len(), 1);
    assert_eq!(agent.skills[0].front_matter["name"], "one");
    let serialized = serde_json::to_string(&agent).unwrap();
    for secret in ["SECRET", "path-token", "not-in-inventory"] {
        assert!(!serialized.contains(secret));
    }
}

#[test]
fn copilot_home_replaces_the_current_users_default_root() {
    let home = PathBuf::from("/isolated/user");
    let custom = PathBuf::from("/isolated/custom");
    assert_eq!(
        config_roots(Some(&home), vec![home.clone()], Some(custom.clone())),
        [custom]
    );
    assert_eq!(
        config_roots(Some(&home), vec![home.clone()], None),
        [home.join(".copilot")]
    );
}

#[test]
fn package_probes_cannot_escape_the_isolation_boundary_or_use_an_unrelated_package() {
    let parent = tempfile::tempdir().unwrap();
    write(
        parent.path(),
        "package.json",
        r#"{"name":"@github/copilot","version":"9.9.9"}"#,
    );
    let root = parent.path().join("isolated");
    let executable = executable(&root);
    assert!(package_version(&executable, Some(&root)).is_none());
    write(
        &root,
        "package.json",
        r#"{"name":"unrelated","version":"9.9.9"}"#,
    );
    assert!(package_version(&executable, Some(&root)).is_none());
    write(
        &root,
        "package.json",
        r#"{"name":"@github/copilot","version":"1.0.85"}"#,
    );
    assert_eq!(
        package_version(&executable, Some(&root)),
        Some(Version::new(1, 0, 85))
    );
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_read_outside_roots_or_alias_auth_data() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    let executable = executable(&root);
    write(
        parent.path(),
        "outside/SKILL.md",
        "---\nname: outside\n---\n",
    );
    write(
        &root,
        "copilot/config.json",
        r#"{"mcpServers":{"auth-secret":{"command":"secret"}}}"#,
    );
    fs::create_dir_all(root.join("copilot/skills")).unwrap();
    symlink(
        parent.path().join("outside"),
        root.join("copilot/skills/escape"),
    )
    .unwrap();
    symlink(
        root.join("copilot/config.json"),
        root.join("copilot/mcp-config.json"),
    )
    .unwrap();
    let context = ScanContext {
        executables: vec![executable],
        config_roots: vec![root.join("copilot")],
        boundary: Some(root.clone()),
    };
    let agent = discover_in(&context).unwrap();
    assert!(agent.skills.is_empty());
    assert!(agent.mcp_servers.is_empty());
    symlink(
        parent.path().join("package.json"),
        root.join("package.json"),
    )
    .unwrap();
    write(
        parent.path(),
        "package.json",
        r#"{"name":"@github/copilot","version":"9.9.9"}"#,
    );
    assert!(discover_in(&context).unwrap().version.is_none());
}

#[test]
fn missing_executable_is_not_discovered_from_config_alone() {
    let root = tempfile::tempdir().unwrap();
    let context = ScanContext {
        executables: vec![root.path().join("missing")],
        config_roots: vec![root.path().join("copilot")],
        boundary: Some(root.path().to_owned()),
    };
    assert!(discover_in(&context).is_none());
}
