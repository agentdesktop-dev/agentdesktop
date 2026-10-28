use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub const CLIENT_ID: &str = "agentdesktop-integration";
pub const AUDIENCE: &str = "agentgateway-integration";
const KEY_ID: &str = "integration";
const DEVICE_CODE: &str = "integration-device";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A fake OpenID Connect provider for the daemon's device sign-in. It holds
/// each sign-in until the test calls [`Issuer::approve`], then issues ES256
/// access tokens that a gateway can check against its JWKS.
pub struct Issuer {
    pub url: String,
    state: Arc<IssuerState>,
    task: JoinHandle<()>,
}

struct IssuerState {
    url: String,
    key: EcdsaKeyPair,
    approved: AtomicBool,
    issued: AtomicUsize,
}

impl Issuer {
    pub async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let state = Arc::new(IssuerState {
            url: url.clone(),
            key: EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING)
                .map_err(|_| anyhow::anyhow!("generate the issuer signing key"))?,
            approved: AtomicBool::new(false),
            issued: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/device", post(device))
            .route("/token", post(token))
            .route("/keys", get(keys))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "Fixture issuer failed");
            }
        });
        tracing::info!(%url, "Started host OIDC issuer fixture");
        Ok(Self { url, state, task })
    }

    /// Approves the pending device sign-in, as a user would in a browser.
    pub fn approve(&self) {
        self.state.approved.store(true, Ordering::SeqCst);
    }

    /// The number of access tokens issued to the daemon.
    pub fn issued(&self) -> usize {
        self.state.issued.load(Ordering::SeqCst)
    }

    /// A valid access token, issued directly to the test.
    pub fn token(&self) -> String {
        self.state.token()
    }
}

impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl IssuerState {
    fn token(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let header = json!({ "alg": "ES256", "typ": "JWT", "kid": KEY_ID });
        let claims = json!({
            "iss": self.url, "sub": "integration-user", "aud": AUDIENCE,
            "iat": now, "exp": now + 3600,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self
            .key
            .sign(&SystemRandom::new(), signed.as_bytes())
            .expect("sign the fixture token");
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

async fn discovery(State(state): State<Arc<IssuerState>>) -> Json<Value> {
    let url = &state.url;
    Json(json!({
        "issuer": url,
        "authorization_endpoint": format!("{url}/authorize"),
        "token_endpoint": format!("{url}/token"),
        "device_authorization_endpoint": format!("{url}/device"),
        "jwks_uri": format!("{url}/keys"),
    }))
}

async fn device(State(state): State<Arc<IssuerState>>) -> Json<Value> {
    Json(json!({
        "device_code": DEVICE_CODE,
        "user_code": "TEST-CODE",
        "verification_uri": format!("{}/activate", state.url),
        "expires_in": 600,
        "interval": 1,
    }))
}

async fn token(State(state): State<Arc<IssuerState>>, body: Bytes) -> Response {
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if field("grant_type") != Some(DEVICE_GRANT)
        || field("device_code") != Some(DEVICE_CODE)
        || field("client_id") != Some(CLIENT_ID)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    }
    if !state.approved.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "authorization_pending" })),
        )
            .into_response();
    }
    state.issued.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "access_token": state.token(),
        "token_type": "Bearer",
        "expires_in": 3600,
    }))
    .into_response()
}

async fn keys(State(state): State<Arc<IssuerState>>) -> Json<Value> {
    // An uncompressed P-256 point: 0x04, then x and y.
    let point = state.key.public_key().as_ref();
    Json(json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": KEY_ID,
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    }] }))
}
