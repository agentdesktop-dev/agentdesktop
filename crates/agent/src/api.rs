use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use agentdesktop_core::{
    config::{DaemonConfig, LlmGatewayAuthentication, ProgramAuthentication, valid_client_id},
    model::{
        Discovery, EnrollmentStatus, LlmGatewayCredential, LlmUsageBreakdown, LlmUsageInteraction,
        LlmUsageInteractions, LlmUsageRange, LlmUsageSummary, TelemetryEvent, TelemetryEventKind,
    },
};

use crate::{enrollment::EnrollmentState, gateway_oidc, remote, subscription};

#[derive(Clone)]
pub struct AppState {
    pub config: DaemonConfig,
    pub discovery: Discovery,
    pub enrollment: EnrollmentState,
    pub state_dir: PathBuf,
    pub oidc_callback_listen: Option<SocketAddr>,
    pub telemetry: Option<mpsc::Sender<TelemetryEvent>>,
    pub logout: Option<mpsc::Sender<remote::LogoutRequest>>,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Deserialize)]
struct CredentialQuery {
    client_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsResponse {
    time_range: AnalyticsTimeRange,
    groups: Vec<AnalyticsGroup>,
}

#[derive(Deserialize)]
struct AnalyticsTimeRange {
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsGroup {
    group: BTreeMap<String, serde_json::Value>,
    requests: u64,
    total_tokens: u64,
    cost: f64,
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default)]
    range: LlmUsageRange,
}

#[derive(Deserialize)]
struct UsageInteractionsQuery {
    from: String,
    to: String,
    model: String,
    agent: String,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    logs: Vec<SearchLog>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchLog {
    id: String,
    started_at: String,
    completed_at: Option<String>,
    duration_ms: Option<u64>,
    http_status: Option<u16>,
    error: Option<String>,
    gen_ai: SearchGenAi,
    usage: SearchUsage,
    cost: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchGenAi {
    operation_name: Option<String>,
    provider_name: Option<String>,
    request_model: Option<String>,
    response_model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/config", get(config))
        .route("/v1/effective-config", get(effective_config))
        .route("/v1/remote-config", get(remote_config))
        .route("/v1/discovery", get(discover))
        .route("/v1/enrollment", get(enrollment))
        .route("/v1/logout", post(logout))
        .route("/v1/telemetry", post(telemetry))
        .route("/v1/llm-gateway/credential", get(llm_gateway_credential))
        .route("/v1/llm-gateway/usage", get(llm_gateway_usage))
        .route(
            "/v1/llm-gateway/usage/interactions",
            get(llm_gateway_usage_interactions),
        )
        .with_state(state)
}

async fn llm_gateway_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Option<LlmUsageSummary>>, (StatusCode, String)> {
    let Some(usage_url) = configured_usage_url(&state)? else {
        return Ok(Json(None));
    };
    fetch_llm_usage(&usage_url, query.range)
        .await
        .map(Some)
        .map(Json)
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("query LLM gateway usage: {error:#}"),
            )
        })
}

fn configured_usage_url(state: &AppState) -> Result<Option<url::Url>, (StatusCode, String)> {
    let effective = load_effective_config(&state.config, &state.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied configuration: {error:#}"),
        )
    })?;
    Ok(effective.llm_gateway.and_then(|gateway| gateway.usage_url))
}

fn usage_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

