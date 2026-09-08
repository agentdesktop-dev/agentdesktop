//! Read-only adapter for Agentgateway's request-log analytics API.
//!
//! The daemon uses it against a loopback gateway in standalone mode; the
//! controller uses it against the cluster-internal gateway admin service in
//! fleet mode. Callers decide the scope; this module never widens it.

use std::{collections::BTreeMap, time::Duration};

use anyhow::Context;
use serde::Deserialize;
use url::Url;

use crate::model::LlmUsageRange;

use crate::model::{
    LlmDeviceUsage, LlmFleetUsageSummary, LlmUsageBreakdown, LlmUsageInteraction,
    LlmUsageInteractions, LlmUsageSummary,
};

/// Agentgateway model catalog rates are expressed in USD.
pub const CURRENCY: &str = "USD";

/// Access-log attribute holding the enrolled device that sent a request.
pub const DEVICE_ID_ATTRIBUTE: &str = "device_id";

const USER_AGENT_ATTRIBUTE: &str = "user_agent.name";
const UNKNOWN_MODEL: &str = "Unknown model";
const UNKNOWN_AGENT: &str = "Unknown agent";
const INTERACTIONS_PAGE_SIZE: u32 = 25;
const MAX_INTERACTIONS_RANGE_DAYS: i64 = 31;

/// Restricts a query to the requests a caller is allowed to see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageScope {
    /// Only requests whose gateway credential carried this device identity.
    pub device_id: Option<String>,
}

impl UsageScope {
    pub fn device(device_id: impl Into<String>) -> Self {
        Self {
            device_id: Some(device_id.into()),
        }
    }

    fn filters(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut filters = serde_json::Map::new();
        if let Some(device_id) = &self.device_id {
            filters.insert(
                "attributes".to_owned(),
                serde_json::json!({ DEVICE_ID_ATTRIBUTE: device_id }),
            );
        }
        filters
    }
}

/// Trailing-window selector shared by the daemon and controller HTTP APIs.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    pub range: LlmUsageRange,
}

/// Bounded interaction page request for one model and agent combination.
#[derive(Clone, Debug, Deserialize)]
pub struct InteractionsQuery {
    pub from: String,
    pub to: String,
    pub model: String,
    pub agent: String,
    #[serde(default)]
    pub cursor: Option<String>,
}

