//! agentgateway access-log records received over OTLP/gRPC.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use agentdesktop_core::config::valid_client_id;
use agentdesktop_proto::otlp_logs::{
    AnyValue, ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
    LogRecord,
    any_value::Value,
    logs_service_server::{LogsService, LogsServiceServer},
};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use tonic::{
    Request, Response, Status,
    service::{Interceptor, interceptor::InterceptedService},
};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::{
    Billing, DAILY_RETENTION, HOUR, Measures, RollupKey, UsageRecord, UsageStore, store, unix_now,
};

/// Shortest accepted export token.
const MIN_TOKEN_LENGTH: usize = 32;
const MAX_LABEL_BYTES: usize = 128;
const MAX_SUBJECT_BYTES: usize = 256;
const MAX_TOKENS_PER_REQUEST: i64 = 1_000_000_000;
const MAX_COST_NANOS_PER_REQUEST: i64 = 1_000_000_000_000_000;
/// A record stamped further ahead than this is counted when received.
const MAX_CLOCK_SKEW: i64 = 5 * 60;

// Attributes of agentgateway's access-log record.
const PROVIDER: &str = "gen_ai.provider.name";
const REQUEST_MODEL: &str = "gen_ai.request.model";
const RESPONSE_MODEL: &str = "gen_ai.response.model";
const INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
const OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
const CACHE_READ_TOKENS: &str = "gen_ai.usage.cache_read.input_tokens";
const CACHE_WRITE_TOKENS: &str = "gen_ai.usage.cache_creation.input_tokens";
const COST: &str = "agw.ai.usage.cost.total";
const STATUS: &str = "http.response.status_code";
const ERROR: &str = "error";
const JWT_SUBJECT: &str = "jwt.sub";
const USER_AGENT: &str = "user_agent.original";
// Fields the agentgateway configuration adds from the controller JWT.
const SUBJECT: &str = "agentdesktop.subject";
const DEVICE: &str = "agentdesktop.device";
const CLIENT: &str = "agentdesktop.client";
const BILLING: &str = "agentdesktop.billing";

/// Tools recognized by their user agent when the credential does not name
/// the tool, as with an OIDC token on a gateway without a controller.
const USER_AGENT_TOOLS: [(&str, &str); 2] = [("claude-cli/", "claude-code"), ("codex_", "codex")];

/// Receives agentgateway access logs and stores the usage of LLM requests.
#[derive(Clone)]
pub struct UsageIngest {
    pub(super) store: UsageStore,
}

impl UsageIngest {
    /// The OTLP logs service, accepting only exports that carry `token`.
    pub fn service(
        store: UsageStore,
        token: &str,
    ) -> InterceptedService<LogsServiceServer<Self>, UsageTokenCheck> {
        LogsServiceServer::with_interceptor(Self { store }, UsageTokenCheck::new(token))
    }
}

#[tonic::async_trait]
impl LogsService for UsageIngest {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let now = unix_now();
        let mut records = Vec::new();
        let mut rejected = 0_i64;
        let mut first_error = None;
        for record in request
            .get_ref()
            .resource_logs
            .iter()
            .flat_map(|resource| &resource.scope_logs)
            .flat_map(|scope| &scope.log_records)
        {
            match parse_record(record, now) {
                Ok(Some(usage)) => records.push(usage),
                Ok(None) => {}
                Err(error) => {
                    rejected += 1;
                    first_error.get_or_insert(error);
                }
            }
        }
        let stored = store::ingest(&self.store, &records, now)
            .await
            .map_err(|error| {
                error!(error = %format!("{error:#}"), "could not store usage records");
                Status::unavailable("usage storage is unavailable")
            })?;
        debug!(received = records.len(), stored, "stored usage records");
        if let Some(error) = &first_error {
            warn!(rejected, %error, "rejected usage records");
        }
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: first_error.map(|error_message| ExportLogsPartialSuccess {
                rejected_log_records: rejected,
                error_message,
            }),
        }))
    }
}