async fn fetch_llm_usage(
    usage_url: &url::Url,
    range: LlmUsageRange,
) -> anyhow::Result<LlmUsageSummary> {
    let (from, to) = usage_time_range(range)?;
    let analytics = usage_client()?
        .post(usage_url.clone())
        .json(&serde_json::json!({
            "timeRange": { "from": from, "to": to },
            "groupBy": [
                { "field": "requestModel" },
                { "field": "attributes", "key": "user_agent.name" }
            ],
            "bucketCount": 1
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<AnalyticsResponse>()
        .await?;
    let mut requests = 0_u64;
    let mut total_tokens = 0_u64;
    let mut estimated_cost_usd = 0.0_f64;
    let mut breakdown = Vec::with_capacity(analytics.groups.len());
    for group in analytics.groups {
        requests = requests.saturating_add(group.requests);
        total_tokens = total_tokens.saturating_add(group.total_tokens);
        estimated_cost_usd += group.cost;
        breakdown.push(LlmUsageBreakdown {
            model: analytics_group_value(&group.group, "requestModel", "Unknown model"),
            agent: analytics_group_value(&group.group, "user_agent.name", "Unknown agent"),
            requests: group.requests,
            total_tokens: group.total_tokens,
            estimated_cost_usd: group.cost,
        });
    }
    breakdown.sort_by(|left, right| {
        right
            .estimated_cost_usd
            .total_cmp(&left.estimated_cost_usd)
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.agent.cmp(&right.agent))
    });
    Ok(LlmUsageSummary {
        from: analytics.time_range.from,
        to: analytics.time_range.to,
        requests,
        total_tokens,
        estimated_cost_usd,
        breakdown,
    })
}

async fn llm_gateway_usage_interactions(
    State(state): State<AppState>,
    Query(query): Query<UsageInteractionsQuery>,
) -> Result<Json<Option<LlmUsageInteractions>>, (StatusCode, String)> {
    if query.model.trim().is_empty() || query.model.len() > 256 {
        return Err((StatusCode::BAD_REQUEST, "invalid model".to_owned()));
    }
    if query.agent.trim().is_empty() || query.agent.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "invalid agent".to_owned()));
    }
    if query
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > 512)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid cursor".to_owned()));
    }
    validate_usage_time_range(&query.from, &query.to)
        .map_err(|message| (StatusCode::BAD_REQUEST, message.to_owned()))?;
    let Some(usage_url) = configured_usage_url(&state)? else {
        return Ok(Json(None));
    };
    fetch_llm_usage_interactions(
        &usage_url,
        &query.from,
        &query.to,
        &query.model,
        &query.agent,
        query.cursor.as_deref(),
    )
    .await
    .map(Some)
    .map(Json)
    .map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("query LLM gateway interactions: {error:#}"),
        )
    })
}

