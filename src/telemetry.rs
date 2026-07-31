use anyhow::{Result, anyhow};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Level;
use tracing_subscriber::prelude::*;
use url::Url;

const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

#[derive(Clone, Debug)]
pub struct TraceContext {
    traceparent: HeaderValue,
    parent: Context,
}

impl TraceContext {
    pub fn apply_to_response(&self, headers: &mut HeaderMap) {
        headers.insert(TRACEPARENT, self.traceparent.clone());
    }

    pub fn parent(&self) -> Context {
        self.parent.clone()
    }
}

pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

pub fn init() -> Result<TelemetryGuard> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::INFO)
        .with_current_span(false)
        .with_span_list(false)
        .finish();
    let Some(endpoint) = otlp_endpoint(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())? else {
        subscriber
            .try_init()
            .map_err(|error| anyhow!("failed to initialize structured logging: {error}"))?;
        return Ok(TelemetryGuard { provider: None });
    };
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.as_str())
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder_empty()
                .with_attributes([
                    KeyValue::new("service.name", env!("CARGO_PKG_NAME")),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                ])
                .build(),
        )
        .build();
    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
    subscriber
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()
        .map_err(|error| anyhow!("failed to initialize structured logging and tracing: {error}"))?;
    Ok(TelemetryGuard {
        provider: Some(provider),
    })
}

fn otlp_endpoint(value: Option<String>) -> Result<Option<Url>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let endpoint = Url::parse(&value)?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        anyhow::bail!("OTEL_EXPORTER_OTLP_ENDPOINT must use http or https");
    }
    Ok(Some(endpoint))
}

pub fn ensure_trace_context(headers: &mut HeaderMap) -> TraceContext {
    let valid = headers
        .get(TRACEPARENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(valid_traceparent);
    if !valid {
        headers.remove(TRACESTATE);
        headers.insert(TRACEPARENT, new_traceparent());
    }
    let parent =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    TraceContext {
        traceparent: headers
            .get(TRACEPARENT)
            .expect("traceparent was retained or generated")
            .clone(),
        parent,
    }
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(HeaderName::as_str).collect()
    }
}

fn valid_traceparent(value: &str) -> bool {
    if value.len() != 55 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return false;
    }
    let version = &value[0..2];
    let trace_id = &value[3..35];
    let parent_id = &value[36..52];
    let flags = &value[53..55];
    version == "00"
        && is_lower_hex(trace_id)
        && is_lower_hex(parent_id)
        && is_lower_hex(flags)
        && trace_id.bytes().any(|byte| byte != b'0')
        && parent_id.bytes().any(|byte| byte != b'0')
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn new_traceparent() -> HeaderValue {
    let mut identifier = [0_u8; 24];
    getrandom::fill(&mut identifier).expect("operating-system randomness is unavailable");
    let trace_id = encode_hex(&identifier[..16]);
    let parent_id = encode_hex(&identifier[16..]);
    HeaderValue::from_str(&format!("00-{trace_id}-{parent_id}-01"))
        .expect("generated W3C traceparent is a valid header")
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{ensure_trace_context, otlp_endpoint};

    #[test]
    fn validates_optional_otlp_endpoint() {
        assert!(otlp_endpoint(None).unwrap().is_none());
        assert_eq!(
            otlp_endpoint(Some("http://127.0.0.1:4317".to_owned()))
                .unwrap()
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4317/"
        );
        assert!(otlp_endpoint(Some("file:///tmp/traces".to_owned())).is_err());
    }

    #[test]
    fn preserves_valid_context_and_tracestate() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        headers.insert("tracestate", HeaderValue::from_static("vendor=value"));

        let context = ensure_trace_context(&mut headers);
        let mut response = HeaderMap::new();
        context.apply_to_response(&mut response);

        assert_eq!(response["traceparent"], headers["traceparent"]);
        assert_eq!(headers["tracestate"], "vendor=value");
    }

    #[test]
    fn replaces_invalid_context_and_drops_untrusted_tracestate() {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", HeaderValue::from_static("invalid"));
        headers.insert("tracestate", HeaderValue::from_static("vendor=untrusted"));

        ensure_trace_context(&mut headers);

        assert_eq!(headers["traceparent"].to_str().unwrap().len(), 55);
        assert!(headers.get("tracestate").is_none());
    }
}
