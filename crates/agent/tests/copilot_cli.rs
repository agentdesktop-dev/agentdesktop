//! Local-only subprocess/IPC contract tests. No installed Copilot, personal home,
//! controller, identity provider, or inference endpoint is contacted.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    process::Command,
    task::JoinHandle,
};

const NATIVE: &str = r#"#!/bin/sh
if [ "$1" = "--binary-version" ]; then
    printf 'Copilot binary version: %s\n' "${FIXTURE_VERSION:-1.0.85}"
  exit 0
fi
[ "$COPILOT_PROVIDER_BASE_URL" = 'https://gateway.invalid/base/v1' ] || exit 81
[ "$COPILOT_PROVIDER_TYPE" = openai ] || exit 82
[ "$COPILOT_MODEL" = gpt-5.2 ] || exit 83
[ "$COPILOT_PROVIDER_WIRE_API" = responses ] || exit 84
[ "$COPILOT_PROVIDER_TRANSPORT" = http ] || exit 85
[ -z "${COPILOT_PROVIDER_API_KEY+x}${COPILOT_PROVIDER_BEARER_TOKEN+x}${HEADERS+x}${OPENAI_API_KEY+x}${COPILOT_MODEL_METADATA+x}" ] || exit 86
[ "${GITHUB_TOKEN:-}" = "${FIXTURE_GITHUB_TOKEN:-}" ] || exit 93
[ "$(/bin/cat "$COPILOT_PROVIDERS_CONFIG")" = '{"providers":[],"models":[]}' ] || exit 87
printf '%s' "$COPILOT_PROVIDERS_CONFIG" > "$FIXTURE_REGISTRY"
for argument do printf '%s\n' "$argument"; done > "$FIXTURE_ARGS"
if [ "${FIXTURE_SIGNAL:-}" = 'wait' ]; then
    trap 'exit 42' TERM
    printf '%s\n' 'native ready'
    IFS= read -r input
    exit 92
fi
if [ "${FIXTURE_SIGNAL:-}" = 'self' ]; then
    kill -TERM $$
fi
IFS= read -r input
[ "$input" = 'native stdin preserved' ] || exit 88
first=$(/bin/sh -c "$COPILOT_PROVIDER_API_KEY_COMMAND") || exit 89
second=$(/bin/sh -c "$COPILOT_PROVIDER_API_KEY_COMMAND") || exit 90
[ "$first" = fixture-token-2 ] && [ "$second" = fixture-token-3 ] || exit 91
printf '%s\n' 'native credential refresh verified'
printf '%s\n' 'native stderr preserved' >&2
exit 37
"#;