/// Rejects exports without the configured bearer token.
#[derive(Clone)]
pub struct UsageTokenCheck {
    digest: Arc<[u8]>,
}

impl UsageTokenCheck {
    fn new(token: &str) -> Self {
        Self {
            digest: Arc::from(Sha256::digest(token.as_bytes()).as_slice()),
        }
    }
}

impl Interceptor for UsageTokenCheck {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("missing usage export token"))?;
        // Comparing digests keeps the comparison constant-time in the length too.
        let digest = Sha256::digest(token.as_bytes());
        aws_lc_rs::constant_time::verify_slices_are_equal(digest.as_slice(), &self.digest)
            .map_err(|_| Status::unauthenticated("invalid usage export token"))?;
        Ok(request)
    }
}

/// Reads the bearer token agentgateway sends with usage exports.
pub fn load_token(path: &Path) -> anyhow::Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read usage token from {}", path.display()))?;
    let token = contents.trim();
    if token.len() < MIN_TOKEN_LENGTH {
        anyhow::bail!(
            "usage token in {} must be at least {MIN_TOKEN_LENGTH} characters",
            path.display()
        );
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        anyhow::bail!(
            "usage token in {} must be printable ASCII without spaces",
            path.display()
        );
    }
    Ok(token.to_owned())
}

/// Reads the usage of one access-log record. Requests that are not LLM calls
/// are `Ok(None)`; malformed records are an error.
fn parse_record(record: &LogRecord, now: i64) -> Result<Option<UsageRecord>, String> {
    let attributes: BTreeMap<&str, &Value> = record
        .attributes
        .iter()
        .filter_map(|attribute| {
            Some((
                attribute.key.as_str(),
                attribute.value.as_ref()?.value.as_ref()?,
            ))
        })
        .collect();
    let text = |key: &str| match attributes.get(key) {
        Some(Value::StringValue(text)) => Some(text.as_str()),
        _ => None,
    };
    let Some(provider) = text(PROVIDER).filter(|provider| !provider.is_empty()) else {
        return Ok(None);
    };
    let model = text(RESPONSE_MODEL)
        .filter(|model| !model.is_empty())
        .or_else(|| text(REQUEST_MODEL))
        .unwrap_or_default();
    let subject = text(SUBJECT)
        .or_else(|| text(JWT_SUBJECT))
        .unwrap_or_default();
    let device_id = text(DEVICE)
        .and_then(|device| Uuid::parse_str(device).ok())
        .map(|device| device.to_string())
        .unwrap_or_default();
    let client_id = text(CLIENT)
        .filter(|client| valid_client_id(client))
        .or_else(|| {
            let user_agent = text(USER_AGENT)?;
            USER_AGENT_TOOLS
                .iter()
                .find(|(prefix, _)| user_agent.starts_with(prefix))
                .map(|(_, tool)| *tool)
        })
        .unwrap_or_default();
    let billing = if text(BILLING) == Some("subscription") {
        Billing::Subscription
    } else {
        Billing::Api
    };
    let count = |key: &str| match attributes.get(key) {
        None => Ok(0),
        Some(value) => integer(value)
            .filter(|count| (0..=MAX_TOKENS_PER_REQUEST).contains(count))
            .ok_or_else(|| {
                format!("{key} must be an integer between 0 and {MAX_TOKENS_PER_REQUEST}")
            }),
    };
    let input_tokens = count(INPUT_TOKENS)?;
    let output_tokens = count(OUTPUT_TOKENS)?;
    let cost_nanos = attributes
        .get(COST)
        .map(|value| {
            cost_nanos(value)
                .filter(|amount| *amount <= MAX_COST_NANOS_PER_REQUEST)
                .ok_or_else(|| {
                    format!("{COST} must be a dollar amount no greater than $1,000,000 per request")
                })
        })
        .transpose()?;
    let status = attributes
        .get(STATUS)
        .and_then(|value| integer(value))
        .unwrap_or_default();
    let error = attributes
        .get(ERROR)
        .is_some_and(|value| !matches!(value, Value::StringValue(text) if text.is_empty()));
    let occurred_at = occurred_at(record, now)?;
    Ok(Some(UsageRecord {
        id: record_id(record),
        key: RollupKey {
            bucket_start: occurred_at.div_euclid(HOUR) * HOUR,
            subject: label(subject, MAX_SUBJECT_BYTES)?,
            device_id,
            client_id: client_id.to_owned(),
            provider: label(provider, MAX_LABEL_BYTES)?,
            model: label(model, MAX_LABEL_BYTES)?,
            billing,
        },
        measures: Measures {
            requests: 1,
            errors: i64::from(error || status >= 400),
            input_tokens,
            output_tokens,
            cache_read_tokens: count(CACHE_READ_TOKENS)?,
            cache_write_tokens: count(CACHE_WRITE_TOKENS)?,
            cost_nanos: cost_nanos.unwrap_or_default(),
            // A failed request without tokens costs nothing; it is not unpriced.
            unpriced_requests: i64::from(
                cost_nanos.is_none() && input_tokens.saturating_add(output_tokens) > 0,
            ),
        },
    }))
}

