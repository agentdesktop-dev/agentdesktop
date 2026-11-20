//! Controller metrics: an OpenTelemetry meter provider whose Prometheus reader
//! is served as `GET /metrics` on the metrics listener (`metricsListener`).
//!
//! The instruments count when a listener's limits act, and the connections
//! open on each listener. Their attributes come from fixed sets (listener,
//! limit, outcome), never from a client address or device, so the number of
//! series stays small whatever the fleet does.

use std::sync::Arc;

use anyhow::Context as _;
use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Meter, MeterProvider as _, UpDownCounter},
};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider};
use prometheus::{Encoder as _, Registry, TextEncoder};

/// The controller's meter provider and the Prometheus registry its reader
/// writes into.
pub struct Telemetry {
    provider: SdkMeterProvider,
    registry: Registry,
    limits: LimitMetrics,
}

impl Telemetry {
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();
        let reader = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .context("build the Prometheus metrics exporter")?;
        let resource = Resource::builder_empty()
            .with_attribute(KeyValue::new("service.name", "agentdesktop-controller"))
            .with_attribute(KeyValue::new("service.version", agentdesktop_core::VERSION))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();
        let limits = LimitMetrics::new(&provider.meter("agentdesktop-controller"));
        Ok(Self {
            provider,
            registry,
            limits,
        })
    }

    /// The limit instruments, for the listeners and the fleet service.
    pub fn limits(&self) -> LimitMetrics {
        self.limits.clone()
    }

    /// Serves `GET /metrics`.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/metrics", get(scrape))
            .with_state(self)
    }

    /// Every instrument in the Prometheus text format.
    fn render(&self) -> anyhow::Result<Vec<u8>> {
        let mut body = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut body)
            .context("encode Prometheus metrics")?;
        Ok(body)
    }

    /// The value of `agentdesktop_limit_hits_total` for one listener, limit
    /// and outcome, or 0 before the first hit.
    #[cfg(test)]
    pub(crate) fn hits(&self, listener: &str, limit: Limit, outcome: Outcome) -> u64 {
        self.sample(
            "agentdesktop_limit_hits_total",
            &[
                (LISTENER, listener),
                (LIMIT, limit.name()),
                (OUTCOME, outcome.name()),
            ],
        )
        .map_or(0, |value| value as u64)
    }

    /// The value of `agentdesktop_connections` for one listener.
    #[cfg(test)]
    pub(crate) fn connections(&self, listener: &str) -> i64 {
        self.sample("agentdesktop_connections", &[(LISTENER, listener)])
            .map_or(0, |value| value as i64)
    }

    /// The sample of `name` whose labels include all of `labels` (with the
    /// attribute keys as given; the exporter writes `.` as `_`).
    #[cfg(test)]
    fn sample(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let body = String::from_utf8(self.render().expect("render metrics")).expect("UTF-8");
        let wanted: Vec<String> = labels
            .iter()
            .map(|(key, value)| format!("{}=\"{value}\"", key.replace('.', "_")))
            .collect();
        body.lines()
            .filter_map(|line| line.strip_prefix(name)?.strip_prefix('{'))
            .find_map(|line| {
                let (labels, value) = line.rsplit_once("} ")?;
                let labels: Vec<&str> = labels.split(',').collect();
                wanted
                    .iter()
                    .all(|label| labels.contains(&label.as_str()))
                    .then(|| value.trim().parse().expect("sample value"))
            })
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::debug!(%error, "shut down the metrics provider");
        }
    }
}

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

async fn scrape(State(telemetry): State<Arc<Telemetry>>) -> Response {
    match telemetry.render() {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Err(error) => {
            tracing::warn!(%error, "failed to encode metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Attribute keys.
const LISTENER: &str = "agentdesktop.listener";
const LIMIT: &str = "agentdesktop.limit.name";
const OUTCOME: &str = "agentdesktop.limit.outcome";

/// The instruments the limits record to. Cloning shares them. Without a
/// metrics listener they are no-ops (`LimitMetrics::disabled`).
#[derive(Clone)]
pub struct LimitMetrics {
    hits: Counter<u64>,
    connections: UpDownCounter<i64>,
}

impl LimitMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            hits: meter
                .u64_counter("agentdesktop.limit.hits")
                .with_description(
                    "Times a controller limit refused, closed or slowed something, by listener, limit and outcome.",
                )
                .with_unit("{hit}")
                .build(),
            connections: meter
                .i64_up_down_counter("agentdesktop.connections")
                .with_description(
                    "Connections open on each controller listener, counted against its maxConnections.",
                )
                .with_unit("{connection}")
                .build(),
        }
    }

    /// Instruments that record nothing, for a controller without a metrics
    /// listener and for tests that do not read them.
    pub fn disabled() -> Self {
        Self::new(&opentelemetry::metrics::noop::NoopMeterProvider::new().meter("agentdesktop"))
    }

    /// The instruments for one listener; `listener` names it, e.g. `fleet`.
    pub(crate) fn listener(&self, listener: &'static str) -> ListenerMetrics {
        ListenerMetrics {
            metrics: self.clone(),
            listener: KeyValue::new(LISTENER, listener),
        }
    }
}

/// `LimitMetrics` for one listener.
#[derive(Clone)]
pub(crate) struct ListenerMetrics {
    metrics: LimitMetrics,
    listener: KeyValue,
}

impl ListenerMetrics {
    /// Counts one time `limit` acted, with `outcome`.
    pub(crate) fn hit(&self, limit: Limit, outcome: Outcome) {
        self.metrics.hits.add(
            1,
            &[
                self.listener.clone(),
                KeyValue::new(LIMIT, limit.name()),
                KeyValue::new(OUTCOME, outcome.name()),
            ],
        );
    }

    pub(crate) fn connection_opened(&self) {
        self.metrics
            .connections
            .add(1, std::slice::from_ref(&self.listener));
    }

    pub(crate) fn connection_closed(&self) {
        self.metrics
            .connections
            .add(-1, std::slice::from_ref(&self.listener));
    }
}

/// A limit, as the `agentdesktop.limit.name` attribute names it. Configured
/// limits are named after their configuration key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    /// `limits.maxConnections` on any listener.
    MaxConnections,
    /// `limits.connectionsPerAddress` on any listener.
    ConnectionsPerAddress,
    /// The time a new connection has to make its first request.
    FirstRequest,
    /// The time a connection may stay without a request in progress.
    IdleConnection,
    /// `fleetListener.limits.enrollmentBurst` and `enrollmentRefill`.
    EnrollmentRate,
    /// `fleetListener.limits.enrollmentInFlight`.
    EnrollmentInFlight,
    /// How often a device may open a `Connect` stream.
    StreamOpenRate,
    /// `fleetListener.limits.deviceMessageBurst` and `deviceMessageRefill`.
    DeviceMessageRate,
    /// `fleetListener.limits.maxMessageBytes`.
    MaxMessageBytes,
    /// The message limit for callers without a device certificate.
    UnauthenticatedMessageBytes,
    /// The time a fleet request handler has to produce its response.
    RequestTimeout,
    /// The messages the controller queues for a device's `Connect` stream
    /// before it closes the stream of a device that stopped reading.
    ConnectResponseBuffer,
}

