use std::fs;
use std::process::Command;

#[test]
fn installs_upgrades_and_uninstalls_standalone_bundle() {
    let temporary = tempfile::tempdir().unwrap();
    let fixtures = temporary.path().join("fixtures");
    fs::create_dir(&fixtures).unwrap();
    for name in ["connector", "agentgateway", "config"] {
        fs::write(fixtures.join(name), format!("first-{name}")).unwrap();
    }
    let root = temporary.path().join("agentdesktop");
    let command_link = temporary.path().join("bin/agentdesktop");
    let runtime_config = temporary.path().join("config/agentgateway.yaml");
    let install = || {
        Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
            .args([
                "install",
                "--root",
                root.to_str().unwrap(),
                "--connector",
                fixtures.join("connector").to_str().unwrap(),
                "--agentgateway",
                fixtures.join("agentgateway").to_str().unwrap(),
                "--starter-config",
                fixtures.join("config").to_str().unwrap(),
                "--control",
                env!("CARGO_BIN_EXE_agentdesktop-install"),
                "--gateway-config",
                runtime_config.to_str().unwrap(),
                "--command-link",
                command_link.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };

    assert!(install().status.success());
    assert_eq!(
        fs::read_to_string(root.join("bin/agentdesktop")).unwrap(),
        "first-connector"
    );
    assert_eq!(
        fs::read_to_string(root.join("share/examples/agentgateway.yaml")).unwrap(),
        "first-config"
    );
    assert!(root.join("bin/agentdesktop-install").is_file());
    assert_eq!(
        fs::read_link(&command_link).unwrap(),
        root.join("bin/agentdesktop")
    );
    let service = fs::read_to_string(root.join("share/systemd/user/agentdesktop.service")).unwrap();
    assert!(service.contains("serve --mode standalone"));
    assert!(service.contains("--gateway-binary"));
    assert!(service.contains("NoNewPrivileges=true"));
    assert!(service.contains(root.to_str().unwrap()));
    assert!(service.contains(runtime_config.to_str().unwrap()));

    fs::write(fixtures.join("connector"), "second-connector").unwrap();
    assert!(install().status.success());
    assert_eq!(
        fs::read_to_string(root.join("bin/agentdesktop")).unwrap(),
        "second-connector"
    );

    let verify = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args(["verify", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(verify.status.success());

    #[cfg(unix)]
    {
        fs::remove_file(&command_link).unwrap();
        std::os::unix::fs::symlink("unexpected-target", &command_link).unwrap();
        let refused = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
            .args(["verify", "--root", root.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!refused.status.success());
        fs::remove_file(&command_link).unwrap();
        std::os::unix::fs::symlink(root.join("bin/agentdesktop"), &command_link).unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let systemctl = temporary.path().join("systemctl");
        let systemctl_log = temporary.path().join("systemctl.log");
        fs::write(
            &systemctl,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SYSTEMCTL_LOG\"\n",
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        for action in ["enable", "disable"] {
            let service = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
                .args([
                    "service",
                    action,
                    "--root",
                    root.to_str().unwrap(),
                    "--systemctl",
                    systemctl.to_str().unwrap(),
                ])
                .env("SYSTEMCTL_LOG", &systemctl_log)
                .output()
                .unwrap();
            assert!(service.status.success());
        }
        let invocations = fs::read_to_string(systemctl_log).unwrap();
        assert_eq!(
            invocations,
            format!(
                "--user enable {}\n--user restart agentdesktop.service\n--user disable --now agentdesktop.service\n",
                root.join("share/systemd/user/agentdesktop.service")
                    .display()
            )
        );
    }

    fs::write(root.join("bin/agentdesktop"), "locally-modified").unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(root.exists());
    fs::write(root.join("bin/agentdesktop"), "second-connector").unwrap();

    let uninstall = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(uninstall.status.success());
    assert!(!root.exists());
    assert!(!command_link.exists());
}

#[test]
fn refuses_to_remove_an_unowned_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("not-an-install");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("user-data"), "keep").unwrap();

    let uninstall = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args(["uninstall", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!uninstall.status.success());
    assert_eq!(fs::read_to_string(root.join("user-data")).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn refuses_to_replace_an_unowned_stable_command() {
    let temporary = tempfile::tempdir().unwrap();
    let fixtures = temporary.path().join("fixtures");
    fs::create_dir(&fixtures).unwrap();
    for name in ["connector", "agentgateway", "config"] {
        fs::write(fixtures.join(name), name).unwrap();
    }
    let root = temporary.path().join("agentdesktop");
    let command_link = temporary.path().join("bin/agentdesktop");
    fs::create_dir(command_link.parent().unwrap()).unwrap();
    fs::write(&command_link, "user-owned command").unwrap();

    let install = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args([
            "install",
            "--root",
            root.to_str().unwrap(),
            "--connector",
            fixtures.join("connector").to_str().unwrap(),
            "--agentgateway",
            fixtures.join("agentgateway").to_str().unwrap(),
            "--starter-config",
            fixtures.join("config").to_str().unwrap(),
            "--command-link",
            command_link.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!install.status.success());
    assert_eq!(
        fs::read_to_string(command_link).unwrap(),
        "user-owned command"
    );
    assert!(!root.exists());
}

#[test]
fn installs_managed_bundle_without_local_gateway() {
    let temporary = tempfile::tempdir().unwrap();
    let fixtures = temporary.path().join("fixtures");
    fs::create_dir(&fixtures).unwrap();
    fs::write(fixtures.join("connector"), "managed-connector").unwrap();
    let organization = fixtures.join("organization.json");
    fs::write(
        &organization,
        br#"{
          "format_version": 1,
          "organization": {"id":"acme","display_name":"Acme","support_url":"https://help.acme.example/"},
          "identity": {"issuer":"https://login.acme.example/","enrollment_url":"https://enrollment.acme.example/","client_id":"agentdesktop","audience":"gateway","scope":"invoke"},
          "gateway": {"url":"https://gateway.acme.example/"}
        }"#,
    )
    .unwrap();
    let root = temporary.path().join("agentdesktop");
    let command_link = temporary.path().join("bin/agentdesktop");

    let install = Command::new(env!("CARGO_BIN_EXE_agentdesktop-install"))
        .args([
            "managed-install",
            "--root",
            root.to_str().unwrap(),
            "--connector",
            fixtures.join("connector").to_str().unwrap(),
            "--organization",
            organization.to_str().unwrap(),
            "--control",
            env!("CARGO_BIN_EXE_agentdesktop-install"),
            "--command-link",
            command_link.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(install.status.success(), "{:?}", install.stderr);
    assert!(root.join("share/organization.json").is_file());
    assert_eq!(
        fs::read_link(command_link).unwrap(),
        root.join("bin/agentdesktop")
    );
    assert!(!root.join("bin/agentgateway").exists());
    assert!(!root.join("share/examples/agentgateway.yaml").exists());
    let service = fs::read_to_string(root.join("share/systemd/user/agentdesktop.service")).unwrap();
    assert!(service.contains("serve --mode managed"));
    assert!(service.contains("--upstream \"https://gateway.acme.example/\""));
    assert!(service.contains("--identity-issuer \"https://login.acme.example/\""));
    assert!(service.contains("--enrollment-url \"https://enrollment.acme.example/\""));
    assert!(!service.contains("--gateway-binary"));
}