async fn fetch_llm_usage_interactions(
    usage_url: &url::Url,
    from: &str,
    to: &str,
    model: &str,
    agent: &str,
    cursor: Option<&str>,
) -> anyhow::Result<LlmUsageInteractions> {
    // Agentgateway serves `/api/logs/search` beside `/api/logs/analytics/summary`.
    let search_url = usage_url.join("../search")?;
    let response = usage_client()?
        .post(search_url)
        .json(&serde_json::json!({
            "limit": 25,
            "cursor": cursor,
            "timeRange": { "from": from, "to": to },
            "filters": {
                "requestModel": [model],
                "attributes": { "user_agent.name": agent }
            }
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<SearchResponse>()
        .await?;
    let interactions = response
        .logs
        .into_iter()
        .map(|log| LlmUsageInteraction {
            agent: agent.to_owned(),
            request_model: log.gen_ai.request_model.unwrap_or_else(|| model.to_owned()),
            id: log.id,
            started_at: log.started_at,
            completed_at: log.completed_at,
            duration_ms: log.duration_ms,
            http_status: log.http_status,
            failed: log.error.is_some(),
            operation: log.gen_ai.operation_name,
            provider: log.gen_ai.provider_name,
            response_model: log.gen_ai.response_model,
            input_tokens: log.usage.input_tokens,
            output_tokens: log.usage.output_tokens,
            total_tokens: log.usage.total_tokens,
            estimated_cost_usd: log.cost,
        })
        .collect();
    Ok(LlmUsageInteractions {
        interactions,
        next_cursor: response.next_cursor,
    })
}

fn validate_usage_time_range(from: &str, to: &str) -> Result<(), &'static str> {
    let timestamp_format = &time::format_description::well_known::Rfc3339;
    let from = time::OffsetDateTime::parse(from, timestamp_format)
        .map_err(|_| "invalid usage start time")?;
    let to =
        time::OffsetDateTime::parse(to, timestamp_format).map_err(|_| "invalid usage end time")?;
    let duration = to - from;
    if duration <= time::Duration::ZERO || duration > time::Duration::days(31) {
        return Err("usage time range must be between zero and 31 days");
    }
    Ok(())
}

fn usage_time_range(range: LlmUsageRange) -> anyhow::Result<(String, String)> {
    let to = time::OffsetDateTime::now_utc();
    let duration = time::Duration::try_from(range.duration())?;
    let from = to - duration;
    let timestamp_format = &time::format_description::well_known::Rfc3339;
    Ok((from.format(timestamp_format)?, to.format(timestamp_format)?))
}

fn analytics_group_value(
    group: &BTreeMap<String, serde_json::Value>,
    key: &str,
    fallback: &str,
) -> String {
    group
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

async fn telemetry(
    State(state): State<AppState>,
    Json(event): Json<TelemetryEventKind>,
) -> Result<StatusCode, (StatusCode, String)> {
    let sender = state.telemetry.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller configured".to_owned(),
        )
    })?;
    validate_telemetry(&event)?;
    let event = TelemetryEvent {
        timestamp_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        event,
    };
    sender.try_send(event).map_err(|error| {
        let status = match error {
            mpsc::error::TrySendError::Full(_) => StatusCode::SERVICE_UNAVAILABLE,
            mpsc::error::TrySendError::Closed(_) => StatusCode::FAILED_DEPENDENCY,
        };
        (status, "telemetry pipeline is unavailable".to_owned())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_telemetry(event: &TelemetryEventKind) -> Result<(), (StatusCode, String)> {
    match event {
        TelemetryEventKind::SessionNew {
            client_id,
            session_id,
        } => {
            if client_id.is_empty() || client_id.len() > 64 {
                return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
            }
            if session_id.is_empty() || session_id.len() > 256 {
                return Err((StatusCode::BAD_REQUEST, "invalid session ID".to_owned()));
            }
        }
        TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } => {
            if client_id.is_empty() || client_id.len() > 64 {
                return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
            }
            if tool_name.is_empty() || tool_name.len() > 128 {
                return Err((StatusCode::BAD_REQUEST, "invalid tool name".to_owned()));
            }
            if tool_use_id.as_ref().is_some_and(|id| id.len() > 256) {
                return Err((StatusCode::BAD_REQUEST, "invalid tool use ID".to_owned()));
            }
            if tool_input.as_ref().is_some_and(|input| {
                serde_json::to_vec(input).is_ok_and(|encoded| encoded.len() > 256 * 1024)
            }) {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "tool input is too large".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn config(State(state): State<AppState>) -> Json<DaemonConfig> {
    Json(state.config)
}

async fn effective_config(
    State(state): State<AppState>,
) -> Result<Json<DaemonConfig>, (StatusCode, String)> {
    load_effective_config(&state.config, &state.state_dir)
        .map(Json)
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read applied configuration: {error:#}"),
            )
        })
}

fn load_effective_config(
    config: &DaemonConfig,
    state_dir: &std::path::Path,
) -> anyhow::Result<DaemonConfig> {
    if config.controller.is_none() {
        return Ok(config.clone());
    }
    let path = state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => agentdesktop_core::config::parse_daemon(&contents)
            .map_err(|error| error.context("parse applied remote configuration")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(config.clone()),
        Err(error) => Err(error.into()),
    }
}

async fn remote_config(
    State(state): State<AppState>,
) -> Result<Json<Option<String>>, (StatusCode, String)> {
    let path = state.state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Json(Some(contents))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Json(None)),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied remote configuration: {error}"),
        )),
    }
}

async fn discover(State(state): State<AppState>) -> Json<Discovery> {
    Json(state.discovery)
}

async fn enrollment(State(state): State<AppState>) -> Json<EnrollmentStatus> {
    Json(state.enrollment.get().await)
}

