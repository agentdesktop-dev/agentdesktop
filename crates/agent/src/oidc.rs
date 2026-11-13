use std::{net::SocketAddr, sync::Arc, time::Duration};

use agentdesktop_core::config::ControllerConnectionConfig;
use agentdesktop_proto::fleet::{
    EnrollDeviceRequest, LoginConfiguration, LoginConfigurationRequest,
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
    gateway_oidc,
    identity::{Identity, OAuthCredentials},
    remote,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// RFC 8628 section 3.2: clients poll every 5 seconds unless told otherwise.
pub(crate) const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 section 3.5: `slow_down` increases the interval by 5 seconds.
const DEVICE_POLL_SLOW_DOWN: Duration = Duration::from_secs(5);
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Bound on a refresh-token request. The daemon refreshes in the background,
/// and a half-open connection (left behind when the computer sleeps) would
/// otherwise hold the refresh, and the controller session waiting on it,
/// forever.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
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
    open_browser: bool,
) -> anyhow::Result<Identity> {
    let mut client = remote::client(controller, None).await?;
    let config = client
        .get_login_configuration(LoginConfigurationRequest {})
        .await
        .context("get controller login configuration (controller and agent must support the same enrollment protocol)")?
        .into_inner();
    let tokens = if controller.device_authorization {
        device_authorization_login(&config, enrollment).await?
    } else {
        browser_login(&config, enrollment, callback_listen, open_browser).await?
    };
    enrollment.set("enrolling").await;
    let (device_key, csr) = device_csr()?;
    complete_enrollment(&mut client, config, &device_key, &csr, tokens).await
}

async fn browser_login(
    config: &LoginConfiguration,
    enrollment: &EnrollmentState,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<TokenResponse> {
    let (verifier, challenge) = pkce();
    let state = random_secret();
    let nonce = random_secret();
    let redirect_uri = Url::parse(&config.redirect_uri).context("parse OIDC redirect URI")?;
    let mut authorization_url =
        Url::parse(&config.authorization_endpoint).context("parse OIDC authorization endpoint")?;
    authorization_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("scope", &config.scopes.join(" "))
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    enrollment
        .awaiting_authentication(authorization_url.to_string())
        .await;
    let authorization_code = wait_for_authorization_code(
        authorization_url.as_str(),
        &redirect_uri,
        state,
        callback_listen,
        open_browser,
    )
    .await?;
    let tokens = exchange_authorization_code(
        &config.token_endpoint,
        &config.client_id,
        &config.redirect_uri,
        &authorization_code,
        &verifier,
    )
    .await?;
    validate_login_nonce(
        tokens
            .id_token
            .as_deref()
            .context("OIDC token response did not contain an ID token")?,
        &nonce,
    )?;
    Ok(tokens)
}

async fn device_authorization_login(
    config: &LoginConfiguration,
    enrollment: &EnrollmentState,
) -> anyhow::Result<TokenResponse> {
    let endpoint = Url::parse(&config.device_authorization_endpoint)
        .context("controller's OIDC provider does not support device authorization")?;
    let device =
        gateway_oidc::request_device_authorization(endpoint, &config.client_id, &config.scopes)
            .await?;
    let verification_url = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| device.verification_uri.clone());
    tracing::info!(
        verification_uri = %device.verification_uri,
        user_code = %device.user_code,
        "approve device enrollment: open the verification URL on any device and confirm the code"
    );
    enrollment
        .awaiting_device_authorization(verification_url, device.user_code.clone())
        .await;

    let interval = device
        .interval
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_DEVICE_POLL_INTERVAL, Duration::from_secs);
    poll_device_token(
        &config.token_endpoint,
        &config.client_id,
        &device.device_code,
        interval,
        Duration::from_secs(device.expires_in),
    )
    .await
}

