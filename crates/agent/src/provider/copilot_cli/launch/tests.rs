use super::*;

#[test]
fn gates_native_credential_command_support_at_stable_1_0_84() {
    for version in ["0.0.420", "1.0.83", "1.0.84-beta.1", "1.0.85-rc.1"] {
        let error = require_supported_version(&Version::parse(version).unwrap()).unwrap_err();
        assert!(error.to_string().contains("1.0.84"));
    }
    for version in ["1.0.84", "1.0.85", "1.1.0", "2.0.0"] {
        require_supported_version(&Version::parse(version).unwrap()).unwrap();
    }
}

#[test]
fn removes_credential_and_routing_overrides_without_changing_native_home() {
    for key in [
        "COPILOT_PROVIDER_API_KEY",
        "COPILOT_PROVIDER_BEARER_TOKEN",
        "COPILOT_PROVIDER_API_KEY_COMMAND",
        "COPILOT_PROVIDER_HEADERS",
        "COPILOT_PROVIDERS_CONFIG",
        "COPILOT_MODEL",
        "COPILOT_MODEL_METADATA",
        "COPILOT_MODEL_CONTEXT_WINDOW",
        "COPILOT_PROVIDER_TRANSPORT",
        "COPILOT_PROVIDER_BASE_URL",
        "COPILOT_PROVIDER_WIRE_API",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "GITHUB_COPILOT_API_TOKEN",
        "API_KEY",
        "API_KEY_COMMAND",
        "BEARER",
        "AUTHORIZATION",
        "HEADERS",
        "OPENAI_BASE_URL",
        "COPILOT_BYOK_CONFIG",
        "copilot_provider_headers",
    ] {
        assert!(
            overrides_routing_or_credentials(OsStr::new(key)),
            "must clear {key}"
        );
    }
    for key in [
        "COPILOT_HOME",
        "HOME",
        "USERPROFILE",
        "PATH",
        "TERM",
        "NO_COLOR",
        "TMPDIR",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "COPILOT_GITHUB_TOKEN",
        "AWS_SESSION_TOKEN",
        "SERVICE_API_KEY",
        "NODE_OPTIONS",
    ] {
        assert!(
            !overrides_routing_or_credentials(OsStr::new(key)),
            "must preserve {key}"
        );
    }
    let mut command = Command::new("unused-test-executable");
    command
        .env("COPILOT_HOME", "keep")
        .env("HEADERS", "must-not-survive");
    scrub_environment(&mut command, ["COPILOT_HOME".into(), "HEADERS".into()]);
    let env = command.as_std().get_envs().collect::<Vec<_>>();
    assert!(env.contains(&(OsStr::new("COPILOT_HOME"), Some(OsStr::new("keep")))));
    assert!(env.contains(&(OsStr::new("HEADERS"), None)));
}

#[test]
fn private_registry_is_empty_restricted_and_removed_after_use() {
    let directory = private_registry().unwrap();
    let path = directory.path().join("providers.json");
    assert_eq!(std::fs::read(&path).unwrap(), EMPTY_REGISTRY);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(directory);
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn helper_command_quotes_socket_and_executable_without_interpolation() {
    let command = credential_command(
        Path::new("/Applications/Agent's Desktop/agentdesktop"),
        Path::new("/tmp/socket ' ; $(false)"),
    )
    .unwrap();
    assert_eq!(
        command,
        "'/Applications/Agent'\\''s Desktop/agentdesktop' '--socket' '/tmp/socket '\\'' ; $(false)' 'credential' '--client-id' 'copilot-cli'"
    );
}

#[test]
fn native_executable_arguments_are_not_shell_strings() {
    let args = ["--model".into(), "a b' ; $(false) %PATH% &".into()];
    let command = native_command(Path::new("copilot.exe"), &args).unwrap();
    assert_eq!(command.as_std().get_program(), "copilot.exe");
    assert_eq!(
        command.as_std().get_args().collect::<Vec<_>>(),
        args.iter().map(OsString::as_os_str).collect::<Vec<_>>()
    );
}

#[test]
fn windows_credential_quoting_uses_encoded_literal_arguments() {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let command = CommandSpec::new(
        Path::new(r"C:\Agent's Desktop\copilot.cmd"),
        [
            "--prompt",
            "a 'quote' ; $(false) & %PATH%",
            "--socket",
            r"\\.\pipe\a b",
        ],
    );
    let wrapper = crate::provider::shared::windows_command(&command);
    assert_eq!(wrapper.program, "powershell.exe");
    let bytes = BASE64_STANDARD
        .decode(wrapper.args.last().unwrap())
        .unwrap();
    let utf16 = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|value| u16::from_le_bytes(*value))
        .collect::<Vec<_>>();
    let script = String::from_utf16(&utf16).unwrap();
    assert_eq!(
        script,
        r"& 'C:\Agent''s Desktop\copilot.cmd' '--prompt' 'a ''quote'' ; $(false) & %PATH%' '--socket' '\\.\pipe\a b'; exit $LASTEXITCODE"
    );
}

#[test]
fn windows_npm_shim_is_resolved_without_sending_arguments_through_cmd() {
    let directory = tempfile::tempdir().unwrap();
    let package = directory.path().join("node_modules/@github/copilot");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@github/copilot","bin":{"copilot":"index.js"}}"#,
    )
    .unwrap();
    std::fs::write(package.join("index.js"), "// not executed").unwrap();
    std::fs::write(directory.path().join("node.exe"), "not executed").unwrap();
    let shim = directory.path().join("copilot.cmd");
    let args = ["--prompt".into(), "\" & %PATH% !NAME! $(false)".into()];
    let command = npm_command(&shim, &args).unwrap();
    assert_eq!(
        command.as_std().get_program(),
        directory.path().join("node.exe")
    );
    let actual = command.as_std().get_args().collect::<Vec<_>>();
    assert_eq!(actual[0], package.join("index.js").canonicalize().unwrap());
    assert_eq!(
        actual[1..],
        args.iter().map(OsString::as_os_str).collect::<Vec<_>>()
    );

    std::fs::write(directory.path().join("outside.js"), "// outside package").unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@github/copilot","bin":{"copilot":"../../../outside.js"}}"#,
    )
    .unwrap();
    assert!(npm_command(&shim, &args).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn credential_timeout_is_bounded_even_when_daemon_accepts_but_never_replies() {
    let directory = tempfile::Builder::new()
        .prefix("ad-cpt-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = directory.path().join("s");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let hold = tokio::spawn(async move {
        let _connection = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let error = credential_with_timeout(&socket, Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    hold.abort();
}