async fn logout(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    let sender = state.logout.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller session to log out".to_owned(),
        )
    })?;
    let (completion, completed) = oneshot::channel();
    sender
        .send(remote::LogoutRequest { completion })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session is unavailable".to_owned(),
            )
        })?;
    completed
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session stopped before logout completed".to_owned(),
            )
        })?
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn llm_gateway_credential(
    State(state): State<AppState>,
    Query(query): Query<CredentialQuery>,
) -> Result<Json<LlmGatewayCredential>, (StatusCode, String)> {
    if !valid_client_id(&query.client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
    }
    let effective = load_effective_config(&state.config, &state.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied configuration: {error:#}"),
        )
    })?;
    let gateway = effective.llm_gateway.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no LLM gateway configured".to_owned(),
        )
    })?;
    let uses_subscription = program_uses_subscription(&effective, &query.client_id);
    let (identity, continue_in_browser) = match gateway.authentication.as_ref() {
        Some(LlmGatewayAuthentication::ControllerJwt { .. }) => {
            let controller = state.config.controller.as_ref().ok_or_else(|| {
                (
                    StatusCode::FAILED_DEPENDENCY,
                    "controller JWT authentication requires a controller".to_owned(),
                )
            })?;
            // Local transport permissions authenticate the user, not the calling
            // process. The client ID selects an allowed policy within that boundary.
            remote::llm_gateway_credential(controller, &state.state_dir, &query.client_id)
                .await
                .map(|credential| (credential, false))
        }
        Some(LlmGatewayAuthentication::Oidc {
            issuer,
            client_id,
            redirect_uri,
            scopes,
            allow_insecure,
        }) => gateway_oidc::credential(
            issuer,
            client_id,
            redirect_uri,
            scopes,
            *allow_insecure,
            &state.state_dir,
            gateway_oidc::LoginOptions {
                callback_listen: state.oidc_callback_listen,
                subscription_available: uses_subscription,
            },
        )
        .await
        .map(|acquired| {
            (
                acquired.credential,
                acquired.interactive && uses_subscription,
            )
        }),
        None => Err(anyhow::anyhow!(
            "LLM gateway has no authentication configured"
        )),
    }
    .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
    if uses_subscription {
        subscription::compose(
            identity,
            &state.state_dir,
            state.oidc_callback_listen,
            continue_in_browser,
        )
        .await
    } else {
        Ok(identity)
    }
    .map(Json)
    .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))
}