fn validate_login_nonce(id_token: &str, expected: &str) -> anyhow::Result<()> {
    // The controller verifies the signature/issuer/audience of this same token.
    // Here the agent binds the token-endpoint response to its own browser login.
    #[derive(Deserialize)]
    struct Nonce {
        nonce: String,
    }
    let payload = id_token
        .split('.')
        .nth(1)
        .context("ID token has no payload")?;
    let claims: Nonce = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .context("decode ID token payload")?,
    )
    .context("decode ID token nonce")?;
    if claims.nonce != expected {
        bail!("OIDC nonce mismatch");
    }
    Ok(())
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
    config: LoginConfiguration,
    device_key: &KeyPair,
    csr: &CertificateSigningRequest,
    tokens: TokenResponse,
) -> anyhow::Result<Identity> {
    let login_started = unix_time_seconds();
    let id_token = tokens
        .id_token
        .context("OIDC token response did not contain an ID token")?;
    let refresh_token = tokens
        .refresh_token
        .context("OIDC token response did not contain a refresh token")?;

    let body = EnrollDeviceRequest {
        hostname: remote::hostname(),
        certificate_signing_request_der: csr.der().as_ref().to_vec(),
        id_token,
    };
    let authorization: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
        format!("Bearer {}", tokens.access_token)
            .parse()
            .context("encode OIDC access token")?;
    let mut delay = Duration::from_secs(1);
    let response = loop {
        let mut request = tonic::Request::new(body.clone());
        request
            .metadata_mut()
            .insert("authorization", authorization.clone());
        request.set_timeout(Duration::from_secs(15));
        match client.enroll_device(request).await {
            Ok(response) => break response.into_inner(),
            Err(error)
                if matches!(
                    error.code(),
                    tonic::Code::Unavailable
                        | tonic::Code::DeadlineExceeded
                        | tonic::Code::ResourceExhausted
                ) && unix_time_seconds() < tokens.expires_in.saturating_add(login_started) =>
            {
                tracing::warn!(%error, "device enrollment interrupted; retrying with the same device key");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            Err(error) => return Err(error).context("enroll authenticated device"),
        }
    };

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
            expires_at_unix_seconds: login_started.saturating_add(tokens.expires_in),
        },
        oauth_token_endpoint: config.token_endpoint,
        oauth_client_id: config.client_id,
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
        "access_denied" => DevicePoll::Failed("device authorization was denied".to_owned()),
        "expired_token" => DevicePoll::Failed("device authorization code expired".to_owned()),
        other => DevicePoll::Failed(match error.error_description {
            Some(description) => format!("OIDC token endpoint returned {other}: {description}"),
            None => format!("OIDC token endpoint returned {other}"),
        }),
    }
}

