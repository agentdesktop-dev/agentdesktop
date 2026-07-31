use std::process::Command;

#[test]
fn help_exits_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--mode <MODE>"));
    assert!(help.contains("--upstream <UPSTREAM>"));
}

#[cfg(unix)]
#[test]
fn identity_storage_check_selects_protected_file() {
    let temporary = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
        .args([
            "identity",
            "storage-check",
            "--credential-storage",
            "file",
            "--storage-dir",
            temporary.path().join("identity").to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("credential storage is ready: file")
    );
}

#[cfg(unix)]
#[test]
fn connector_subcommand_persists_claude_settings() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let binary_directory = home.join(".local/bin");
    std::fs::create_dir_all(&binary_directory).unwrap();
    std::fs::write(binary_directory.join("claude"), "installed").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
        .args(["connect-agents", "--yes"])
        .env("HOME", home)
        .env("PATH", &binary_directory)
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Claude Code connected.\n"
    );
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:8080"
    );
    assert_eq!(
        settings["env"]["ANTHROPIC_API_KEY"],
        "local-gateway-placeholder"
    );
}

#[cfg(unix)]
#[test]
fn connector_subcommand_does_not_treat_eof_as_consent() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path();
    let binary_directory = home.join(".local/bin");
    std::fs::create_dir_all(&binary_directory).unwrap();
    std::fs::write(binary_directory.join("claude"), "installed").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
        .arg("connect-agents")
        .env("HOME", home)
        .env("PATH", &binary_directory)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("No agents were changed.")
    );
    assert!(!home.join(".claude/settings.json").exists());
}