struct Fixture {
    root: TempDir,
    socket: PathBuf,
    helper: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    credentials: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new(config: Value, credential_error: Option<String>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("ad-copilot-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("socket ' quoted");
        fs::create_dir_all(root.path().join("bin")).unwrap();
        fs::create_dir_all(root.path().join("native-home")).unwrap();
        fs::write(root.path().join("bin/copilot"), NATIVE).unwrap();
        fs::set_permissions(
            root.path().join("bin/copilot"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::write(
            root.path().join("native-home/config.json"),
            b"{\"token\":\"personal-auth-untouched\"}",
        )
        .unwrap();
        fs::write(
            root.path().join("native-home/providers.json"),
            b"{\"providers\":[{\"id\":\"personal-provider-untouched\"}],\"models\":[]}",
        )
        .unwrap();
        // Copy to a quoted path so current_exe() tests real helper quoting, not
        // a test substitute for the Credential command.
        let helper = root.path().join("Agent Desktop's helper");
        fs::copy(env!("CARGO_BIN_EXE_agentdesktop-headless"), &helper).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let credentials = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let count = credentials.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).await.is_err() {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") || request.len() > 8192 {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let uri = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                seen.lock().unwrap().push(uri.clone());
                let (code, body) = match uri.as_str() {
                    "/v1/effective-config" => (200, config.to_string()),
                    "/v1/config" => (200, "{}".to_owned()),
                    "/v1/llm-gateway/credential?client_id=copilot-cli" => {
                        let number = count.fetch_add(1, Ordering::SeqCst) + 1;
                        if let Some(error) = &credential_error {
                            (502, error.clone())
                        } else {
                            (200, json!({"credential":format!("fixture-token-{number}"),"expiresAtUnixSeconds":4102444800_u64}).to_string())
                        }
                    }
                    _ => (404, "not found".to_owned()),
                };
                let response = format!(
                    "HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            root,
            socket,
            helper,
            requests,
            credentials,
            server,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.helper);
        command
            .env_clear()
            .env("PATH", self.root.path().join("bin"))
            .env("HOME", self.root.path())
            .env("COPILOT_HOME", self.root.path().join("native-home"))
            .env(
                "COPILOT_PROVIDERS_CONFIG",
                self.root.path().join("native-home/providers.json"),
            )
            .env("FIXTURE_ARGS", self.root.path().join("arguments"))
            .env("FIXTURE_REGISTRY", self.root.path().join("registry-path"))
            .arg("--socket")
            .arg(&self.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn configuration() -> Value {
    json!({
        "llmGateway": {
            "url": "https://gateway.invalid/base/",
            "authentication": {"type":"oidc", "issuer":"https://identity.invalid", "clientId":"fixture"}
        },
        "programs": {"copilotCli": {"useLlmGateway":true,"model":"gpt-5.2","wireApi":"responses"}}
    })
}

#[tokio::test]
async fn launcher_uses_effective_config_refreshes_credentials_and_preserves_native_process_contract()
 {
    let fixture = Fixture::new(configuration(), None).await;
    let auth = fs::read(fixture.root.path().join("native-home/config.json")).unwrap();
    let providers = fs::read(fixture.root.path().join("native-home/providers.json")).unwrap();
    let args = [
        "-p",
        "literal ' quotes & $(false); %PATH%",
        "--socket",
        "native-not-daemon",
    ];
    let mut command = fixture.command();
    for key in [
        "COPILOT_PROVIDER_API_KEY",
        "COPILOT_PROVIDER_BEARER_TOKEN",
        "HEADERS",
        "GITHUB_TOKEN",
        "OPENAI_API_KEY",
        "COPILOT_MODEL_METADATA",
    ] {
        command.env(key, "INHERITED-SECRET");
    }
    command
        .env("FIXTURE_GITHUB_TOKEN", "INHERITED-SECRET")
        .env("COPILOT_MODEL", "wrong-model")
        .env("COPILOT_PROVIDER_BASE_URL", "https://wrong.invalid")
        .env("COPILOT_PROVIDER_API_KEY_COMMAND", "must never execute")
        .arg("copilot")
        .arg("--")
        .args(args);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"native stdin preserved\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(37),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"native credential refresh verified\n");
    assert_eq!(output.stderr, b"native stderr preserved\n");
    assert_eq!(fixture.credentials.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.requests.lock().unwrap()[0], "/v1/effective-config");
    assert!(
        !fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri == "/v1/config")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("arguments")).unwrap(),
        format!("{}\n", args.join("\n"))
    );
    let registry = fs::read_to_string(fixture.root.path().join("registry-path")).unwrap();
    assert!(
        !PathBuf::from(registry).exists(),
        "temporary registry must be cleaned up after nonzero exit"
    );
    assert_eq!(
        fs::read(fixture.root.path().join("native-home/config.json")).unwrap(),
        auth
    );
    assert_eq!(
        fs::read(fixture.root.path().join("native-home/providers.json")).unwrap(),
        providers
    );
}

#[tokio::test]
async fn rejects_old_native_versions_before_credential_or_native_launch() {
    let fixture = Fixture::new(configuration(), None).await;
    let output = fixture
        .command()
        .env("FIXTURE_VERSION", "1.0.83")
        .arg("copilot")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("1.0.84"));
    assert_eq!(fixture.credentials.load(Ordering::SeqCst), 0);
    assert!(!fixture.root.path().join("arguments").exists());
}

#[tokio::test]
async fn rejects_disabled_and_disallowed_routing_without_widening_policy() {
    let mut disabled = configuration();
    disabled["programs"]["copilotCli"]["useLlmGateway"] = json!(false);
    let mut disallowed = configuration();
    disallowed["llmGateway"]["authentication"] = json!({
        "type":"controllerJwt", "audience":"gateway", "allowedClientIds":["claude-code"]
    });
    for (config, message) in [
        (disabled, "enable programs.copilotCli"),
        (disallowed, "explicitly include copilot-cli"),
    ] {
        let fixture = Fixture::new(config, None).await;
        let output = fixture.command().arg("copilot").output().await.unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        assert_eq!(fixture.credentials.load(Ordering::SeqCst), 0);
        assert!(!fixture.root.path().join("arguments").exists());
    }
}

#[tokio::test]
async fn preflight_and_credential_helper_do_not_print_daemon_error_bodies() {
    let fixture = Fixture::new(
        configuration(),
        Some("DAEMON-SECRET-DO-NOT-PRINT".to_owned()),
    )
    .await;
    for args in [
        vec!["copilot"],
        vec!["credential", "--client-id", "copilot-cli"],
    ] {
        let output = fixture.command().args(args).output().await.unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("credential unavailable"));
        assert!(!error.contains("DAEMON-SECRET"));
    }
    assert!(!fixture.root.path().join("arguments").exists());
}

#[tokio::test]
async fn signals_reach_native_child_and_cleanup_preserves_its_exit_status() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let fixture = Fixture::new(configuration(), None).await;
    let mut child = fixture
        .command()
        .env("FIXTURE_SIGNAL", "wait")
        .arg("copilot")
        .spawn()
        .unwrap();
    // Child::wait closes its own stdin handle. Hold it separately so the fake
    // native process cannot exit from EOF before the signal is forwarded.
    let _stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "native ready\n");
    // Signal only Agentdesktop; it must forward to its own native child.
    let pid = child.id().unwrap() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let result = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.code(), Some(42));
    let registry = fs::read_to_string(fixture.root.path().join("registry-path")).unwrap();
    assert!(!PathBuf::from(registry).exists());
}

#[tokio::test]
async fn native_signal_exit_is_reported_without_a_launcher_error_message() {
    let fixture = Fixture::new(configuration(), None).await;
    let output = fixture
        .command()
        .env("FIXTURE_SIGNAL", "self")
        .arg("copilot")
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM));
    assert!(output.stderr.is_empty());
    let registry = fs::read_to_string(fixture.root.path().join("registry-path")).unwrap();
    assert!(!PathBuf::from(registry).exists());
}