impl Limit {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::MaxConnections => "max_connections",
            Self::ConnectionsPerAddress => "connections_per_address",
            Self::FirstRequest => "first_request",
            Self::IdleConnection => "idle_connection",
            Self::EnrollmentRate => "enrollment_rate",
            Self::EnrollmentInFlight => "enrollment_in_flight",
            Self::StreamOpenRate => "stream_open_rate",
            Self::DeviceMessageRate => "device_message_rate",
            Self::MaxMessageBytes => "max_message_bytes",
            Self::UnauthenticatedMessageBytes => "unauthenticated_message_bytes",
            Self::RequestTimeout => "request_timeout",
            Self::ConnectResponseBuffer => "connect_response_buffer",
        }
    }
}

/// What a limit did, as the `agentdesktop.limit.outcome` attribute names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A new connection, request or stream was refused.
    Rejected,
    /// An idle connection was closed to make room for a new one.
    Evicted,
    /// A connection or request ran out of time.
    TimedOut,
    /// A device spent its message budget; its next message is read no sooner than the refill.
    Throttled,
    /// An open stream was closed.
    Closed,
}

impl Outcome {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Evicted => "evicted",
            Self::TimedOut => "timed_out",
            Self::Throttled => "throttled",
            Self::Closed => "closed",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Limit, LimitMetrics, Outcome, Telemetry};

    #[test]
    fn hits_are_counted_by_listener_limit_and_outcome() {
        let telemetry = Telemetry::new().unwrap();
        let fleet = telemetry.limits().listener("fleet");
        let admin = telemetry.limits().listener("admin");
        assert_eq!(
            telemetry.hits("fleet", Limit::MaxConnections, Outcome::Evicted),
            0
        );

        fleet.hit(Limit::MaxConnections, Outcome::Evicted);
        fleet.hit(Limit::MaxConnections, Outcome::Evicted);
        fleet.hit(Limit::MaxConnections, Outcome::Rejected);
        admin.hit(Limit::MaxConnections, Outcome::Evicted);

        assert_eq!(
            telemetry.hits("fleet", Limit::MaxConnections, Outcome::Evicted),
            2
        );
        assert_eq!(
            telemetry.hits("fleet", Limit::MaxConnections, Outcome::Rejected),
            1
        );
        assert_eq!(
            telemetry.hits("admin", Limit::MaxConnections, Outcome::Evicted),
            1
        );
    }

    #[test]
    fn connections_go_up_and_down_per_listener() {
        let telemetry = Telemetry::new().unwrap();
        let fleet = telemetry.limits().listener("fleet");
        fleet.connection_opened();
        fleet.connection_opened();
        fleet.connection_closed();
        telemetry.limits().listener("gateway").connection_opened();

        assert_eq!(telemetry.connections("fleet"), 1);
        assert_eq!(telemetry.connections("gateway"), 1);
    }

    #[test]
    fn disabled_metrics_record_nothing_and_do_not_fail() {
        let disabled = LimitMetrics::disabled().listener("fleet");
        disabled.hit(Limit::RequestTimeout, Outcome::TimedOut);
        disabled.connection_opened();
    }

    #[tokio::test]
    async fn the_endpoint_serves_the_prometheus_text_format() {
        let telemetry = Arc::new(Telemetry::new().unwrap());
        telemetry
            .limits()
            .listener("fleet")
            .hit(Limit::EnrollmentRate, Outcome::Rejected);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, telemetry.clone().router()).into_future());

        let response = reqwest::get(format!("http://{address}/metrics"))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[reqwest::header::CONTENT_TYPE],
            super::CONTENT_TYPE
        );
        let body = response.text().await.unwrap();
        assert!(
            body.lines()
                .any(|line| line.starts_with("agentdesktop_limit_hits_total{")
                    && line.contains("agentdesktop_listener=\"fleet\"")
                    && line.contains("agentdesktop_limit_name=\"enrollment_rate\"")
                    && line.contains("agentdesktop_limit_outcome=\"rejected\"")
                    && line.ends_with(" 1")),
            "{body}"
        );
        assert!(
            body.contains("# TYPE agentdesktop_limit_hits_total counter"),
            "{body}"
        );
    }
}
