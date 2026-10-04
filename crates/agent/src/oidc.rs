use std::{net::SocketAddr, sync::Arc, time::Duration};

use agentdesktop_core::config::ControllerConnectionConfig;
use agentdesktop_proto::fleet::{
    BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    fleet_agent_client::FleetAgentClient,
};
use anyhow::{Context, bail};
use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use rcgen::{
    CertificateParams, CertificateSigningRequest, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot};
use tonic::transport::Channel;
use url::Url;

use crate::{
    enrollment::EnrollmentState,
    identity::{Identity, OAuthCredentials},
    remote,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// RFC 8628 section 3.2: clients poll every 5 seconds unless told otherwise.
const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 section 3.5: `slow_down` increases the interval by 5 seconds.
const DEVICE_POLL_SLOW_DOWN: Duration = Duration::from_secs(5);
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const BRAND_MARK_SVG: &str = include_str!("../../../images/mark.svg");

pub(crate) fn page(title: &str, content: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{title} · Agentdesktop</title>
  <style>
        :root {{ color-scheme: light; font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
        * {{ box-sizing: border-box; }}
        body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: #fff; color: #18181b; }}
        main {{ width: min(100% - 40px, 380px); padding: 32px; }}
    .brand-mark {{ width: 48px; height: 48px; margin-bottom: 24px; overflow: hidden; background: #8023c3; border-radius: 8px; }}
    .brand-mark svg {{ display: block; width: 100%; height: 100%; }}
        h1 {{ margin: 0 0 8px; font-size: 20px; line-height: 1.35; font-weight: 600; letter-spacing: -.01em; }}
        p {{ margin: 0; color: #71717a; font-size: 14px; line-height: 1.55; }}
        .steps {{ display: grid; gap: 10px; margin: 24px 0; }}
        .step {{ display: grid; grid-template-columns: 22px 1fr; gap: 11px; padding: 14px; border: 1px solid #e4e4e7; border-radius: 8px; }}
        .check {{ position: relative; width: 20px; height: 20px; margin-top: 1px; border: 1.5px solid #d4d4d8; border-radius: 6px; background: linear-gradient(#fff, #fafafa); box-shadow: inset 0 0 0 1px rgba(255,255,255,.7), 0 1px 2px rgba(24,24,27,.06); }}
        .check.complete {{ border-color: #8023c3; background: linear-gradient(145deg, #9333d1, #7020ad); box-shadow: 0 2px 6px rgba(128,35,195,.24); }}
        .check.complete::after {{ content: ""; position: absolute; left: 6px; top: 3px; width: 5px; height: 9px; border: solid #fff; border-width: 0 2px 2px 0; transform: rotate(45deg); }}
        .check.skipped {{ border-color: #d4d4d8; background: #f4f4f5; }}
        .check.skipped::after {{ content: ""; position: absolute; left: 5px; right: 5px; top: 8px; height: 2px; border-radius: 2px; background: #a1a1aa; }}
        .step strong {{ display: block; margin-bottom: 3px; font-size: 14px; }}
        .step span {{ color: #71717a; font-size: 13px; line-height: 1.45; }}
        .tag {{ float: right; color: #71717a; font-size: 11px; font-weight: 500; text-transform: uppercase; letter-spacing: .04em; }}
        .actions {{ display: flex; align-items: center; gap: 16px; margin-top: 24px; }}
        .button {{ display: inline-block; padding: 10px 15px; border-radius: 7px; background: #8023c3; color: #fff; font-size: 14px; font-weight: 600; text-decoration: none; }}
        .skip {{ color: #52525b; font-size: 14px; text-decoration: none; }}
  </style>
</head>
<body>
  <main>
        <div class="brand-mark" role="img" aria-label="Agentdesktop">{brand_mark}</div>
        <h1>{title}</h1>
        {content}
  </main>
</body>
</html>"##,
        brand_mark = BRAND_MARK_SVG,
    )
}

fn callback_page(title: &str, message: &str) -> String {
    page(title, &format!("<p>{message}</p>"))
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum AuthorizationPage {
    Identity {
        subscription_available: bool,
        github_available: bool,
    },
    Subscription,
}

impl AuthorizationPage {
    fn html(self) -> String {
        match self {
            Self::Identity {
                subscription_available,
                github_available,
            } => page(
                "Connect Agentdesktop",
                &format!(
                    r#"<p>Agentdesktop needs your identity before it can issue credentials to configured agents.</p>
{}{}
<div class="actions"><a class="button" href="/continue">Continue to sign in</a></div>"#,
                    checklist(false, subscription_available, SubscriptionState::Pending),
                    if github_available {
                        "<div class=\"steps\"><div class=\"step\"><span class=\"check\" aria-hidden=\"true\"></span><div><strong>GitHub Copilot</strong><span>Connect your GitHub account after organization sign-in.</span></div></div></div>"
                    } else {
                        ""
                    }
                ),
            ),
            Self::Subscription => page(
                "Connect Agentdesktop",
                &format!(
                    r#"<p>Your organization identity is connected. You can optionally add the model provider subscription configured for this agent.</p>
{}
<div class="actions"><a class="button" href="/continue">Connect subscription</a><a class="skip" href="/skip">Skip</a></div>"#,
                    checklist(true, true, SubscriptionState::Pending)
                ),
            ),
        }
    }

    fn success_html(self) -> String {
        match self {
            Self::Identity {
                github_available: true,
                subscription_available: false,
            } => page(
                "Connect GitHub",
                r#"<p>Organization sign-in is complete. Preparing GitHub authorization…</p>
<script>
async function advance() {
  try {
    const response = await fetch('/flow-ready', { cache: 'no-store' });
    if (response.ok) { location.replace('/'); return; }
  } catch (_) {}
  setTimeout(advance, 400);
}
setTimeout(advance, 400);
</script>"#,
            ),
            Self::Identity {
                subscription_available: true,
                ..
            } => page(
                "Connect Agentdesktop",
                &format!(
                    r#"<p>Organization sign-in is complete. Preparing the optional subscription step…</p>
{}
<script>
const advance = async () => {{
  try {{
    const response = await fetch('http://localhost:51327/flow-ready', {{ cache: 'no-store' }});
    if (response.ok) {{ location.replace('http://localhost:51327/'); return; }}
  }} catch (_) {{}}
  setTimeout(advance, 400);
}};
setTimeout(advance, 400);
</script>"#,
                    checklist(true, true, SubscriptionState::Pending)
                ),
            ),
            Self::Identity {
                subscription_available: false,
                ..
            } => page(
                "Agentdesktop connected",
                &format!(
                    "<p>Organization sign-in is complete. You can close this window.</p>{}",
                    checklist(true, false, SubscriptionState::Pending)
                ),
            ),
            Self::Subscription => page(
                "Agentdesktop connected",
                &format!(
                    "<p>All configured connections are complete. You can close this window.</p>{}",
                    checklist(true, true, SubscriptionState::Connected)
                ),
            ),
        }
    }

    fn skipped_html(self) -> String {
        page(
            "Agentdesktop connected",
            &format!(
                "<p>The optional subscription was skipped. You can close this window.</p>{}",
                checklist(true, true, SubscriptionState::Skipped)
            ),
        )
    }

    fn optional(self) -> bool {
        matches!(self, Self::Subscription)
    }
}

#[derive(Clone, Copy)]
enum SubscriptionState {
    Pending,
    Connected,
    Skipped,
}

fn checklist(
    identity_connected: bool,
    subscription_available: bool,
    subscription_state: SubscriptionState,
) -> String {
    let identity_state = if identity_connected { " complete" } else { "" };
    let subscription = if subscription_available {
        let (state, status, description) = match subscription_state {
            SubscriptionState::Pending => (
                "",
                "Optional",
                "Connect the model provider subscription configured for this agent.",
            ),
            SubscriptionState::Connected => (
                " complete",
                "Complete",
                "The configured model provider subscription is connected.",
            ),
            SubscriptionState::Skipped => (
                " skipped",
                "Skipped",
                "Agentdesktop will use your organization identity credential only.",
            ),
        };
        format!(
            r#"<div class="step"><span class="check{state}" aria-hidden="true"></span><div><span class="tag">{status}</span><strong>Model provider subscription</strong><span>{description}</span></div></div>"#
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="steps">
  <div class="step"><span class="check{identity_state}" aria-hidden="true"></span><div><span class="tag">Required</span><strong>Organization sign-in</strong><span>Authenticate with the configured identity provider.</span></div></div>
  {subscription}
</div>"#
    )
}

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    authorization_url: String,
    page: AuthorizationPage,
    result: Arc<Mutex<Option<AuthorizationResultSender>>>,
}

type AuthorizationResultSender = oneshot::Sender<anyhow::Result<Option<String>>>;

#[derive(Clone)]
struct ContinuedPageState {
    html: String,
    viewed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) id_token: Option<String>,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_in: u64,
    pub(crate) token_type: String,
}

pub async fn enroll(
    controller: &ControllerConnectionConfig,
    enrollment: &EnrollmentState,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<Identity> {
    if controller.device_authorization {
        return enroll_with_device_authorization(controller, enrollment).await;
    }
    let (verifier, challenge) = pkce();
    let mut client = remote::client(controller, None).await?;
    let (device_key, csr) = device_csr()?;
    let mut begin = client
        .begin_enrollment(BeginEnrollmentRequest {
            hostname: remote::hostname(),
            code_challenge: challenge,
            device_authorization: false,
        })
        .await
        .context("begin OIDC enrollment")?
        .into_inner();

    let redirect_uri = Url::parse(&begin.redirect_uri).context("parse OIDC redirect URI")?;
    enrollment
        .awaiting_authentication(begin.authorization_url.clone())
        .await;
    let authorization_code = wait_for_authorization_code(
        &begin.authorization_url,
        &redirect_uri,
        std::mem::take(&mut begin.state),
        callback_listen,
    )
    .await?;
    enrollment.set("enrolling").await;

    let tokens = exchange_authorization_code(
        &begin.token_endpoint,
        &begin.client_id,
        &begin.redirect_uri,
        &authorization_code,
        &verifier,
    )
    .await?;
    complete_enrollment(&mut client, begin, &device_key, &csr, tokens).await
}

/// Enrolls with the OAuth 2.0 Device Authorization Grant (RFC 8628).
///
/// The controller starts the device authorization with the identity provider
/// and returns a user code. The daemon publishes the verification URL and code
/// through the enrollment status and logs, then polls the token endpoint until
/// the user approves the request from another device.
async fn enroll_with_device_authorization(
    controller: &ControllerConnectionConfig,
    enrollment: &EnrollmentState,
) -> anyhow::Result<Identity> {
    let mut client = remote::client(controller, None).await?;
    let (device_key, csr) = device_csr()?;
    let begin = client
        .begin_enrollment(BeginEnrollmentRequest {
            hostname: remote::hostname(),
            code_challenge: String::new(),
            device_authorization: true,
        })
        .await
        .context("begin OIDC device authorization enrollment")?
        .into_inner();
    if begin.device_code.is_empty() || begin.user_code.is_empty() {
        bail!("controller does not support device authorization enrollment");
    }

    let verification_url = if begin.verification_uri_complete.is_empty() {
        begin.verification_uri.clone()
    } else {
        begin.verification_uri_complete.clone()
    };
    tracing::info!(
        verification_uri = %begin.verification_uri,
        verification_uri_complete = %begin.verification_uri_complete,
        user_code = %begin.user_code,
        "approve device enrollment: open the verification URL on any device and confirm the code"
    );
    enrollment
        .awaiting_device_authorization(verification_url, begin.user_code.clone())
        .await;

    let interval = match begin.interval_seconds {
        0 => DEFAULT_DEVICE_POLL_INTERVAL,
        seconds => Duration::from_secs(seconds),
    };
    let tokens = poll_device_token(
        &begin.token_endpoint,
        &begin.client_id,
        &begin.device_code,
        interval,
        Duration::from_secs(begin.expires_in_seconds),
    )
    .await?;
    enrollment.set("enrolling").await;
    complete_enrollment(&mut client, begin, &device_key, &csr, tokens).await
}

fn device_csr() -> anyhow::Result<(KeyPair, CertificateSigningRequest)> {
    let device_key = KeyPair::generate().context("generate device TLS private key")?;
    let mut certificate_params = CertificateParams::new(Vec::<String>::new())?;
    certificate_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    certificate_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let csr = certificate_params
        .serialize_request(&device_key)
        .context("create device certificate signing request")?;
    Ok((device_key, csr))
}

async fn complete_enrollment(
    client: &mut FleetAgentClient<Channel>,
    begin: BeginEnrollmentResponse,
    device_key: &KeyPair,
    csr: &CertificateSigningRequest,
    tokens: TokenResponse,
) -> anyhow::Result<Identity> {
    let id_token = tokens
        .id_token
        .context("OIDC token response did not contain an ID token")?;
    let refresh_token = tokens
        .refresh_token
        .context("OIDC token response did not contain a refresh token")?;

    let mut request = tonic::Request::new(CompleteEnrollmentRequest {
        enrollment_id: begin.enrollment_id,
        authorization_code: String::new(),
        code_verifier: String::new(),
        certificate_signing_request_der: csr.der().as_ref().to_vec(),
        id_token,
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", tokens.access_token)
            .parse()
            .context("encode OIDC access token")?,
    );
    let response = client
        .complete_enrollment(request)
        .await
        .context("complete OIDC enrollment")?
        .into_inner();

    let client_certificate_pem = String::from_utf8(response.client_certificate_pem)
        .context("controller returned a non-UTF-8 device certificate")?;
    if client_certificate_pem.is_empty() {
        anyhow::bail!("controller returned an empty device certificate");
    }
    Ok(Identity {
        device_id: response.device_id,
        client_certificate_pem,
        client_private_key_pem: device_key.serialize_pem(),
        client_certificate_expires_at_unix_seconds: response
            .client_certificate_expires_at_unix_seconds,
        oauth: OAuthCredentials {
            access_token: tokens.access_token,
            refresh_token,
            expires_at_unix_seconds: unix_time_seconds().saturating_add(tokens.expires_in),
        },
        oauth_token_endpoint: begin.token_endpoint,
        oauth_client_id: begin.client_id,
    })
}

pub(crate) async fn exchange_authorization_code(
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> anyhow::Result<TokenResponse> {
    let response = reqwest::Client::new()
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("exchange OIDC authorization code")?
        .error_for_status()
        .context("OIDC token endpoint rejected authorization code")?
        .json::<TokenResponse>()
        .await
        .context("decode OIDC token response")?;
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    Ok(response)
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum DevicePoll {
    Pending,
    SlowDown,
    Failed(String),
}

/// Classifies an RFC 8628 section 3.5 token endpoint error response.
fn classify_device_token_error(body: &[u8]) -> DevicePoll {
    let Ok(error) = serde_json::from_slice::<TokenErrorResponse>(body) else {
        return DevicePoll::Failed("OIDC token endpoint returned an unreadable error".to_owned());
    };
    match error.error.as_str() {
        "authorization_pending" => DevicePoll::Pending,
        "slow_down" => DevicePoll::SlowDown,
        "access_denied" => DevicePoll::Failed("device enrollment was denied".to_owned()),
        "expired_token" => DevicePoll::Failed("device enrollment code expired".to_owned()),
        other => DevicePoll::Failed(match error.error_description {
            Some(description) => format!("OIDC token endpoint returned {other}: {description}"),
            None => format!("OIDC token endpoint returned {other}"),
        }),
    }
}

async fn poll_device_token(
    token_endpoint: &str,
    client_id: &str,
    device_code: &str,
    mut interval: Duration,
    expires_in: Duration,
) -> anyhow::Result<TokenResponse> {
    let deadline = tokio::time::Instant::now() + expires_in;
    let client = reqwest::Client::new();
    loop {
        tokio::time::sleep(interval).await;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!("device enrollment code expired before it was approved");
        }
        // Bound the request (connect through response body) by whatever's left of
        // expires_in, not just the pre-request deadline check above. Without this, a
        // token endpoint that accepts the connection and then stalls — never sending a
        // response at all — leaves the request outstanding indefinitely: the deadline
        // check only runs again once `send()` (or the subsequent `.json()`/`.bytes()`
        // body read) actually returns, which a stalled peer never does. Polling could
        // then hang past the advertised expires_in forever instead of failing at it.
        let remaining = deadline - now;
        let response = client
            .post(token_endpoint)
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("device_code", device_code),
                ("client_id", client_id),
            ])
            .timeout(remaining)
            .send()
            .await
            .context("poll OIDC token endpoint for device authorization")?;
        if response.status().is_success() {
            let response = response
                .json::<TokenResponse>()
                .await
                .context("decode OIDC token response")?;
            if !response.token_type.eq_ignore_ascii_case("Bearer") {
                bail!("OIDC token endpoint returned unsupported token type");
            }
            return Ok(response);
        }
        let body = response
            .bytes()
            .await
            .context("read OIDC token endpoint error")?;
        match classify_device_token_error(&body) {
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => interval += DEVICE_POLL_SLOW_DOWN,
            DevicePoll::Failed(message) => bail!(message),
        }
    }
}

pub async fn refresh(identity: &mut Identity) -> anyhow::Result<()> {
    let endpoint = &identity.oauth_token_endpoint;
    let client_id = &identity.oauth_client_id;
    let current = &identity.oauth;
    let response = refresh_access_token(endpoint, client_id, &current.refresh_token).await?;
    apply_refreshed_tokens(identity, response)
}

pub(crate) async fn refresh_access_token(
    endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> anyhow::Result<TokenResponse> {
    let response = reqwest::Client::new()
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("refresh OIDC access token")?
        .error_for_status()
        .context("OIDC token endpoint rejected refresh token")?
        .json::<TokenResponse>()
        .await
        .context("decode refreshed OIDC token response")?;
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    Ok(response)
}

fn apply_refreshed_tokens(identity: &mut Identity, response: TokenResponse) -> anyhow::Result<()> {
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    let current_refresh_token = identity.oauth.refresh_token.clone();
    identity.oauth = OAuthCredentials {
        access_token: response.access_token,
        refresh_token: response.refresh_token.unwrap_or(current_refresh_token),
        expires_at_unix_seconds: unix_time_seconds().saturating_add(response.expires_in),
    };
    Ok(())
}

fn unix_time_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) async fn wait_for_authorization_code(
    authorization_url: &str,
    redirect_uri: &Url,
    expected_state: String,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<String> {
    wait_for_authorization_code_with_page(
        authorization_url,
        redirect_uri,
        expected_state,
        callback_listen,
        AuthorizationPage::Identity {
            subscription_available: false,
            github_available: false,
        },
        true,
    )
    .await?
    .context("required authorization was skipped")
}

pub(crate) async fn wait_for_authorization_code_with_page(
    authorization_url: &str,
    redirect_uri: &Url,
    expected_state: String,
    callback_listen: Option<SocketAddr>,
    page: AuthorizationPage,
    open_browser: bool,
) -> anyhow::Result<Option<String>> {
    let listener = bind_callback(redirect_uri, callback_listen).await?;
    let (result_sender, result_receiver) = oneshot::channel();
    let state = CallbackState {
        expected_state,
        authorization_url: authorization_url.to_owned(),
        page,
        result: Arc::new(Mutex::new(Some(result_sender))),
    };
    let callback_path = redirect_uri.path().to_owned();
    let app = Router::new()
        .route("/", get(authorization_prompt))
        .route("/continue", get(continue_authorization))
        .route("/skip", get(skip_authorization))
        .route("/flow-ready", get(flow_ready))
        .route(&callback_path, get(callback))
        .with_state(state);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
            .context("serve OIDC callback")
    });

    let mut prompt_url = redirect_uri.clone();
    prompt_url.set_path("/");
    prompt_url.set_query(None);
    prompt_url.set_fragment(None);
    println!("Open this URL to connect Agentdesktop:\n{prompt_url}");
    if open_browser {
        tracing::info!(%authorization_url, %prompt_url, "opening Agentdesktop authorization page");
        // Detached: `open::that` waits for the launcher to exit, and a stuck
        // `xdg-open` (no display, portal call hanging) would block this runtime
        // worker with the just-spawned callback server behind it, so the page
        // the user is told to open never answers.
        if let Err(error) = open::that_detached(prompt_url.as_str()) {
            tracing::warn!(%error, "could not open the browser automatically");
        }
    } else {
        tracing::info!(%prompt_url, "continuing in existing Agentdesktop authorization page");
    }

    let authorization_code = tokio::time::timeout(CALLBACK_TIMEOUT, result_receiver)
        .await
        .context("timed out waiting for OIDC callback")?
        .context("OIDC callback server stopped")??;
    let _ = shutdown_sender.send(());
    server.await.context("join OIDC callback server")??;
    Ok(authorization_code)
}

async fn authorization_prompt(State(state): State<CallbackState>) -> Html<String> {
    Html(state.page.html())
}

async fn continue_authorization(State(state): State<CallbackState>) -> Redirect {
    Redirect::temporary(&state.authorization_url)
}

async fn skip_authorization(State(state): State<CallbackState>) -> Response {
    if !state.page.optional() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(sender) = state.result.lock().await.take() {
        let _ = sender.send(Ok(None));
    }
    Html(state.page.skipped_html()).into_response()
}

async fn flow_ready(State(state): State<CallbackState>) -> Response {
    let status = if state.page == AuthorizationPage::Subscription {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    };
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    response
}

pub(crate) async fn continue_subscription_page(
    redirect_uri: &Url,
    callback_listen: Option<SocketAddr>,
    connected: bool,
) -> anyhow::Result<()> {
    let listener = bind_callback(redirect_uri, callback_listen).await?;
    let (viewed_sender, viewed_receiver) = oneshot::channel();
    let html = if connected {
        AuthorizationPage::Subscription.success_html()
    } else {
        AuthorizationPage::Subscription.skipped_html()
    };
    let state = ContinuedPageState {
        html,
        viewed: Arc::new(Mutex::new(Some(viewed_sender))),
    };
    let app = Router::new()
        .route("/", get(continued_page))
        .route("/flow-ready", get(continued_flow_ready))
        .with_state(state);
    tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = tokio::time::timeout(CALLBACK_TIMEOUT, viewed_receiver).await;
            })
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "serve continued Agentdesktop authorization page");
        }
    });
    Ok(())
}

async fn continued_page(State(state): State<ContinuedPageState>) -> Html<String> {
    if let Some(sender) = state.viewed.lock().await.take() {
        let _ = sender.send(());
    }
    Html(state.html)
}

async fn continued_flow_ready() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    response
}

pub(crate) async fn bind_callback(
    redirect_uri: &Url,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<tokio::net::TcpListener> {
    if redirect_uri.scheme() != "http" {
        bail!("OIDC native callback must use HTTP on loopback");
    }
    let host = redirect_uri
        .host_str()
        .context("OIDC callback has no host")?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        bail!("OIDC callback must use a loopback host");
    }
    let port = redirect_uri
        .port_or_known_default()
        .context("OIDC callback has no port")?;
    let advertised = format!("{host}:{port}");
    match callback_listen {
        Some(listen) => {
            if !listen.ip().is_loopback() {
                tracing::warn!(
                    %listen,
                    %advertised,
                    "OIDC callback server is listening beyond loopback; restrict access at the container or host boundary"
                );
            } else {
                tracing::info!(%listen, %advertised, "binding OIDC callback server");
            }
            tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("bind OIDC callback at {listen}"))
        }
        None => {
            tracing::info!(listen = %advertised, %advertised, "binding OIDC callback server");
            tokio::net::TcpListener::bind((host, port))
                .await
                .with_context(|| format!("bind OIDC callback at {advertised}"))
        }
    }
}

async fn callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let result = if let Some(error) = query.error {
        Err(anyhow::anyhow!(
            "identity provider returned {error}: {}",
            query.error_description.unwrap_or_default()
        ))
    } else if query.state.as_deref() != Some(&state.expected_state) {
        Err(anyhow::anyhow!("OIDC state mismatch"))
    } else {
        query
            .code
            .context("OIDC callback has no authorization code")
            .map(Some)
    };
    let succeeded = result.is_ok();
    if let Some(sender) = state.result.lock().await.take() {
        let _ = sender.send(result);
    }

    if succeeded {
        Html(state.page.success_html()).into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Html(callback_page(
                "Sign-in failed",
                "Return to Agentdesktop and try again. Details are available in the daemon logs.",
            )),
        )
            .into_response()
    }
}

pub(crate) fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn pkce() -> (String, String) {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        AuthorizationPage, DevicePoll, TokenResponse, apply_refreshed_tokens,
        classify_device_token_error, poll_device_token,
    };
    use crate::identity::{Identity, OAuthCredentials};

    // Regression test for the actual bug this fix targets. The pre-request deadline
    // check only runs again once a request RETURNS, so a peer that accepts the
    // connection and then never answers at all previously left polling stuck past
    // expires_in indefinitely — the per-request .timeout() is what actually bounds it.
    //
    // A real elapsed-time wait, not tokio::time::pause: poll_device_token's HTTP calls
    // go through a real reqwest::Client over real sockets, which doesn't observe
    // tokio's virtual clock.
    #[tokio::test]
    async fn poll_device_token_times_out_against_a_stalling_peer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalling listener");
        let addr = listener.local_addr().expect("stalling listener local addr");
        let server = tokio::spawn(async move {
            // Accept and hold every connection open, writing nothing back, for as
            // long as the test might run — simulating a token endpoint that accepts
            // the connection and then never answers.
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let _socket = socket;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    });
                }
            }
        });

        let token_endpoint = format!("http://{addr}/token");
        let expires_in = Duration::from_secs(2);
        let started = Instant::now();
        let result = poll_device_token(
            &token_endpoint,
            "client",
            "device-code",
            Duration::from_millis(50),
            expires_in,
        )
        .await;
        let elapsed = started.elapsed();

        server.abort();
        assert!(
            result.is_err(),
            "a stalling peer must fail the poll, not hang forever"
        );
        // Generous slack over the 2s expires_in: proves the request itself was bounded
        // rather than left outstanding — a regression here would hang for the full 30s
        // the mock connection holds itself open for (or until the test harness's own
        // timeout), not fail anywhere near expires_in.
        assert!(
            elapsed < Duration::from_secs(10),
            "poll_device_token took {elapsed:?} against a 2s expires_in — the per-request \
             timeout did not bound the stalled connection"
        );
    }

    #[test]
    fn classifies_device_token_errors() {
        assert_eq!(
            classify_device_token_error(br#"{"error":"authorization_pending"}"#),
            DevicePoll::Pending
        );
        assert_eq!(
            classify_device_token_error(br#"{"error":"slow_down"}"#),
            DevicePoll::SlowDown
        );
        assert_eq!(
            classify_device_token_error(br#"{"error":"expired_token"}"#),
            DevicePoll::Failed("device enrollment code expired".to_owned())
        );
        assert_eq!(
            classify_device_token_error(br#"{"error":"access_denied"}"#),
            DevicePoll::Failed("device enrollment was denied".to_owned())
        );
        assert_eq!(
            classify_device_token_error(
                br#"{"error":"invalid_client","error_description":"grant not allowed"}"#
            ),
            DevicePoll::Failed(
                "OIDC token endpoint returned invalid_client: grant not allowed".to_owned()
            )
        );
        assert!(matches!(
            classify_device_token_error(b"<html>bad gateway</html>"),
            DevicePoll::Failed(_)
        ));
    }

    #[test]
    fn identity_page_only_mentions_subscription_when_configured() {
        let without_subscription = AuthorizationPage::Identity {
            subscription_available: false,
            github_available: false,
        }
        .html();
        assert!(without_subscription.contains("Organization sign-in"));
        assert!(!without_subscription.contains("Model provider subscription"));

        let with_subscription = AuthorizationPage::Identity {
            subscription_available: true,
            github_available: false,
        }
        .html();
        assert!(with_subscription.contains("Organization sign-in"));
        assert!(with_subscription.contains("Model provider subscription"));
        assert!(with_subscription.contains("Optional"));

        let with_github = AuthorizationPage::Identity {
            subscription_available: false,
            github_available: true,
        };
        assert!(with_github.html().contains("GitHub Copilot"));
        assert!(!without_subscription.contains("GitHub Copilot"));
        assert!(with_github.success_html().contains("location.replace('/')"));
    }

    #[test]
    fn subscription_page_can_be_skipped() {
        let html = AuthorizationPage::Subscription.html();
        assert!(html.contains("You can optionally add"));
        assert!(html.contains("href=\"/skip\""));
        assert!(html.contains("Organization sign-in"));
        assert!(html.contains("class=\"check complete\""));
    }

    #[test]
    fn completed_subscription_page_checks_both_steps() {
        let html = AuthorizationPage::Subscription.success_html();
        assert_eq!(html.matches("class=\"check complete\"").count(), 2);
        assert!(html.contains("All configured connections are complete"));
    }

    #[test]
    fn identity_completion_advances_same_tab_and_skip_preserves_checklist() {
        let identity_complete = AuthorizationPage::Identity {
            subscription_available: true,
            github_available: false,
        }
        .success_html();
        assert_eq!(
            identity_complete
                .matches("class=\"check complete\"")
                .count(),
            1
        );
        assert!(identity_complete.contains("location.replace('http://localhost:51327/')"));

        let skipped = AuthorizationPage::Subscription.skipped_html();
        assert_eq!(skipped.matches("class=\"check complete\"").count(), 1);
        assert!(skipped.contains("class=\"check skipped\""));
        assert!(skipped.contains("Skipped"));
    }

    #[test]
    fn refresh_replaces_rotated_oauth_credentials() {
        let mut identity = Identity {
            device_id: "device".to_owned(),
            client_certificate_pem: "certificate".to_owned(),
            client_private_key_pem: "key".to_owned(),
            client_certificate_expires_at_unix_seconds: u64::MAX,
            oauth: OAuthCredentials {
                access_token: "old-access".to_owned(),
                refresh_token: "old-refresh".to_owned(),
                expires_at_unix_seconds: 0,
            },
            oauth_token_endpoint: "https://idp.example/token".to_owned(),
            oauth_client_id: "client".to_owned(),
        };

        apply_refreshed_tokens(
            &mut identity,
            TokenResponse {
                access_token: "new-access".to_owned(),
                id_token: None,
                refresh_token: Some("new-refresh".to_owned()),
                expires_in: 600,
                token_type: "Bearer".to_owned(),
            },
        )
        .unwrap();
        let oauth = identity.oauth;
        assert_eq!(oauth.access_token, "new-access");
        assert_eq!(oauth.refresh_token, "new-refresh");
        assert!(oauth.expires_at_unix_seconds > 0);
    }
}