fn program_uses_subscription(config: &DaemonConfig, client_id: &str) -> bool {
    match client_id {
        "claude-code" => config
            .programs
            .claude_code
            .as_ref()
            .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription)),
        "claude-desktop" => config
            .programs
            .claude_desktop
            .as_ref()
            .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agentdesktop_core::{config::parse_daemon, model::LlmUsageRange};
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};

    use super::{
        fetch_llm_usage, fetch_llm_usage_interactions, load_effective_config,
        program_uses_subscription,
    };

    #[tokio::test]
    async fn fetches_and_combines_agentgateway_usage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/api/logs/analytics/summary",
                    post(|Json(request): Json<Value>| async move {
                        assert_eq!(
                            request["groupBy"],
                            json!([
                                { "field": "requestModel" },
                                { "field": "attributes", "key": "user_agent.name" }
                            ])
                        );
                        assert_eq!(request["bucketCount"], 1);
                        let timestamp_format = &time::format_description::well_known::Rfc3339;
                        let from = time::OffsetDateTime::parse(
                            request["timeRange"]["from"].as_str().unwrap(),
                            timestamp_format,
                        )
                        .unwrap();
                        let to = time::OffsetDateTime::parse(
                            request["timeRange"]["to"].as_str().unwrap(),
                            timestamp_format,
                        )
                        .unwrap();
                        assert_eq!(to - from, time::Duration::hours(1));
                        Json(json!({
                            "timeRange": {
                                "from": "2026-09-02T12:00:00Z",
                                "to": "2026-09-03T12:00:00Z"
                            },
                            "bucketSeconds": 86400,
                            "buckets": [],
                            "groups": [
                                {
                                    "group": {
                                        "requestModel": "claude-haiku-4-5",
                                        "user_agent.name": "claude-cli"
                                    },
                                    "requests": 2,
                                    "totalTokens": 1200,
                                    "cost": 0.012
                                },
                                {
                                    "group": {
                                        "requestModel": "claude-sonnet-4-5",
                                        "user_agent.name": "codex_cli_rs"
                                    },
                                    "requests": 1,
                                    "totalTokens": 300,
                                    "cost": 0.004
                                }
                            ],
                            "filterOptions": {}
                        }))
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let usage_url = format!("http://{address}/api/logs/analytics/summary")
            .parse()
            .unwrap();

        let usage = fetch_llm_usage(&usage_url, LlmUsageRange::Hour)
            .await
            .unwrap();

        assert_eq!(usage.requests, 3);
        assert_eq!(usage.total_tokens, 1500);
        assert!((usage.estimated_cost_usd - 0.016).abs() < f64::EPSILON);
        assert_eq!(usage.from, "2026-09-02T12:00:00Z");
        assert_eq!(usage.to, "2026-09-03T12:00:00Z");
        assert_eq!(usage.breakdown.len(), 2);
        assert_eq!(usage.breakdown[0].model, "claude-haiku-4-5");
        assert_eq!(usage.breakdown[0].agent, "claude-cli");
        assert_eq!(usage.breakdown[0].requests, 2);
        assert_eq!(usage.breakdown[0].total_tokens, 1200);
        assert!((usage.breakdown[0].estimated_cost_usd - 0.012).abs() < f64::EPSILON);
        server.abort();
    }

    #[tokio::test]
    async fn fetches_metadata_only_interactions_for_model_and_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/api/logs/search",
                    post(|Json(request): Json<Value>| async move {
                        assert_eq!(request["limit"], 25);
                        assert_eq!(request["cursor"], "next-page");
                        assert!(request.get("includeAttributes").is_none());
                        assert!(request.get("includePayload").is_none());
                        assert_eq!(
                            request["filters"],
                            json!({
                                "requestModel": ["gpt-5.6-sol"],
                                "attributes": {
                                    "user_agent.name": "GitHubCopilotChat"
                                }
                            })
                        );
                        assert_eq!(
                            request["timeRange"],
                            json!({
                                "from": "2026-09-07T05:00:00Z",
                                "to": "2026-09-07T06:00:00Z"
                            })
                        );
                        Json(json!({
                            "logs": [{
                                "id": "request-1",
                                "startedAt": "2026-09-07T06:09:23Z",
                                "completedAt": "2026-09-07T06:09:29Z",
                                "durationMs": 6038,
                                "httpStatus": 200,
                                "error": null,
                                "genAi": {
                                    "operationName": "chat",
                                    "providerName": "copilot",
                                    "requestModel": "gpt-5.6-sol",
                                    "responseModel": "gpt-5.6-sol"
                                },
                                "usage": {
                                    "inputTokens": 1200,
                                    "outputTokens": 300,
                                    "totalTokens": 1500
                                },
                                "cost": 0.012,
                                "hasPayload": false
                            }],
                            "nextCursor": "following-page"
                        }))
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let usage_url = format!("http://{address}/api/logs/analytics/summary")
            .parse()
            .unwrap();

        let result = fetch_llm_usage_interactions(
            &usage_url,
            "2026-09-07T05:00:00Z",
            "2026-09-07T06:00:00Z",
            "gpt-5.6-sol",
            "GitHubCopilotChat",
            Some("next-page"),
        )
        .await
        .unwrap();

        assert_eq!(result.next_cursor.as_deref(), Some("following-page"));
        assert_eq!(result.interactions.len(), 1);
        let interaction = &result.interactions[0];
        assert_eq!(interaction.id, "request-1");
        assert_eq!(interaction.agent, "GitHubCopilotChat");
        assert_eq!(interaction.request_model, "gpt-5.6-sol");
        assert!(!interaction.failed);
        assert_eq!(interaction.total_tokens, Some(1500));
        assert_eq!(interaction.estimated_cost_usd, Some(0.012));
        server.abort();
    }

    #[test]
    fn subscription_is_selected_by_requesting_agent() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode:
    auth: subscription
  claudeDesktop: {}
"#,
        )
        .unwrap();
        assert!(program_uses_subscription(&config, "claude-code"));
        assert!(!program_uses_subscription(&config, "claude-desktop"));
        assert!(!program_uses_subscription(&config, "codex"));
    }

    #[test]
    fn controller_configuration_is_the_effective_gateway_configuration() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-api-effective-config-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
"#,
        )
        .unwrap();
        fs::write(
            root.join("remote-config.yaml"),
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        assert_eq!(
            effective.llm_gateway.unwrap().url.as_str(),
            "https://gateway.example.com/"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standalone_configuration_ignores_stale_remote_configuration() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-api-standalone-config-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4001
  authentication:
    type: oidc
    issuer: http://127.0.0.1:5557/dex
    clientId: agentdesktop-local
    allowInsecure: true
"#,
        )
        .unwrap();
        fs::write(
            root.join("remote-config.yaml"),
            r#"
llmGateway:
  url: https://stale.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        let gateway = effective.llm_gateway.unwrap();
        assert_eq!(gateway.url.as_str(), "http://127.0.0.1:4001/");
        assert!(matches!(
            gateway.authentication,
            Some(agentdesktop_core::config::LlmGatewayAuthentication::Oidc { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
