use std::fs;
use std::process::Command;

#[test]
fn installs_upgrades_and_uninstalls_standalone_bundle() {
    let temporary = tempfile::tempdir().unwrap();
    let fixtures = temporary.path().join("fixtures");
    fs::create_dir(&fixtures).unwrap();
    for name in ["connector", "identity", "claude", "agentgateway", "config"] {
        fs::write(fixtures.join(name), format!("first-{name}")).unwrap();
    }
    let root = temporary.path().join("agentgateway-edge");
    let install = || {
        Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-install"))
            .args([
                "install",
                "--root",
                root.to_str().unwrap(),
                "--connector",
                fixtures.join("connector").to_str().unwrap(),
                "--identity",
                fixtures.join("identity").to_str().unwrap(),
                "--claude",
                fixtures.join("claude").to_str().unwrap(),
                "--agentgateway",
                fixtures.join("agentgateway").to_str().unwrap(),
                "--starter-config",
                fixtures.join("config").to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };

    assert!(install().status.success());
    assert_eq!(
        fs::read_to_string(root.join("bin/agentgateway-edge-connector")).unwrap(),
        "first-connector"
    );
    assert_eq!(
        fs::read_to_string(root.join("share/examples/agentgateway.yaml")).unwrap(),
        "first-config"
    );

    fs::write(fixtures.join("connector"), "second-connector").unwrap();
    assert!(install().status.success());
    assert_eq!(
        fs::read_to_string(root.join("bin/agentgateway-edge-connector")).unwrap(),
        "second-connector"
    );

    let verify = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-install"))
        .args(["verify", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(verify.status.success());

    fs::write(
        root.join("bin/agentgateway-edge-connector"),
        "locally-modified",
    )
    .unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(root.exists());
    fs::write(
        root.join("bin/agentgateway-edge-connector"),
        "second-connector",
    )
    .unwrap();

    let uninstall = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    assert!(!root.exists());
}

#[test]
fn refuses_to_remove_an_unowned_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("not-an-install");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("user-data"), "keep").unwrap();

    let uninstall = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!uninstall.status.success());
    assert_eq!(fs::read_to_string(root.join("user-data")).unwrap(), "keep");
}
