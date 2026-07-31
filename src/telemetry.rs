use anyhow::{Result, anyhow};
use axum::http::{HeaderMap, HeaderValue};
use tracing::Level;

const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceContext {
    traceparent: HeaderValue,
}

impl TraceContext {
    pub fn apply_to_response(&self, headers: &mut HeaderMap) {
        headers.insert(TRACEPARENT, self.traceparent.clone());
    }
}

pub fn init() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::INFO)
        .with_current_span(false)
        .with_span_list(false)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize structured logging: {error}"))
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
    TraceContext {
        traceparent: headers
            .get(TRACEPARENT)
            .expect("traceparent was retained or generated")
            .clone(),
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

    use super::ensure_trace_context;

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
