use std::process::Command;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[test]
fn help_exits_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["serve", "connect-agents", "identity", "capture", "launch"] {
        assert!(help.contains(command));
    }

    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--mode <MODE>"));
    assert!(help.contains("--upstream <UPSTREAM>"));

    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .args(["launch", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--skip-preflight")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn launch_requires_a_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .arg("launch")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("<COMMAND>...")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn launch_child_waits_for_explicit_release() {
    let temporary = tempfile::tempdir().unwrap();
    let marker = temporary.path().join("executed");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .args([
            "_launch-child",
            "--gate-directory",
            temporary.path().to_str().unwrap(),
            "--controller-pid",
            &std::process::id().to_string(),
            "--",
            "/usr/bin/touch",
            marker.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while !temporary.path().join("ready").is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(temporary.path().join("ready").is_file());
    assert!(!marker.exists());

    std::fs::write(temporary.path().join("release"), "").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(marker.is_file());
}

#[cfg(unix)]
#[test]
fn identity_storage_check_selects_protected_file() {
    let temporary = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
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
