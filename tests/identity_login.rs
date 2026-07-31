use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};

use agentgateway_edge_connector::identity::dpop::decode_jwt_claims;
use agentgateway_edge_connector::identity::oauth::{LoginConfig, ManagedIdentity, load_session};
use agentgateway_edge_connector::identity::storage::CredentialStore;
use url::Url;

struct FakeIssuer(Child);

impl Drop for FakeIssuer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn browser_pkce_login_persists_dpop_bound_session() {
    let mut issuer = FakeIssuer(
        Command::new("node")
            .arg("tests/fixtures/fake-authorization-server.mjs")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut issuer_output = BufReader::new(issuer.0.stdout.take().unwrap());
    let mut issuer_url = String::new();
    issuer_output.read_line(&mut issuer_url).unwrap();
    let issuer_url = issuer_url.trim();
    assert!(issuer_url.starts_with("http://127.0.0.1:"));

    let temporary = tempfile::tempdir().unwrap();
    let storage_dir = temporary.path().join("identity");
    let mut login = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-identity"))
        .args([
            "login",
            "--issuer",
            issuer_url,
            "--client-id",
            "agentgateway-edge-test",
            "--audience",
            "agentgateway-edge",
            "--scope",
            "agentgateway.invoke",
            "--gateway-origin",
            "https://gateway.example",
            "--credential-storage",
            "file",
            "--storage-dir",
            storage_dir.to_str().unwrap(),
            "--no-open",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut login_output = BufReader::new(login.stdout.take().unwrap());
    let mut authorization_line = String::new();
    login_output.read_line(&mut authorization_line).unwrap();
    let authorization_url = authorization_line
        .trim()
        .strip_prefix("authorization URL: ")
        .expect("identity CLI did not print an authorization URL");

    let callback = reqwest::get(authorization_url).await.unwrap();
    assert!(callback.status().is_success());
    assert!(callback.text().await.unwrap().contains("login complete"));

    let status = login.wait().unwrap();
    let mut remaining_output = String::new();
    login_output.read_to_string(&mut remaining_output).unwrap();
    let mut login_error = String::new();
    login
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut login_error)
        .unwrap();
    assert!(status.success(), "identity login failed: {login_error}");
    assert!(remaining_output.contains("managed login complete"));

    let config = LoginConfig {
        issuer: Url::parse(issuer_url).unwrap(),
        client_id: "agentgateway-edge-test".into(),
        audience: "agentgateway-edge".into(),
        scope: "agentgateway.invoke".into(),
        gateway_origin: Url::parse("https://gateway.example").unwrap(),
    };
    let store = CredentialStore::load(&storage_dir).unwrap();
    let mut session = load_session(&config, &store).unwrap();
    assert!(!session.is_expired().unwrap());
    let claims = decode_jwt_claims(&session.access_token).unwrap();
    assert_eq!(claims["iss"], issuer_url);
    assert_eq!(
        claims["cnf"]["jkt"],
        session.dpop_key().unwrap().thumbprint().unwrap()
    );

    let original_refresh_token = session.refresh_token.clone();
    session.expires_at = 0;
    let identity = ManagedIdentity::new(session, store.clone());
    let (first, second) = tokio::join!(
        identity.credentials("POST", "https://gateway.example/v1/messages"),
        identity.credentials("POST", "https://gateway.example/v1/messages"),
    );
    assert!(!first.unwrap().access_token.is_empty());
    assert!(!second.unwrap().access_token.is_empty());
    let restored = load_session(&config, &store).unwrap();
    assert_ne!(restored.refresh_token, original_refresh_token);
    assert!(!restored.is_expired().unwrap());

    let logout = Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-identity"))
        .args([
            "logout",
            "--issuer",
            issuer_url,
            "--gateway-origin",
            "https://gateway.example",
            "--storage-dir",
            storage_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(logout.status.success(), "{:?}", logout.stderr);
    assert!(load_session(&config, &store).is_err());
}