fn label(value: &str, max_bytes: usize) -> Result<String, String> {
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Ok(String::new());
    }
    Ok(value.to_owned())
}

fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::IntValue(number) => Some(*number),
        Value::DoubleValue(number)
            if number.is_finite() && number.fract() == 0.0 && number.abs() < 9.0e15 =>
        {
            Some(*number as i64)
        }
        Value::StringValue(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// A non-negative US dollar amount in billionths, rounded to the nearest.
fn cost_nanos(value: &Value) -> Option<i64> {
    match value {
        Value::StringValue(text) => decimal_nanos(text.trim()),
        Value::DoubleValue(dollars) => dollars_to_nanos(*dollars),
        Value::IntValue(dollars) => dollars
            .checked_mul(1_000_000_000)
            .filter(|nanos| *nanos >= 0),
        _ => None,
    }
}

/// Parses a plain decimal exactly; other notations go through `f64`.
fn decimal_nanos(text: &str) -> Option<i64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let plain = !(whole.is_empty() && fraction.is_empty())
        && whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit());
    if !plain {
        return text.parse::<f64>().ok().and_then(dollars_to_nanos);
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let digits = fraction.as_bytes();
    let mut nanos = 0_i64;
    for index in 0..9 {
        nanos = nanos * 10 + digits.get(index).map_or(0, |digit| i64::from(digit - b'0'));
    }
    let round_up = digits.get(9).is_some_and(|digit| *digit >= b'5');
    whole
        .checked_mul(1_000_000_000)?
        .checked_add(nanos + i64::from(round_up))
}

fn dollars_to_nanos(dollars: f64) -> Option<i64> {
    let nanos = (dollars * 1e9).round();
    (nanos.is_finite() && (0.0..9.0e18).contains(&nanos)).then_some(nanos as i64)
}

fn occurred_at(record: &LogRecord, now: i64) -> Result<i64, String> {
    let nanos = if record.time_unix_nano == 0 {
        record.observed_time_unix_nano
    } else {
        record.time_unix_nano
    };
    if nanos == 0 {
        return Ok(now);
    }
    let seconds = i64::try_from(nanos / 1_000_000_000).unwrap_or(i64::MAX);
    if seconds > now.saturating_add(MAX_CLOCK_SKEW) {
        return Ok(now);
    }
    if seconds < now - DAILY_RETENTION {
        return Err("record is older than usage retention".to_owned());
    }
    Ok(seconds)
}

/// Digest of everything in the record, so the same record sent twice has the
/// same identifier.
fn record_id(record: &LogRecord) -> String {
    let mut hash = Sha256::new();
    hash.update(record.time_unix_nano.to_be_bytes());
    hash.update(record.observed_time_unix_nano.to_be_bytes());
    hash_bytes(&mut hash, &record.trace_id);
    hash_bytes(&mut hash, &record.span_id);
    let mut attributes: Vec<_> = record.attributes.iter().collect();
    attributes.sort_by(|left, right| left.key.cmp(&right.key));
    hash.update((attributes.len() as u64).to_be_bytes());
    for attribute in attributes {
        hash_bytes(&mut hash, attribute.key.as_bytes());
        hash_value(&mut hash, attribute.value.as_ref());
    }
    hash_value(&mut hash, record.body.as_ref());
    URL_SAFE_NO_PAD.encode(hash.finalize())
}

