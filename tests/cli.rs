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
fn claude_adapter_sets_selected_path_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-claude"))
        .args([
            "--path",
            "native",
            "--base-url",
            "http://localhost:4444/anthropic/",
            "--claude-binary",
            "/bin/sh",
            "--",
            "-c",
            "printf '%s\\n%s' \"$ANTHROPIC_BASE_URL\" \"$ANTHROPIC_API_KEY\"",
        ])
        .env("AGENTGATEWAY_EDGE_CLAUDE_CREDENTIAL", "test-placeholder")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "http://localhost:4444/anthropic/\ntest-placeholder"
    );
}

#[cfg(unix)]
#[test]
fn identity_storage_check_selects_protected_file() {
    let temporary = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-identity"))
        .args([
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

#[test]
fn claude_adapter_rejects_unavailable_capture_before_launch() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-claude"))
        .args([
            "--path",
            "captured",
            "--claude-binary",
            "/path/that/must/not/run",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("captured path is unavailable")
    );
}