impl InteractionsQuery {
    /// Validates client-supplied bounds before they reach the gateway.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.trim().is_empty() || self.model.len() > 256 {
            return Err("invalid model");
        }
        if self.agent.trim().is_empty() || self.agent.len() > 128 {
            return Err("invalid agent");
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > 512)
        {
            return Err("invalid cursor");
        }
        let timestamp_format = &time::format_description::well_known::Rfc3339;
        let from = time::OffsetDateTime::parse(&self.from, timestamp_format)
            .map_err(|_| "invalid usage start time")?;
        let to = time::OffsetDateTime::parse(&self.to, timestamp_format)
            .map_err(|_| "invalid usage end time")?;
        let duration = to - from;
        if duration <= time::Duration::ZERO
            || duration > time::Duration::days(MAX_INTERACTIONS_RANGE_DAYS)
        {
            return Err("usage time range must be between zero and 31 days");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct UsageClient {
    client: reqwest::Client,
    summary_url: Url,
    search_url: Url,
}

impl UsageClient {
    /// `summary_url` is the exact `/api/logs/analytics/summary` endpoint.
    pub fn new(summary_url: Url) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build LLM usage HTTP client")?;
        // Agentgateway serves `/api/logs/search` beside `/api/logs/analytics/summary`.
        let search_url = summary_url
            .join("../search")
            .context("derive LLM usage search URL")?;
        Ok(Self {
            client,
            summary_url,
            search_url,
        })
    }

    /// Totals and a model-by-agent breakdown for the trailing `range`.
    pub async fn summary(
        &self,
        range: LlmUsageRange,
        scope: &UsageScope,
    ) -> anyhow::Result<LlmUsageSummary> {
        let (from, to) = trailing_time_range(range)?;
        let analytics = self
            .analytics(serde_json::json!({
                "timeRange": { "from": from, "to": to },
                "filters": scope.filters(),
                "groupBy": [
                    { "field": "requestModel" },
                    { "field": "attributes", "key": USER_AGENT_ATTRIBUTE }
                ],
                "bucketCount": 1
            }))
            .await?;
        let (requests, total_tokens, estimated_cost) = analytics.totals();
        let mut breakdown: Vec<_> = analytics
            .groups
            .into_iter()
            .map(|group| LlmUsageBreakdown {
                model: group_value(&group.group, "requestModel", UNKNOWN_MODEL),
                agent: group_value(&group.group, USER_AGENT_ATTRIBUTE, UNKNOWN_AGENT),
                requests: group.requests,
                total_tokens: group.total_tokens,
                estimated_cost: group.cost,
            })
            .collect();
        breakdown.sort_by(|left, right| {
            right
                .estimated_cost
                .total_cmp(&left.estimated_cost)
                .then_with(|| left.model.cmp(&right.model))
                .then_with(|| left.agent.cmp(&right.agent))
        });
        Ok(LlmUsageSummary {
            from: analytics.time_range.from,
            to: analytics.time_range.to,
            currency: CURRENCY.to_owned(),
            requests,
            total_tokens,
            estimated_cost,
            breakdown,
        })
    }

    /// Totals and a per-device breakdown for the trailing `range` across every device.
    ///
    /// Hostnames are left empty; the controller fills them from its device inventory.
    pub async fn fleet_summary(
        &self,
        range: LlmUsageRange,
    ) -> anyhow::Result<LlmFleetUsageSummary> {
        let (from, to) = trailing_time_range(range)?;
        let analytics = self
            .analytics(serde_json::json!({
                "timeRange": { "from": from, "to": to },
                "groupBy": [
                    { "field": "attributes", "key": DEVICE_ID_ATTRIBUTE }
                ],
                "bucketCount": 1
            }))
            .await?;
        let (requests, total_tokens, estimated_cost) = analytics.totals();
        let mut devices: Vec<_> = analytics
            .groups
            .into_iter()
            .map(|group| LlmDeviceUsage {
                device_id: group
                    .group
                    .get(DEVICE_ID_ATTRIBUTE)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                hostname: None,
                requests: group.requests,
                total_tokens: group.total_tokens,
                estimated_cost: group.cost,
            })
            .collect();
        devices.sort_by(|left, right| {
            right
                .estimated_cost
                .total_cmp(&left.estimated_cost)
                .then_with(|| left.device_id.cmp(&right.device_id))
        });
        Ok(LlmFleetUsageSummary {
            from: analytics.time_range.from,
            to: analytics.time_range.to,
            currency: CURRENCY.to_owned(),
            requests,
            total_tokens,
            estimated_cost,
            devices,
        })
    }

    /// One page of metadata-only requests for a model and agent within `scope`.
    pub async fn interactions(
        &self,
        query: &InteractionsQuery,
        scope: &UsageScope,
    ) -> anyhow::Result<LlmUsageInteractions> {
        let mut filters = scope.filters();
        filters.insert("requestModel".to_owned(), serde_json::json!([query.model]));
        let attributes = filters
            .entry("attributes")
            .or_insert_with(|| serde_json::json!({}));
        attributes[USER_AGENT_ATTRIBUTE] = serde_json::Value::String(query.agent.clone());
        let response = self
            .client
            .post(self.search_url.clone())
            .json(&serde_json::json!({
                "limit": INTERACTIONS_PAGE_SIZE,
                "cursor": query.cursor,
                "timeRange": { "from": query.from, "to": query.to },
                "filters": filters
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
                agent: query.agent.clone(),
                request_model: log
                    .gen_ai
                    .request_model
                    .unwrap_or_else(|| query.model.clone()),
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
                estimated_cost: log.cost,
            })
            .collect();
        Ok(LlmUsageInteractions {
            currency: CURRENCY.to_owned(),
            interactions,
            next_cursor: response.next_cursor,
        })
    }

    async fn analytics(&self, request: serde_json::Value) -> anyhow::Result<AnalyticsResponse> {
        Ok(self
            .client
            .post(self.summary_url.clone())
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<AnalyticsResponse>()
            .await?)
    }
}

fn trailing_time_range(range: LlmUsageRange) -> anyhow::Result<(String, String)> {
    let to = time::OffsetDateTime::now_utc();
    let duration = time::Duration::try_from(range.duration())?;
    let from = to - duration;
    let timestamp_format = &time::format_description::well_known::Rfc3339;
    Ok((from.format(timestamp_format)?, to.format(timestamp_format)?))
}

fn group_value(group: &BTreeMap<String, serde_json::Value>, key: &str, fallback: &str) -> String {
    group
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsResponse {
    time_range: AnalyticsTimeRange,
    groups: Vec<AnalyticsGroup>,
}

impl AnalyticsResponse {
    /// `(requests, total_tokens, cost)` summed over every group.
    fn totals(&self) -> (u64, u64, f64) {
        self.groups
            .iter()
            .fold((0, 0, 0.0), |(requests, tokens, cost), group| {
                (
                    requests.saturating_add(group.requests),
                    tokens.saturating_add(group.total_tokens),
                    cost + group.cost,
                )
            })
    }
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

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    use super::{InteractionsQuery, UsageClient, UsageScope};
    use crate::model::LlmUsageRange;

    type Recorded = Arc<Mutex<Vec<Value>>>;

    fn interactions_query(cursor: Option<&str>) -> InteractionsQuery {
        InteractionsQuery {
            from: "2026-09-07T05:00:00Z".to_owned(),
            to: "2026-09-07T06:00:00Z".to_owned(),
            model: "gpt-5.6-sol".to_owned(),
            agent: "GitHubCopilotChat".to_owned(),
            cursor: cursor.map(str::to_owned),
        }
    }

    async fn serve(summary: Value, search: Value) -> (SocketAddr, Recorded) {
        let recorded: Recorded = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let summary_recorded = recorded.clone();
        let search_recorded = recorded.clone();
        tokio::spawn(async move {
            let router = Router::new()
                .route(
                    "/api/logs/analytics/summary",
                    post(move |Json(request): Json<Value>| {
                        let recorded = summary_recorded.clone();
                        let summary = summary.clone();
                        async move {
                            recorded.lock().await.push(request);
                            Json(summary)
                        }
                    }),
                )
                .route(
                    "/api/logs/search",
                    post(move |Json(request): Json<Value>| {
                        let recorded = search_recorded.clone();
                        let search = search.clone();
                        async move {
                            recorded.lock().await.push(request);
                            Json(search)
                        }
                    }),
                );
            axum::serve(listener, router).await.unwrap();
        });
        (address, recorded)
    }

    fn client(address: SocketAddr) -> UsageClient {
        UsageClient::new(
            format!("http://{address}/api/logs/analytics/summary")
                .parse()
                .unwrap(),
        )
        .unwrap()
    }

    fn summary_response() -> Value {
        json!({
            "timeRange": { "from": "2026-09-02T12:00:00Z", "to": "2026-09-03T12:00:00Z" },
            "bucketSeconds": 86400,
            "buckets": [],
            "groups": [
                {
                    "group": { "requestModel": "claude-haiku-4-5", "user_agent.name": "claude-cli" },
                    "requests": 2,
                    "totalTokens": 1200,
                    "cost": 0.012
                },
                {
                    "group": { "requestModel": "claude-sonnet-4-5", "user_agent.name": "codex_cli_rs" },
                    "requests": 1,
                    "totalTokens": 300,
                    "cost": 0.004
                }
            ],
            "filterOptions": {}
        })
    }

    #[tokio::test]
    async fn summary_combines_groups_and_uses_trailing_range() {
        let (address, recorded) = serve(summary_response(), json!({})).await;

        let usage = client(address)
            .summary(LlmUsageRange::Hour, &UsageScope::default())
            .await
            .unwrap();

        let request = recorded.lock().await.pop().unwrap();
        assert_eq!(
            request["groupBy"],
            json!([
                { "field": "requestModel" },
                { "field": "attributes", "key": "user_agent.name" }
            ])
        );
        assert_eq!(request["bucketCount"], 1);
        assert_eq!(request["filters"], json!({}));
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

        assert_eq!(usage.requests, 3);
        assert_eq!(usage.total_tokens, 1500);
        assert_eq!(usage.currency, "USD");
        assert!((usage.estimated_cost - 0.016).abs() < f64::EPSILON);
        assert_eq!(usage.from, "2026-09-02T12:00:00Z");
        assert_eq!(usage.to, "2026-09-03T12:00:00Z");
        assert_eq!(usage.breakdown.len(), 2);
        assert_eq!(usage.breakdown[0].model, "claude-haiku-4-5");
        assert_eq!(usage.breakdown[0].agent, "claude-cli");
        assert_eq!(usage.breakdown[0].requests, 2);
        assert_eq!(usage.breakdown[0].total_tokens, 1200);
        assert!((usage.breakdown[0].estimated_cost - 0.012).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn device_scope_filters_every_query_by_device_attribute() {
        let (address, recorded) = serve(
            summary_response(),
            json!({ "logs": [], "nextCursor": null }),
        )
        .await;
        let client = client(address);
        let scope = UsageScope::device("7ca03414-bb20-4c80-98ef-7b0538b988ba");

        client.summary(LlmUsageRange::Day, &scope).await.unwrap();
        client
            .interactions(&interactions_query(None), &scope)
            .await
            .unwrap();

        let recorded = recorded.lock().await;
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded[0]["filters"],
            json!({ "attributes": { "device_id": "7ca03414-bb20-4c80-98ef-7b0538b988ba" } })
        );
        assert_eq!(
            recorded[1]["filters"],
            json!({
                "requestModel": ["gpt-5.6-sol"],
                "attributes": {
                    "device_id": "7ca03414-bb20-4c80-98ef-7b0538b988ba",
                    "user_agent.name": "GitHubCopilotChat"
                }
            })
        );
    }

    #[tokio::test]
    async fn fleet_summary_groups_by_device_and_keeps_unattributed_rows() {
        let (address, recorded) = serve(
            json!({
                "timeRange": { "from": "2026-09-02T12:00:00Z", "to": "2026-09-03T12:00:00Z" },
                "bucketSeconds": 86400,
                "buckets": [],
                "groups": [
                    { "group": { "device_id": "device-a" }, "requests": 4, "totalTokens": 4000, "cost": 0.04 },
                    { "group": { "device_id": "device-b" }, "requests": 9, "totalTokens": 9000, "cost": 0.09 },
                    { "group": { "device_id": null }, "requests": 1, "totalTokens": 10, "cost": 0.0 }
                ],
                "filterOptions": {}
            }),
            json!({}),
        )
        .await;

        let usage = client(address)
            .fleet_summary(LlmUsageRange::Week)
            .await
            .unwrap();

        let request = recorded.lock().await.pop().unwrap();
        assert_eq!(
            request["groupBy"],
            json!([{ "field": "attributes", "key": "device_id" }])
        );
        assert!(request.get("filters").is_none());
        assert_eq!(usage.requests, 14);
        assert_eq!(usage.total_tokens, 13_010);
        assert!((usage.estimated_cost - 0.13).abs() < 1e-9);
        assert_eq!(usage.devices.len(), 3);
        assert_eq!(usage.devices[0].device_id.as_deref(), Some("device-b"));
        assert_eq!(usage.devices[1].device_id.as_deref(), Some("device-a"));
        assert_eq!(usage.devices[2].device_id, None);
        assert!(usage.devices.iter().all(|device| device.hostname.is_none()));
    }

    #[tokio::test]
    async fn interactions_map_metadata_only_rows() {
        let (address, recorded) = serve(
            json!({}),
            json!({
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
                    "usage": { "inputTokens": 1200, "outputTokens": 300, "totalTokens": 1500 },
                    "cost": 0.012,
                    "hasPayload": false
                }],
                "nextCursor": "following-page"
            }),
        )
        .await;

        let result = client(address)
            .interactions(
                &interactions_query(Some("next-page")),
                &UsageScope::default(),
            )
            .await
            .unwrap();

        let request = recorded.lock().await.pop().unwrap();
        assert_eq!(request["limit"], 25);
        assert_eq!(request["cursor"], "next-page");
        assert!(request.get("includeAttributes").is_none());
        assert!(request.get("includePayload").is_none());
        assert_eq!(
            request["filters"],
            json!({
                "requestModel": ["gpt-5.6-sol"],
                "attributes": { "user_agent.name": "GitHubCopilotChat" }
            })
        );
        assert_eq!(
            request["timeRange"],
            json!({ "from": "2026-09-07T05:00:00Z", "to": "2026-09-07T06:00:00Z" })
        );
        assert_eq!(result.currency, "USD");
        assert_eq!(result.next_cursor.as_deref(), Some("following-page"));
        assert_eq!(result.interactions.len(), 1);
        let interaction = &result.interactions[0];
        assert_eq!(interaction.id, "request-1");
        assert_eq!(interaction.agent, "GitHubCopilotChat");
        assert_eq!(interaction.request_model, "gpt-5.6-sol");
        assert!(!interaction.failed);
        assert_eq!(interaction.total_tokens, Some(1500));
        assert_eq!(interaction.estimated_cost, Some(0.012));
    }

    #[test]
    fn interactions_query_bounds_are_enforced() {
        let valid = interactions_query(None);
        assert!(valid.validate().is_ok());
        assert!(
            InteractionsQuery {
                model: " ".to_owned(),
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            InteractionsQuery {
                to: "2026-09-07T05:00:00Z".to_owned(),
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            InteractionsQuery {
                from: "2026-07-01T00:00:00Z".to_owned(),
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            interactions_query(Some(&"x".repeat(513)))
                .validate()
                .is_err()
        );
    }
}