fn hash_bytes(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn hash_value(hash: &mut Sha256, value: Option<&AnyValue>) {
    match value.and_then(|value| value.value.as_ref()) {
        None => hash.update([0_u8]),
        Some(Value::StringValue(text)) => {
            hash.update([1_u8]);
            hash_bytes(hash, text.as_bytes());
        }
        Some(Value::BoolValue(flag)) => hash.update([2_u8, u8::from(*flag)]),
        Some(Value::IntValue(number)) => {
            hash.update([3_u8]);
            hash.update(number.to_be_bytes());
        }
        Some(Value::DoubleValue(number)) => {
            hash.update([4_u8]);
            hash.update(number.to_bits().to_be_bytes());
        }
        Some(Value::ArrayValue(array)) => {
            hash.update([5_u8]);
            hash.update((array.values.len() as u64).to_be_bytes());
            for item in &array.values {
                hash_value(hash, Some(item));
            }
        }
        Some(Value::KvlistValue(list)) => {
            hash.update([6_u8]);
            hash.update((list.values.len() as u64).to_be_bytes());
            for entry in &list.values {
                hash_bytes(hash, entry.key.as_bytes());
                hash_value(hash, entry.value.as_ref());
            }
        }
        Some(Value::BytesValue(bytes)) => {
            hash.update([7_u8]);
            hash_bytes(hash, bytes);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use agentdesktop_proto::otlp_logs::{AnyValue, KeyValue, LogRecord, any_value::Value};
    use tonic::{Code, Request, service::Interceptor};

    use super::{
        Billing, HOUR, MAX_TOKENS_PER_REQUEST, UsageTokenCheck, decimal_nanos, parse_record,
    };

    pub(crate) const DEVICE_ID: &str = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
    // 2026-03-10T15:20:00Z
    pub(crate) const NOW: i64 = 1_773_156_000;

    pub(crate) fn attribute(key: &str, value: Value) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        }
    }

    pub(crate) fn text(value: &str) -> Value {
        Value::StringValue(value.to_owned())
    }

    /// An access-log record for a priced Anthropic request.
    pub(crate) fn llm_record(occurred_at: i64, extra: Vec<KeyValue>) -> LogRecord {
        let mut attributes = vec![
            attribute("gen_ai.provider.name", text("anthropic")),
            attribute("gen_ai.request.model", text("claude-sonnet-4-5")),
            attribute("gen_ai.response.model", text("claude-sonnet-4-5-20250929")),
            attribute("gen_ai.usage.input_tokens", Value::IntValue(1200)),
            attribute("gen_ai.usage.output_tokens", Value::IntValue(300)),
            attribute("gen_ai.usage.cache_read.input_tokens", Value::IntValue(800)),
            attribute("agw.ai.usage.cost.total", text("0.0081")),
            attribute("http.response.status_code", Value::IntValue(200)),
            attribute("agentdesktop.subject", text("user-123")),
            attribute("agentdesktop.device", text(DEVICE_ID)),
            attribute("agentdesktop.client", text("claude-code")),
        ];
        attributes.extend(extra);
        LogRecord {
            observed_time_unix_nano: u64::try_from(occurred_at).unwrap() * 1_000_000_000,
            attributes,
            ..Default::default()
        }
    }

    /// A record as a gateway without a controller sends it: an OIDC subject
    /// and the caller's user agent, but no device or tool.
    pub(crate) fn standalone_record(occurred_at: i64, user_agent: &str) -> LogRecord {
        let mut record = llm_record(occurred_at, Vec::new());
        record
            .attributes
            .retain(|attribute| !attribute.key.starts_with("agentdesktop."));
        record.attributes.extend([
            attribute("jwt.sub", text("CgVhbGljZRIFbG9jYWw")),
            attribute("user_agent.original", text(user_agent)),
        ]);
        record
    }

    #[test]
    fn reads_an_llm_request() {
        let usage = parse_record(&llm_record(NOW, Vec::new()), NOW)
            .unwrap()
            .expect("an LLM request");
        assert_eq!(usage.key.bucket_start, NOW.div_euclid(HOUR) * HOUR);
        assert_eq!(usage.key.subject, "user-123");
        assert_eq!(usage.key.device_id, DEVICE_ID);
        assert_eq!(usage.key.client_id, "claude-code");
        assert_eq!(usage.key.provider, "anthropic");
        assert_eq!(
            usage.key.model, "claude-sonnet-4-5-20250929",
            "the response model is the one billed"
        );
        assert_eq!(usage.key.billing, Billing::Api);
        assert_eq!(usage.measures.requests, 1);
        assert_eq!(usage.measures.errors, 0);
        assert_eq!(usage.measures.input_tokens, 1200);
        assert_eq!(usage.measures.output_tokens, 300);
        assert_eq!(usage.measures.cache_read_tokens, 800);
        assert_eq!(usage.measures.cache_write_tokens, 0);
        assert_eq!(usage.measures.cost_nanos, 8_100_000);
        assert_eq!(usage.measures.unpriced_requests, 0);
    }

    #[test]
    fn skips_requests_that_are_not_llm_calls() {
        let record = LogRecord {
            observed_time_unix_nano: 1,
            attributes: vec![attribute("http.response.status_code", Value::IntValue(200))],
            ..Default::default()
        };
        assert_eq!(parse_record(&record, NOW), Ok(None));
    }

    #[test]
    fn marks_subscription_failed_and_unpriced_requests() {
        let mut record = llm_record(
            NOW,
            vec![
                attribute("agentdesktop.billing", text("subscription")),
                attribute("http.response.status_code", Value::IntValue(429)),
            ],
        );
        record
            .attributes
            .retain(|attribute| attribute.key != "agw.ai.usage.cost.total");
        let usage = parse_record(&record, NOW).unwrap().unwrap();
        assert_eq!(usage.key.billing, Billing::Subscription);
        assert_eq!(usage.measures.errors, 1);
        assert_eq!(usage.measures.unpriced_requests, 1);
    }

    #[test]
    fn unattributable_values_are_left_empty_and_bad_measures_rejected() {
        let usage = parse_record(
            &llm_record(
                NOW,
                vec![
                    attribute("agentdesktop.device", text("not-a-device")),
                    attribute("agentdesktop.client", text("not a client")),
                ],
            ),
            NOW,
        )
        .unwrap()
        .unwrap();
        assert_eq!(usage.key.device_id, "");
        assert_eq!(usage.key.client_id, "");

        for bad in [
            attribute("gen_ai.usage.input_tokens", Value::IntValue(-1)),
            attribute(
                "gen_ai.usage.input_tokens",
                Value::IntValue(MAX_TOKENS_PER_REQUEST + 1),
            ),
            attribute("agw.ai.usage.cost.total", text("-0.5")),
            attribute("agw.ai.usage.cost.total", text("free")),
            attribute("agw.ai.usage.cost.total", text("1000001")),
        ] {
            let key = bad.key.clone();
            let mut record = llm_record(NOW, Vec::new());
            record.attributes.retain(|attribute| attribute.key != key);
            record.attributes.push(bad);
            assert!(
                parse_record(&record, NOW).is_err(),
                "{key} must be rejected"
            );
        }
    }

    #[test]
    fn invalid_labels_do_not_drop_usage_measures() {
        let usage = parse_record(
            &llm_record(
                NOW,
                vec![
                    attribute("gen_ai.response.model", text(&"m".repeat(129))),
                    attribute("agentdesktop.subject", text("bad\nsubject")),
                ],
            ),
            NOW,
        )
        .unwrap()
        .unwrap();
        assert_eq!(usage.key.model, "");
        assert_eq!(usage.key.subject, "");
        assert_eq!(usage.measures.input_tokens, 1200);
        assert_eq!(usage.measures.cost_nanos, 8_100_000);
    }

    #[test]
    fn tools_without_a_client_claim_are_recognized_by_user_agent() {
        for (user_agent, tool) in [
            ("claude-cli/2.1.260 (external, sdk-cli)", "claude-code"),
            ("claude-cli/2.1.260 (external, cli)", "claude-code"),
            (
                "codex_exec/0.153.4 (Mac OS 26.5.2; arm64) vscode/1.140.0 (codex_exec; 0.153.4)",
                "codex",
            ),
            (
                "codex_cli_rs/0.153.4 (Mac OS 26.5.2; arm64) iTerm.app/3.6.6",
                "codex",
            ),
            ("curl/8.7.1", ""),
        ] {
            let usage = parse_record(&standalone_record(NOW, user_agent), NOW)
                .unwrap()
                .unwrap();
            assert_eq!(usage.key.client_id, tool, "{user_agent}");
            assert_eq!(usage.key.subject, "CgVhbGljZRIFbG9jYWw");
            assert_eq!(usage.key.device_id, "");
        }

        let claimed = parse_record(
            &llm_record(
                NOW,
                vec![attribute(
                    "user_agent.original",
                    text("codex_cli_rs/0.153.4"),
                )],
            ),
            NOW,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            claimed.key.client_id, "claude-code",
            "the credential's client wins over the user agent"
        );
    }

    #[test]
    fn timestamps_are_clamped_to_now_and_retention() {
        let future = parse_record(&llm_record(NOW + 3600, Vec::new()), NOW)
            .unwrap()
            .unwrap();
        assert_eq!(future.key.bucket_start, NOW.div_euclid(HOUR) * HOUR);
        assert!(parse_record(&llm_record(NOW - 400 * 24 * HOUR, Vec::new()), NOW).is_err());
    }

    #[test]
    fn the_same_record_has_the_same_identifier() {
        let first = parse_record(&llm_record(NOW, Vec::new()), NOW)
            .unwrap()
            .unwrap();
        let again = parse_record(&llm_record(NOW, Vec::new()), NOW + 60)
            .unwrap()
            .unwrap();
        let later = parse_record(&llm_record(NOW + 1, Vec::new()), NOW + 60)
            .unwrap()
            .unwrap();
        assert_eq!(first.id, again.id);
        assert_ne!(first.id, later.id);
    }

    #[test]
    fn dollar_amounts_become_exact_nanos() {
        assert_eq!(decimal_nanos("0.0081"), Some(8_100_000));
        assert_eq!(decimal_nanos("12"), Some(12_000_000_000));
        assert_eq!(decimal_nanos(".5"), Some(500_000_000));
        assert_eq!(decimal_nanos("0.0000000015"), Some(2));
        assert_eq!(decimal_nanos("1.5e-6"), Some(1_500));
        assert_eq!(decimal_nanos(""), None);
        assert_eq!(decimal_nanos("."), None);
        assert_eq!(decimal_nanos("-1"), None);
    }

    #[test]
    fn exports_need_the_configured_token() {
        let token = "0123456789abcdef0123456789abcdef";
        let mut check = UsageTokenCheck::new(token);
        let request = |authorization: Option<&str>| {
            let mut request = Request::new(());
            if let Some(authorization) = authorization {
                request
                    .metadata_mut()
                    .insert("authorization", authorization.parse().unwrap());
            }
            request
        };
        assert!(
            check
                .call(request(Some(&format!("Bearer {token}"))))
                .is_ok()
        );
        for rejected in [
            None,
            Some(token),
            Some("Bearer 0123456789abcdef0123456789abcdee"),
            Some("Bearer "),
        ] {
            let status = check.call(request(rejected)).expect_err("rejected token");
            assert_eq!(status.code(), Code::Unauthenticated);
        }
    }
}