pub(crate) async fn poll_device_token(
    token_endpoint: &str,
    client_id: &str,
    device_code: &str,
    mut interval: Duration,
    expires_in: Duration,
) -> anyhow::Result<TokenResponse> {
    let deadline = tokio::time::Instant::now() + expires_in;
    let client = reqwest::Client::new();
    loop {
        // Never sleep past the deadline. The interval is IdP-controlled and grows by
        // DEVICE_POLL_SLOW_DOWN on every slow_down, so it can exceed what's left of
        // expires_in; sleeping the full interval would then keep the task (and any
        // pending login it represents) alive long after the code expired.
        tokio::time::sleep_until((tokio::time::Instant::now() + interval).min(deadline)).await;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!("device authorization code expired before it was approved");
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
        .timeout(REFRESH_TIMEOUT)
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
    open_browser: bool,
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
        open_browser,
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
    wait_for_authorization_code_with_callback(
        authorization_url,
        redirect_uri,
        expected_state,
        callback_listen,
        page,
        open_browser,
        None,
    )
    .await
}

pub(crate) async fn wait_for_subscription_authorization_code(
    authorization_url: &str,
    provider_callback: &Url,
    expected_state: String,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<String>> {
    let prompt_uri =
        Url::parse("http://localhost:51327/callback").expect("valid subscription prompt URI");
    wait_for_authorization_code_with_callback(
        authorization_url,
        &prompt_uri,
        expected_state,
        callback_listen,
        AuthorizationPage::Subscription,
        open_browser,
        Some(provider_callback),
    )
    .await
}

async fn wait_for_authorization_code_with_callback(
    authorization_url: &str,
    redirect_uri: &Url,
    expected_state: String,
    callback_listen: Option<SocketAddr>,
    page: AuthorizationPage,
    open_browser: bool,
    provider_callback: Option<&Url>,
) -> anyhow::Result<Option<String>> {
    let listener = bind_callback(redirect_uri, callback_listen).await?;
    let provider_listener = match provider_callback {
        Some(callback) => Some(bind_callback(callback, None).await?),
        None => None,
    };
    let (result_sender, result_receiver) = oneshot::channel();
    let state = CallbackState {
        expected_state,
        authorization_url: authorization_url.to_owned(),
        page,
        result: Arc::new(Mutex::new(Some(result_sender))),
    };
    let callback_path = redirect_uri.path().to_owned();
    let mut app = Router::new()
        .route("/", get(authorization_prompt))
        .route("/continue", get(continue_authorization))
        .route("/skip", get(skip_authorization))
        .route("/flow-ready", get(flow_ready))
        .route(&callback_path, get(callback));
    if let Some(provider_callback) = provider_callback
        && provider_callback.path() != callback_path
    {
        app = app.route(provider_callback.path(), get(callback));
    }
    let app = app.with_state(state);
    let provider_server = provider_listener.map(|listener| {
        let app = app.clone();
        let (shutdown, stopped) = oneshot::channel::<()>();
        (
            shutdown,
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = stopped.await;
                    })
                    .await
            }),
        )
    });
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

    let authorization_code = async {
        tokio::time::timeout(CALLBACK_TIMEOUT, result_receiver)
            .await
            .context("timed out waiting for OIDC callback")?
            .context("OIDC callback server stopped")?
    }
    .await;
    let _ = shutdown_sender.send(());
    if let Some((shutdown, provider_server)) = provider_server {
        let _ = shutdown.send(());
        provider_server
            .await
            .context("join provider OAuth callback server")??;
    }
    server.await.context("join OIDC callback server")??;
    authorization_code
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
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = tokio::time::timeout(CALLBACK_TIMEOUT, viewed_receiver).await;
            })
            .await
    })
    .await
    .context("join continued Agentdesktop authorization page")?
    .context("serve continued Agentdesktop authorization page")?;
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
        classify_device_token_error, continue_subscription_page, poll_device_token,
    };
    use crate::identity::{Identity, OAuthCredentials};

    #[derive(Clone)]
    struct EnrollmentController {
        config: agentdesktop_proto::fleet::LoginConfiguration,
        authenticated: std::sync::Arc<std::sync::atomic::AtomicBool>,
        requests:
            std::sync::Arc<tokio::sync::Mutex<Vec<agentdesktop_proto::fleet::EnrollDeviceRequest>>>,
    }

    #[tonic::async_trait]
    impl agentdesktop_proto::fleet::fleet_agent_server::FleetAgent for EnrollmentController {
        type ConnectStream = tokio_stream::wrappers::ReceiverStream<
            Result<agentdesktop_proto::fleet::ControllerMessage, tonic::Status>,
        >;

        async fn get_login_configuration(
            &self,
            _: tonic::Request<agentdesktop_proto::fleet::LoginConfigurationRequest>,
        ) -> Result<tonic::Response<agentdesktop_proto::fleet::LoginConfiguration>, tonic::Status>
        {
            Ok(tonic::Response::new(self.config.clone()))
        }

        async fn enroll_device(
            &self,
            request: tonic::Request<agentdesktop_proto::fleet::EnrollDeviceRequest>,
        ) -> Result<tonic::Response<agentdesktop_proto::fleet::EnrollResponse>, tonic::Status>
        {
            assert!(
                self.authenticated.load(std::sync::atomic::Ordering::SeqCst),
                "enrollment must not be called before user authentication"
            );
            assert_eq!(
                request.metadata().get("authorization").unwrap(),
                "Bearer access-token"
            );
            let request = request.into_inner();
            rcgen::CertificateSigningRequestParams::from_der(
                &request.certificate_signing_request_der.as_slice().into(),
            )
            .unwrap();
            let mut requests = self.requests.lock().await;
            requests.push(request);
            if requests.len() == 1 {
                return Err(tonic::Status::unavailable(
                    "simulated lost enrollment response",
                ));
            }
            assert_eq!(
                requests[0], requests[1],
                "retries must reuse the CSR and user credentials"
            );
            Ok(tonic::Response::new(
                agentdesktop_proto::fleet::EnrollResponse {
                    device_id: "device-1".to_owned(),
                    client_certificate_pem: b"mock-certificate".to_vec(),
                    client_certificate_expires_at_unix_seconds: 12345,
                },
            ))
        }

        async fn renew_device_certificate(
            &self,
            _: tonic::Request<agentdesktop_proto::fleet::RenewDeviceCertificateRequest>,
        ) -> Result<
            tonic::Response<agentdesktop_proto::fleet::DeviceCertificateResponse>,
            tonic::Status,
        > {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn get_llm_gateway_credential(
            &self,
            _: tonic::Request<agentdesktop_proto::fleet::LlmGatewayCredentialRequest>,
        ) -> Result<
            tonic::Response<agentdesktop_proto::fleet::LlmGatewayCredentialResponse>,
            tonic::Status,
        > {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn get_device_usage(
            &self,
            _: tonic::Request<agentdesktop_proto::fleet::DeviceUsageRequest>,
        ) -> Result<tonic::Response<agentdesktop_proto::fleet::DeviceUsageResponse>, tonic::Status>
        {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn connect(
            &self,
            _: tonic::Request<tonic::Streaming<agentdesktop_proto::fleet::AgentMessage>>,
        ) -> Result<tonic::Response<Self::ConnectStream>, tonic::Status> {
            Err(tonic::Status::unimplemented("not used"))
        }
    }

    #[tokio::test]
    async fn automatic_browser_and_headless_login_precede_enrollment_and_retries_reuse_key() {
        use agentdesktop_proto::fleet::{LoginConfiguration, fleet_agent_server::FleetAgentServer};
        use axum::{Json, Router, response::IntoResponse, routing::post};
        use base64::Engine as _;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use tokio::sync::{Mutex, Notify};

        for (headless, approved) in [(false, true), (true, true), (false, false), (true, false)] {
            let authenticated = Arc::new(AtomicBool::new(false));
            let release = Arc::new(Notify::new());
            let token_claims = Arc::new(Mutex::new(serde_json::json!({"sub": "user-1"})));
            let idp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let idp_address = idp_listener.local_addr().unwrap();
            let app = Router::new()
                .route(
                    "/device",
                    post(|| async {
                        Json(serde_json::json!({
                            "device_code": "device-code", "user_code": "CODE",
                            "verification_uri": "https://idp.example/activate",
                            "expires_in": 600, "interval": 1,
                        }))
                    }),
                )
                .route(
                    "/token",
                    post({
                        let authenticated = authenticated.clone();
                        let release = release.clone();
                        let token_claims = token_claims.clone();
                        move |axum::Form(form): axum::Form<
                            std::collections::HashMap<String, String>,
                        >| {
                            let authenticated = authenticated.clone();
                            let release = release.clone();
                            let token_claims = token_claims.clone();
                            async move {
                                assert_eq!(form["client_id"], "client");
                                if headless {
                                    assert_eq!(form["device_code"], "device-code");
                                } else {
                                    assert_eq!(form["code"], "authorization-code");
                                    assert!(!form["code_verifier"].is_empty());
                                }
                                release.notified().await;
                                if !approved {
                                    return (
                                        axum::http::StatusCode::BAD_REQUEST,
                                        Json(serde_json::json!({"error": "access_denied"})),
                                    )
                                        .into_response();
                                }
                                authenticated.store(true, Ordering::SeqCst);
                                let payload = super::URL_SAFE_NO_PAD.encode(
                                    serde_json::to_vec(&*token_claims.lock().await).unwrap(),
                                );
                                Json(serde_json::json!({
                                "access_token": "access-token", "refresh_token": "refresh-token",
                                "id_token": format!("header.{payload}.signature"),
                                "expires_in": 600, "token_type": "Bearer",
                            })).into_response()
                            }
                        }
                    }),
                );
            let idp = tokio::spawn(async move { axum::serve(idp_listener, app).await.unwrap() });
            let callback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let callback_address = callback_listener.local_addr().unwrap();
            drop(callback_listener);
            let config = LoginConfiguration {
                issuer: "https://idp.example".to_owned(),
                authorization_endpoint: format!("http://{idp_address}/authorize"),
                token_endpoint: format!("http://{idp_address}/token"),
                client_id: "client".to_owned(),
                redirect_uri: format!("http://{callback_address}/callback"),
                device_authorization_endpoint: format!("http://{idp_address}/device"),
                scopes: vec!["openid".to_owned(), "offline_access".to_owned()],
            };
            let mock = EnrollmentController {
                config,
                authenticated: authenticated.clone(),
                requests: Arc::default(),
            };
            let requests = mock.requests.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(FleetAgentServer::new(mock))
                    .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
                    .await
                    .unwrap();
            });
            let directory = tempfile::tempdir().unwrap();
            let ca_certificate_path = directory.path().join("ca.pem");
            let ca = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
            std::fs::write(&ca_certificate_path, ca.cert.pem()).unwrap();
            let controller = agentdesktop_core::config::ControllerConnectionConfig {
                address: format!("http://{address}"),
                ca_certificate_path,
                heartbeat_interval: Duration::from_secs(30),
                device_authorization: headless,
            };
            let enrollment = crate::enrollment::EnrollmentState::new(true);
            let mut pending = Box::pin(super::enroll(&controller, &enrollment, None, false));
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut pending => panic!("enrollment finished before login: {result:?}"),
                    status = async {
                        loop {
                            let status = enrollment.get().await;
                            if status.status == "awaitingAuthentication" {
                                break status;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    } => status,
                }
            })
            .await
            .expect("login must become ready");
            assert_eq!(status.status, "awaitingAuthentication");
            assert!(requests.lock().await.is_empty());
            assert!(!authenticated.load(Ordering::SeqCst));
            if headless {
                assert_eq!(status.user_code.as_deref(), Some("CODE"));
                assert_eq!(
                    status.authorization_url.as_deref(),
                    Some("https://idp.example/activate")
                );
            } else {
                let url = url::Url::parse(status.authorization_url.as_deref().unwrap()).unwrap();
                let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
                assert_eq!(query["code_challenge_method"], "S256");
                assert!(!query["code_challenge"].is_empty());
                token_claims.lock().await["nonce"] = serde_json::json!(query["nonce"]);
                let callback = format!(
                    "http://{callback_address}/callback?code=authorization-code&state={}",
                    query["state"],
                );
                reqwest::get(callback)
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
            release.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap();
            if approved {
                let identity = result.unwrap();
                assert_eq!(identity.device_id, "device-1");
                assert_eq!(identity.oauth.access_token, "access-token");
                assert_eq!(requests.lock().await.len(), 2);
                assert_eq!(enrollment.get().await.status, "enrolling");
            } else {
                assert!(result.is_err());
                assert!(requests.lock().await.is_empty());
                assert!(!authenticated.load(Ordering::SeqCst));
            }
            server.abort();
            idp.abort();
        }
    }

    #[test]
    fn browser_nonce_must_match_before_device_enrollment() {
        use base64::Engine as _;
        let token = |value| {
            format!(
                "header.{}.signature",
                super::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap()),
            )
        };
        assert!(
            super::validate_login_nonce(
                &token(serde_json::json!({"nonce": "expected"})),
                "expected"
            )
            .is_ok()
        );
        for token in [
            "invalid".to_owned(),
            "header.invalid.signature".to_owned(),
            token(serde_json::json!({})),
            token(serde_json::json!({"nonce": "other"})),
        ] {
            assert!(super::validate_login_nonce(&token, "expected").is_err());
        }
    }

    // The poll interval is provider-controlled and grows on every slow_down, so it can
    // exceed what is left of expires_in. Sleeping the whole interval kept the task, and
    // the pending login it stands for, alive long after the code had expired.
    #[tokio::test(start_paused = true)]
    async fn poll_device_token_does_not_sleep_past_the_deadline() {
        let started = tokio::time::Instant::now();
        let result = poll_device_token(
            "http://127.0.0.1:1/token",
            "client",
            "device-code",
            Duration::from_secs(3600),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_err());
        assert!(
            started.elapsed() <= Duration::from_secs(6),
            "slept {:?} against a 5s expires_in",
            started.elapsed()
        );
    }

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
            DevicePoll::Failed("device authorization code expired".to_owned())
        );
        assert_eq!(
            classify_device_token_error(br#"{"error":"access_denied"}"#),
            DevicePoll::Failed("device authorization was denied".to_owned())
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

    #[tokio::test]
    async fn subscription_prompt_accepts_a_separate_registered_provider_callback() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let prompt_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let prompt_address = prompt_listener.local_addr().unwrap();
            let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let provider_address = provider_listener.local_addr().unwrap();
            drop((prompt_listener, provider_listener));
            let prompt = url::Url::parse(&format!("http://{prompt_address}/callback")).unwrap();
            let mut provider =
                url::Url::parse(&format!("http://{provider_address}/auth/callback")).unwrap();
            let callback = provider.clone();
            let task = tokio::spawn(async move {
                super::wait_for_authorization_code_with_callback(
                    "https://provider.example/authorize",
                    &prompt,
                    "expected-state".to_owned(),
                    None,
                    super::AuthorizationPage::Subscription,
                    false,
                    Some(&callback),
                )
                .await
            });
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            loop {
                if let Ok(response) = client
                    .get(format!("http://{prompt_address}/flow-ready"))
                    .send()
                    .await
                    && response.status().is_success()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let response = client
                .get(format!("http://{prompt_address}/continue"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.headers()["location"],
                "https://provider.example/authorize"
            );
            provider
                .query_pairs_mut()
                .extend_pairs([("state", "expected-state"), ("code", "provider-code")]);
            let response = client.get(provider).send().await.unwrap();
            assert!(response.status().is_success());
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("Agentdesktop connected")
            );
            assert_eq!(
                task.await.unwrap().unwrap().as_deref(),
                Some("provider-code")
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn continued_subscription_page_waits_for_view_and_then_stops() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            let redirect = url::Url::parse("http://localhost:51327/callback").unwrap();
            let server = tokio::spawn(async move {
                continue_subscription_page(&redirect, Some(address), true).await
            });
            let client = reqwest::Client::new();
            loop {
                if let Ok(response) = client
                    .get(format!("http://{address}/flow-ready"))
                    .send()
                    .await
                    && response.status().is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                !server.is_finished(),
                "listener must remain owned until page visit"
            );
            let response = client
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("Agentdesktop connected")
            );
            server.await.unwrap().unwrap();
            assert!(
                client
                    .get(format!("http://{address}/"))
                    .send()
                    .await
                    .is_err()
            );
        })
        .await
        .unwrap();
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
