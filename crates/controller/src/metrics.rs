//! Fleet controller metrics, built on the OpenTelemetry SDK.
//!
//! One meter provider feeds two optional readers:
//!
//! * a Prometheus reader, served at `GET /metrics` on `metricsListen`;
//! * an OTLP push exporter, enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` or
//!   `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is set and configured entirely
//!   through the standard `OTEL_*` variables.
//!
//! Instruments use dotted OpenTelemetry names and semantic-convention
//! attributes; the Prometheus exporter derives the scrape names
//! (`agentdesktop.devices` with unit `{device}` becomes `agentdesktop_devices`).
//!
//! Fleet numbers come from the database, which is too slow to query on every
//! scrape. A background task refreshes them every [`REFRESH_INTERVAL`] and the
//! observable instruments only read that snapshot. When a refresh fails the
//! snapshot is dropped, so the database-backed series disappear instead of
//! going stale, while the in-memory counters keep being served.
//!
//! Attribute values never come straight from a device: OS and agent version
//! are mapped to a fixed set (anything else becomes `unknown`), and the other
//! attributes are chosen from constants here. Hostnames, users, device IDs and
//! tool or MCP server names are never recorded.
//!
//! Every replica reads the same database, so each one reports the same fleet
//! numbers. Dashboards should average those series across replicas before
//! summing (see the README).

use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{self, Poll},
    time::{Duration, SystemTime},
};

use anyhow::Context;
use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_core::Stream;
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Meter, MeterProvider as _, UpDownCounter},
};
use opentelemetry_otlp::{
    WithTonicConfig,
    tonic_types::transport::{Certificate, ClientTlsConfig},
};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    resource::{EnvResourceDetector, ResourceDetector, TelemetryResourceDetector},
};
use prometheus::{Encoder, Registry, TextEncoder};
use tracing::{info, warn};

use crate::{daemon_config::DaemonConfigStore, database::Database};

/// How often fleet numbers are recomputed from the database.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// A refresh that takes longer than this counts as failed, so a hung query drops
/// the snapshot instead of leaving the last one served forever. Shorter than
/// [`REFRESH_INTERVAL`], so refreshes never overlap.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

/// Last-seen buckets, in seconds. A device seen within the first bound is
/// `online`, the same threshold the management UI overview uses.
const LAST_SEEN_BUCKETS: [(i64, &str); 3] = [(90, "online"), (3_600, "1h"), (86_400, "24h")];
const LAST_SEEN_OLDER: &str = "older";
const LAST_SEEN_NEVER: &str = "never";

/// Stored values of `ConfigState::Applied` and `ConfigState::Failed`.
const CONFIG_STATE_APPLIED: i64 = 1;
const CONFIG_STATE_FAILED: i64 = 2;

const UNKNOWN: &str = "unknown";

/// Telemetry event names the controller stores.
const KNOWN_EVENTS: [&str; 2] = ["session.new", "tool.use"];

/// Attribute values for `device.messages`. `agentdesktop.device.message.type` names the kind of
/// device message; `error.type` is set only when storing it failed.
pub mod message {
    pub const HELLO: &str = "hello";
    pub const HEARTBEAT: &str = "heartbeat";
    pub const INVENTORY: &str = "inventory";
    pub const CONFIG_STATUS: &str = "config_status";
    pub const TELEMETRY: &str = "telemetry";
    pub const OTHER: &str = "other";
    /// `error.type` for a message the controller could not store.
    pub const STORE_FAILED: &str = "store_failed";
}

/// Maps a reported OS to `os.type`: the OpenTelemetry well-known value, or
/// `unknown`.
fn os_type(os: &str) -> &'static str {
    match os {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => UNKNOWN,
    }
}

/// Keeps a version only when it looks like `MAJOR.MINOR.PATCH` with an
/// optional short pre-release, so a device cannot invent label values.
fn agent_version(version: &str) -> String {
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    let parts: Vec<&str> = core.split('.').collect();
    let numeric = parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 4 && part.bytes().all(|b| b.is_ascii_digit())
        });
    let pre_ok = pre.len() <= 12 && pre.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.');
    if numeric && pre_ok {
        version.to_owned()
    } else {
        UNKNOWN.to_owned()
    }
}

fn config_state(state: Option<i64>) -> &'static str {
    match state {
        Some(CONFIG_STATE_APPLIED) => "applied",
        Some(CONFIG_STATE_FAILED) => "failed",
        _ => "unreported",
    }
}

fn last_seen_bucket(now: i64, last_seen: Option<i64>) -> &'static str {
    let Some(last_seen) = last_seen else {
        return LAST_SEEN_NEVER;
    };
    let age = (now - last_seen).max(0);
    LAST_SEEN_BUCKETS
        .iter()
        .find(|(bound, _)| age <= *bound)
        .map_or(LAST_SEEN_OLDER, |(_, name)| name)
}

/// One row of `agentdesktop.devices`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeviceKey {
    os_type: &'static str,
    agent_version: String,
    config_state: &'static str,
    last_seen: &'static str,
}

/// Database-backed numbers, recomputed on a timer.
#[derive(Clone, Debug, Default)]
struct FleetSnapshot {
    devices: Vec<(DeviceKey, u64)>,
    config_revision: Option<u64>,
    mcp_servers: u64,
    skills: u64,
}

impl FleetSnapshot {
    async fn load(database: &Database, daemon_config: &DaemonConfigStore) -> anyhow::Result<Self> {
        let devices = database.list_devices().await?;
        let (mcp_servers, skills) = database.inventory_totals().await?;
        let now = unix_time_seconds();
        let mut counts = std::collections::BTreeMap::<DeviceKey, u64>::new();
        for device in &devices {
            let key = DeviceKey {
                os_type: os_type(&device.os),
                agent_version: agent_version(&device.agent_version),
                config_state: config_state(device.config_state),
                last_seen: last_seen_bucket(now, device.last_seen_at),
            };
            *counts.entry(key).or_default() += 1;
        }
        Ok(Self {
            devices: counts.into_iter().collect(),
            config_revision: daemon_config.current().map(|config| config.revision),
            mcp_servers: u64::try_from(mcp_servers).unwrap_or(u64::MAX),
            skills: u64::try_from(skills).unwrap_or(u64::MAX),
        })
    }
}

type SharedSnapshot = Arc<RwLock<Option<FleetSnapshot>>>;

/// Instruments recorded by the fleet service.
#[derive(Clone)]
pub struct Metrics {
    streams: UpDownCounter<i64>,
    messages: Counter<u64>,
    telemetry: Counter<u64>,
}

impl Metrics {
    fn new(meter: &Meter) -> Self {
        Self {
            streams: meter
                .i64_up_down_counter("agentdesktop.device.streams")
                .with_description("Device streams open to this controller replica.")
                .with_unit("{stream}")
                .build(),
            messages: meter
                .u64_counter("agentdesktop.device.messages")
                .with_description(
                    "Device messages received; error.type is set when storing one failed.",
                )
                .with_unit("{message}")
                .build(),
            telemetry: meter
                .u64_counter("agentdesktop.telemetry.events")
                .with_description("Telemetry events stored, by event.name.")
                .with_unit("{event}")
                .build(),
        }
    }

    /// Instruments that record nothing, for services built without metrics.
    pub fn disabled() -> Self {
        Self::new(&opentelemetry::metrics::noop::NoopMeterProvider::new().meter("agentdesktop"))
    }

    /// Counts a device stream as open until the returned response stream is dropped.
    ///
    /// The count follows the response, not the request: a device that half-closes
    /// its request stream still receives configuration updates, so it stays open.
    pub fn track_stream<S>(&self, stream: S) -> TrackedStream<S> {
        TrackedStream {
            inner: stream,
            _guard: self.stream_opened(),
        }
    }

    fn stream_opened(&self) -> StreamGuard {
        self.streams.add(1, &[]);
        StreamGuard(self.streams.clone())
    }

    /// Counts one device message by type, with `error.type` when it failed.
    pub fn message(&self, message_type: &'static str, error_type: Option<&'static str>) {
        let mut attributes = vec![KeyValue::new(
            "agentdesktop.device.message.type",
            message_type,
        )];
        if let Some(error_type) = error_type {
            attributes.push(KeyValue::new("error.type", error_type));
        }
        self.messages.add(1, &attributes);
    }

    /// Counts a stored telemetry event; unknown names are recorded as `unknown`.
    pub fn telemetry_event(&self, event_name: &str) {
        let name = KNOWN_EVENTS
            .iter()
            .find(|known| **known == event_name)
            .copied()
            .unwrap_or(UNKNOWN);
        self.telemetry.add(1, &[KeyValue::new("event.name", name)]);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Decrements the open-stream count when a device stream ends.
struct StreamGuard(UpDownCounter<i64>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.add(-1, &[]);
    }
}

/// A device response stream, counted as open until it is dropped.
pub struct TrackedStream<S> {
    inner: S,
    _guard: StreamGuard,
}

impl<S: Stream + Unpin> Stream for TrackedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// The controller's meter provider and its Prometheus registry.
pub struct Telemetry {
    provider: SdkMeterProvider,
    registry: Registry,
    snapshot: SharedSnapshot,
    otlp: bool,
    pub metrics: Metrics,
}

impl Telemetry {
    /// Builds the meter provider with a Prometheus reader and, when an OTLP
    /// endpoint is configured in the environment, an OTLP push reader.
    ///
    /// Fails when the environment asks for something the OTLP exporter cannot
    /// do (a protocol other than gRPC, an unreadable CA file), so a
    /// misconfiguration stops the controller instead of silently exporting
    /// nothing.
    pub fn new() -> anyhow::Result<Self> {
        let otlp = otlp_settings(|name| std::env::var_os(name))?;
        let service_name = std::env::var("OTEL_SERVICE_NAME").ok();
        let detectors: [Box<dyn ResourceDetector>; 2] = [
            Box::new(TelemetryResourceDetector),
            Box::new(EnvResourceDetector::new()),
        ];
        Self::build(otlp, resource(service_name, &detectors))
    }

    fn build(otlp: Option<OtlpSettings>, resource: Resource) -> anyhow::Result<Self> {
        let registry = Registry::new();
        let prometheus = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .scope_info_enabled(true)
            .build()
            .context("build Prometheus exporter")?;
        let mut builder = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(prometheus);
        let pushing = otlp.is_some();
        if let Some(settings) = otlp {
            let exporter = otlp_exporter(&settings)?;
            builder = builder.with_reader(PeriodicReader::builder(exporter).build());
            info!(endpoint = %settings.endpoint, tls = settings.https, "controller metrics: OTLP push enabled (gRPC)");
        }
        let provider = builder.build();
        let meter = provider.meter("agentdesktop-controller");
        let snapshot: SharedSnapshot = Arc::default();
        register_fleet_instruments(&meter, &snapshot);
        Ok(Self {
            metrics: Metrics::new(&meter),
            provider,
            registry,
            snapshot,
            otlp: pushing,
        })
    }

    /// True when something exports the metrics: the Prometheus endpoint
    /// (`serving`) or OTLP push. With neither, there is nothing to refresh.
    pub fn exported(&self, serving: bool) -> bool {
        serving || self.otlp
    }

    /// Recomputes the fleet snapshot every [`REFRESH_INTERVAL`] until dropped.
    pub fn spawn_refresh(&self, database: Database, daemon_config: DaemonConfigStore) {
        let snapshot = self.snapshot.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REFRESH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                refresh(&snapshot, &database, &daemon_config).await;
            }
        });
    }

    /// Prometheus text exposition of every instrument.
    fn render(&self) -> anyhow::Result<Vec<u8>> {
        let mut body = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut body)
            .context("encode Prometheus metrics")?;
        Ok(body)
    }

    /// Flushes the OTLP reader, if any.
    pub fn shutdown(&self) {
        if let Err(error) = self.provider.shutdown() {
            warn!(%error, "failed to shut down the metrics provider");
        }
    }
}

/// The controller's OpenTelemetry resource.
///
/// Precedence, lowest first: the built-in defaults (`service.name`,
/// `service.version`), then the SDK and `OTEL_RESOURCE_ATTRIBUTES` detectors,
/// then `OTEL_SERVICE_NAME`, as the specification orders them. The SDK's own
/// "provided" detector is left out because it always writes a `service.name`
/// (`unknown_service`) that would replace the default.
fn resource(service_name_env: Option<String>, detectors: &[Box<dyn ResourceDetector>]) -> Resource {
    let mut builder = Resource::builder_empty()
        .with_attribute(KeyValue::new("service.name", "agentdesktop-controller"))
        .with_attribute(KeyValue::new("service.version", agentdesktop_core::VERSION))
        .with_detectors(detectors);
    if let Some(name) = service_name_env.filter(|name| !name.is_empty()) {
        builder = builder.with_attribute(KeyValue::new("service.name", name));
    }
    builder.build()
}

/// What the environment asks of the OTLP push exporter.
#[derive(Debug, PartialEq, Eq)]
struct OtlpSettings {
    /// Endpoint as written, for the startup log. Never holds credentials: the
    /// OTLP headers variable is not read here.
    endpoint: String,
    https: bool,
    /// PEM file with the CA that signed the collector's certificate.
    certificate: Option<std::path::PathBuf>,
}

/// Reads the standard `OTEL_*` variables. `Ok(None)` means push is off.
///
/// Push is on when an OTLP endpoint is set, unless `OTEL_SDK_DISABLED=true` or
/// `OTEL_METRICS_EXPORTER=none`. The exporter speaks gRPC only, so any other
/// protocol is an error rather than being silently ignored.
fn otlp_settings(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> anyhow::Result<Option<OtlpSettings>> {
    let text = |name: &str| var(name).map(|value| value.to_string_lossy().into_owned());
    let nonempty = |name: &str| text(name).filter(|value| !value.is_empty());
    let is = |name: &str, expected: &str| {
        text(name).is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    if is("OTEL_SDK_DISABLED", "true") || is("OTEL_METRICS_EXPORTER", "none") {
        return Ok(None);
    }
    let Some(endpoint) = nonempty("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .or_else(|| nonempty("OTEL_EXPORTER_OTLP_ENDPOINT"))
    else {
        return Ok(None);
    };
    for name in [
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
    ] {
        if let Some(protocol) = nonempty(name)
            && !protocol.eq_ignore_ascii_case("grpc")
        {
            anyhow::bail!(
                "{name}={protocol} is not supported: controller metrics push uses OTLP/gRPC only \
                 (set it to \"grpc\" or unset it)"
            );
        }
    }
    let certificate = nonempty("OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE")
        .or_else(|| nonempty("OTEL_EXPORTER_OTLP_CERTIFICATE"))
        .map(std::path::PathBuf::from);
    Ok(Some(OtlpSettings {
        https: endpoint.to_ascii_lowercase().starts_with("https://"),
        endpoint,
        certificate,
    }))
}

/// Builds the OTLP/gRPC exporter. Endpoint, headers, timeout and compression
/// come from the standard variables inside the exporter.
///
/// For an `https://` endpoint the tonic default trusts no root certificate, so
/// a TLS config is always supplied: the platform's roots, Mozilla's embedded
/// roots (for images without a CA bundle), and the CA from
/// `OTEL_EXPORTER_OTLP_CERTIFICATE` when set (a private CA in front of an
/// in-cluster collector).
fn otlp_exporter(settings: &OtlpSettings) -> anyhow::Result<opentelemetry_otlp::MetricExporter> {
    let mut builder = opentelemetry_otlp::MetricExporter::builder().with_tonic();
    if settings.https {
        let mut tls = ClientTlsConfig::new().with_enabled_roots();
        if let Some(path) = &settings.certificate {
            let pem = std::fs::read(path)
                .with_context(|| format!("read OTLP CA certificate {}", path.display()))?;
            tls = tls.ca_certificate(Certificate::from_pem(pem));
        }
        builder = builder.with_tls_config(tls);
    }
    builder.build().context("build OTLP metric exporter")
}

async fn refresh(
    snapshot: &SharedSnapshot,
    database: &Database,
    daemon_config: &DaemonConfigStore,
) {
    apply_refresh(
        snapshot,
        REFRESH_TIMEOUT,
        FleetSnapshot::load(database, daemon_config),
    )
    .await;
}

/// Stores the result of `load`, or `None` when it fails or takes longer than
/// `timeout`, so the database-backed series are left out rather than stale.
async fn apply_refresh(
    snapshot: &SharedSnapshot,
    timeout: Duration,
    load: impl std::future::Future<Output = anyhow::Result<FleetSnapshot>>,
) {
    let next = match tokio::time::timeout(timeout, load).await {
        Ok(Ok(next)) => Some(next),
        Ok(Err(error)) => {
            warn!(error = %format!("{error:#}"), "fleet metrics refresh failed; omitting database-backed metrics");
            None
        }
        Err(_) => {
            warn!(
                timeout_secs = timeout.as_secs_f32(),
                "fleet metrics refresh timed out; omitting database-backed metrics"
            );
            None
        }
    };
    *snapshot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
}

fn read(snapshot: &SharedSnapshot) -> Option<FleetSnapshot> {
    snapshot
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Counts for `observe`: a count above `i64::MAX` is not a real fleet.
fn signed(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Observable instruments that read the latest snapshot. With no snapshot
/// (not loaded yet, or the database is unavailable) they record nothing.
///
/// Device and inventory counts are additive across attribute sets, so they are
/// up-down counters: an OTLP backend sums them, where it would average gauges.
/// The configuration revision is a point-in-time value and stays a gauge.
fn register_fleet_instruments(meter: &Meter, snapshot: &SharedSnapshot) {
    let devices = snapshot.clone();
    meter
        .i64_observable_up_down_counter("agentdesktop.devices")
        .with_description(
            "Enrolled devices by os.type, agent version, config state and last-seen bucket.",
        )
        .with_unit("{device}")
        .with_callback(move |observer| {
            let Some(snapshot) = read(&devices) else {
                return;
            };
            for (key, count) in &snapshot.devices {
                observer.observe(
                    signed(*count),
                    &[
                        KeyValue::new("os.type", key.os_type),
                        KeyValue::new("agentdesktop.agent.version", key.agent_version.clone()),
                        KeyValue::new("agentdesktop.device.config.state", key.config_state),
                        KeyValue::new("agentdesktop.device.last_seen", key.last_seen),
                    ],
                );
            }
        })
        .build();
    let revision = snapshot.clone();
    meter
        .u64_observable_gauge("agentdesktop.config.revision")
        .with_description("Revision of the daemon configuration distributed to devices.")
        .with_unit("1")
        .with_callback(move |observer| {
            if let Some(revision) = read(&revision).and_then(|snapshot| snapshot.config_revision) {
                observer.observe(revision, &[]);
            }
        })
        .build();
    let inventory = snapshot.clone();
    meter
        .i64_observable_up_down_counter("agentdesktop.inventory.items")
        .with_description("MCP servers and skills reported across all devices, by kind.")
        .with_unit("{item}")
        .with_callback(move |observer| {
            let Some(snapshot) = read(&inventory) else {
                return;
            };
            observer.observe(
                signed(snapshot.mcp_servers),
                &[KeyValue::new("agentdesktop.inventory.kind", "mcp_server")],
            );
            observer.observe(
                signed(snapshot.skills),
                &[KeyValue::new("agentdesktop.inventory.kind", "skill")],
            );
        })
        .build();
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Serves `GET /metrics` on `address` until the listener fails.
pub async fn serve(address: SocketAddr, telemetry: Arc<Telemetry>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(telemetry);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("bind metrics listener {address}"))?;
    info!(listen = %address, "controller metrics listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn scrape(State(telemetry): State<Arc<Telemetry>>) -> Response {
    match telemetry.render() {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Err(error) => {
            warn!(%error, "failed to encode metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(telemetry: &Telemetry) -> String {
        String::from_utf8(telemetry.render().expect("render")).expect("utf-8")
    }

    #[test]
    fn device_input_is_mapped_to_a_fixed_set() {
        assert_eq!(os_type("macos"), "darwin");
        assert_eq!(os_type("linux"), "linux");
        assert_eq!(os_type("windows"), "windows");
        assert_eq!(os_type("freebsd"), UNKNOWN);
        assert_eq!(os_type("macos\"} 1\nevil"), UNKNOWN);
        // Every OS the agent reports (`std::env::consts::OS`) maps to a known value.
        assert!(
            ["macos", "linux", "windows"]
                .iter()
                .all(|os| os_type(os) != UNKNOWN)
        );

        assert_eq!(agent_version("0.5.0"), "0.5.0");
        assert_eq!(agent_version("0.1.2-124"), "0.1.2-124");
        assert_eq!(agent_version("0.5"), UNKNOWN);
        assert_eq!(agent_version("1.2.3.4"), UNKNOWN);
        assert_eq!(agent_version("1.2.x"), UNKNOWN);
        assert_eq!(agent_version("99999.0.0"), UNKNOWN);
        assert_eq!(agent_version("1.0.0-a-very-long-prerelease"), UNKNOWN);
        assert_eq!(agent_version("1.0.0-al\"ice"), UNKNOWN);
        assert_eq!(agent_version(""), UNKNOWN);
    }

    #[test]
    fn last_seen_is_bucketed() {
        let now = 1_000_000;
        assert_eq!(last_seen_bucket(now, None), LAST_SEEN_NEVER);
        assert_eq!(last_seen_bucket(now, Some(now)), "online");
        assert_eq!(last_seen_bucket(now, Some(now - 90)), "online");
        assert_eq!(last_seen_bucket(now, Some(now - 91)), "1h");
        assert_eq!(last_seen_bucket(now, Some(now - 3_601)), "24h");
        assert_eq!(last_seen_bucket(now, Some(now - 86_401)), LAST_SEEN_OLDER);
        assert_eq!(last_seen_bucket(now, Some(now + 30)), "online");
    }

    fn env(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| std::ffi::OsString::from(value))
        }
    }

    #[test]
    fn otlp_is_only_on_when_an_endpoint_is_set() {
        let settings = |pairs| otlp_settings(env(pairs)).expect("valid settings");
        assert_eq!(settings(&[]), None);
        assert_eq!(settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "")]), None);
        let plain = settings(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317")]).unwrap();
        assert_eq!(plain.endpoint, "http://collector:4317");
        assert!(!plain.https);
        assert!(
            settings(&[(
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "https://collector:4317"
            )])
            .unwrap()
            .https
        );
        // The signal-specific endpoint wins.
        assert_eq!(
            settings(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://generic:4317"),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://metrics:4317"),
            ])
            .unwrap()
            .endpoint,
            "http://metrics:4317"
        );
        // Switched off by the standard variables.
        assert_eq!(
            settings(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
                ("OTEL_METRICS_EXPORTER", "none"),
            ]),
            None
        );
        assert_eq!(
            settings(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
                ("OTEL_SDK_DISABLED", "TRUE"),
            ]),
            None
        );
    }

    #[test]
    fn a_ca_certificate_is_read_from_the_standard_variables() {
        let settings = |pairs| otlp_settings(env(pairs)).unwrap().unwrap();
        let both = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector:4317"),
            ("OTEL_EXPORTER_OTLP_CERTIFICATE", "/etc/ca/generic.pem"),
            (
                "OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE",
                "/etc/ca/metrics.pem",
            ),
        ]);
        assert_eq!(both.certificate, Some("/etc/ca/metrics.pem".into()));
        let generic = settings(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector:4317"),
            ("OTEL_EXPORTER_OTLP_CERTIFICATE", "/etc/ca/generic.pem"),
        ]);
        assert_eq!(generic.certificate, Some("/etc/ca/generic.pem".into()));
    }

    #[test]
    fn protocols_other_than_grpc_are_rejected_at_startup() {
        for (name, value) in [
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "http/protobuf"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "carrier-pigeon"),
        ] {
            let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
                (name, value),
            ]));
            let error = otlp_settings(env(pairs)).expect_err("must be rejected");
            let message = format!("{error:#}");
            assert!(
                message.contains(name) && message.contains(value),
                "{message}"
            );
            assert!(message.contains("gRPC"), "{message}");
        }
        // grpc, in any case, and no protocol at all, are fine.
        for value in ["grpc", "GRPC"] {
            let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", value),
            ]));
            assert!(otlp_settings(env(pairs)).unwrap().is_some());
        }
        // Without an endpoint push is off, so the protocol does not matter.
        assert_eq!(
            otlp_settings(env(&[("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")])).unwrap(),
            None
        );
    }

    #[test]
    fn an_unreadable_ca_certificate_stops_startup() {
        let settings = OtlpSettings {
            endpoint: "https://collector:4317".to_owned(),
            https: true,
            certificate: Some("/nonexistent/ca.pem".into()),
        };
        let error = otlp_exporter(&settings).expect_err("missing CA file");
        assert!(
            format!("{error:#}").contains("/nonexistent/ca.pem"),
            "{error:#}"
        );
    }

    struct FixedDetector(Vec<KeyValue>);

    impl ResourceDetector for FixedDetector {
        fn detect(&self) -> Resource {
            Resource::builder_empty()
                .with_attributes(self.0.clone())
                .build()
        }
    }

    fn attribute(resource: &Resource, key: &str) -> Option<String> {
        resource
            .get(&opentelemetry::Key::new(key.to_owned()))
            .map(|value| value.as_str().into_owned())
    }

    #[test]
    fn the_resource_defaults_then_detectors_then_otel_service_name() {
        let none: [Box<dyn ResourceDetector>; 0] = [];
        let defaults = resource(None, &none);
        assert_eq!(
            attribute(&defaults, "service.name").as_deref(),
            Some("agentdesktop-controller")
        );
        assert_eq!(
            attribute(&defaults, "service.version").as_deref(),
            Some(agentdesktop_core::VERSION)
        );

        // OTEL_RESOURCE_ATTRIBUTES (a detector) replaces the default name...
        let detectors: [Box<dyn ResourceDetector>; 1] = [Box::new(FixedDetector(vec![
            KeyValue::new("service.name", "from-attributes"),
            KeyValue::new("deployment.environment.name", "prod"),
        ]))];
        let detected = resource(None, &detectors);
        assert_eq!(
            attribute(&detected, "service.name").as_deref(),
            Some("from-attributes")
        );
        assert_eq!(
            attribute(&detected, "deployment.environment.name").as_deref(),
            Some("prod")
        );
        assert!(attribute(&detected, "service.version").is_some());

        // ...and OTEL_SERVICE_NAME replaces both.
        let named = resource(Some("from-service-name".to_owned()), &detectors);
        assert_eq!(
            attribute(&named, "service.name").as_deref(),
            Some("from-service-name")
        );
        // An empty OTEL_SERVICE_NAME is ignored.
        let empty = resource(Some(String::new()), &detectors);
        assert_eq!(
            attribute(&empty, "service.name").as_deref(),
            Some("from-attributes")
        );
    }

    #[tokio::test]
    async fn a_hung_refresh_drops_the_snapshot() {
        let snapshot: SharedSnapshot = Arc::new(RwLock::new(Some(FleetSnapshot::default())));
        apply_refresh(&snapshot, Duration::from_millis(50), std::future::pending()).await;
        assert!(
            read(&snapshot).is_none(),
            "a refresh that never finishes must not leave the old snapshot"
        );

        // A refresh that completes in time stores its result.
        apply_refresh(&snapshot, Duration::from_secs(5), async {
            Ok(FleetSnapshot::default())
        })
        .await;
        assert!(read(&snapshot).is_some());
        // One that fails drops it again.
        apply_refresh(&snapshot, Duration::from_secs(5), async {
            anyhow::bail!("boom")
        })
        .await;
        assert!(read(&snapshot).is_none());
    }

    #[test]
    fn the_refresh_timeout_is_shorter_than_the_interval() {
        assert!(REFRESH_TIMEOUT < REFRESH_INTERVAL);
    }

    #[test]
    fn in_memory_instruments_are_served_with_prometheus_names() {
        let telemetry = Telemetry::build(None, resource(None, &[])).expect("build telemetry");
        let metrics = &telemetry.metrics;
        let stream = metrics.stream_opened();
        let second = metrics.stream_opened();
        drop(second);
        metrics.message(message::TELEMETRY, None);
        metrics.message(message::INVENTORY, Some(message::STORE_FAILED));
        metrics.telemetry_event("tool.use");
        metrics.telemetry_event("tool.use");
        metrics.telemetry_event("something.else");

        let body = body(&telemetry);
        for expected in [
            "agentdesktop_device_streams{",
            "agentdesktop_device_messages_total{agentdesktop_device_message_type=\"telemetry\"",
            "agentdesktop_device_messages_total{agentdesktop_device_message_type=\"inventory\",error_type=\"store_failed\"",
            "agentdesktop_telemetry_events_total{event_name=\"tool.use\"",
            "agentdesktop_telemetry_events_total{event_name=\"unknown\"",
            "otel_scope_name=\"agentdesktop-controller\"",
            "target_info{",
        ] {
            assert!(body.contains(expected), "missing {expected:?} in:\n{body}");
        }
        assert!(
            body.lines().any(
                |line| line.starts_with("agentdesktop_device_streams{") && line.ends_with(" 1")
            ),
            "open streams should be 1:\n{body}"
        );
        assert!(
            !body.contains("something.else"),
            "raw event name leaked:\n{body}"
        );
        drop(stream);
    }

    #[tokio::test]
    async fn a_device_stream_stays_open_until_the_response_is_dropped() {
        use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};

        fn open_streams(telemetry: &Telemetry) -> String {
            let body = body(telemetry);
            body.lines()
                .find(|line| line.starts_with("agentdesktop_device_streams{"))
                .and_then(|line| line.rsplit(' ').next())
                .unwrap_or_else(|| panic!("no stream gauge in:\n{body}"))
                .to_owned()
        }

        let telemetry = Telemetry::build(None, resource(None, &[])).expect("build telemetry");
        // The service hands each device a response fed by several senders: the
        // request loop and the configuration-update task.
        let (request_loop, receiver) = tokio::sync::mpsc::channel::<u32>(1);
        let config_updates = request_loop.clone();
        let mut response = telemetry
            .metrics
            .track_stream(ReceiverStream::new(receiver));
        assert_eq!(open_streams(&telemetry), "1");

        // The device half-closes its request stream, so the request loop ends.
        drop(request_loop);
        config_updates.send(7).await.expect("response still open");
        assert_eq!(response.next().await, Some(7));
        assert_eq!(
            open_streams(&telemetry),
            "1",
            "a half-closed device is still connected"
        );

        drop(response);
        assert_eq!(open_streams(&telemetry), "0");
    }

    #[tokio::test]
    async fn fleet_metrics_come_from_the_snapshot_and_vanish_when_the_database_fails() {
        use agentdesktop_proto::fleet::{Discovery, Hello, Inventory, McpServer, Skill};

        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-metrics-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.join("controller.db").display()
        ))
        .await
        .expect("connect test database");
        for (id, os, version) in [
            ("device-1", "macos", "0.5.0"),
            ("device-2", "macos", "0.5.0"),
            ("device-3", "plan9", "evil\"version"),
        ] {
            database
                .enroll_device(id, "secret-hostname", "issuer", "user-1", None)
                .await
                .unwrap();
            database
                .update_hello(
                    id,
                    &Hello {
                        device_id: id.to_owned(),
                        hostname: "secret-hostname".to_owned(),
                        os: os.to_owned(),
                        architecture: "aarch64".to_owned(),
                        agent_version: version.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        database
            .replace_inventory(
                "device-1",
                &Inventory {
                    discoveries: vec![Discovery {
                        kind: "claude-code".to_owned(),
                        version: "2.1.0".to_owned(),
                        path: "/bin/claude".to_owned(),
                        mcp_servers: vec![
                            McpServer {
                                name: "private-mcp-name".to_owned(),
                                transport: "http".to_owned(),
                                command: String::new(),
                                url: "https://example.com/mcp".to_owned(),
                                enabled: true,
                                source: "/tmp/settings.json".to_owned(),
                            };
                            3
                        ],
                        skills: vec![Skill {
                            path: "/tmp/skill".to_owned(),
                            front_matter_json: b"{}".to_vec(),
                        }],
                    }],
                    model_runtimes: vec![],
                },
            )
            .await
            .unwrap();

        let telemetry = Telemetry::build(None, resource(None, &[])).expect("build telemetry");
        telemetry.metrics.message(message::HELLO, None);
        assert!(
            !body(&telemetry).contains("agentdesktop_devices{"),
            "no snapshot yet, so no fleet series"
        );

        let config = DaemonConfigStore::new(None);
        refresh(&telemetry.snapshot, &database, &config).await;
        let body_ok = body(&telemetry);
        for expected in [
            "agentdesktop_agent_version=\"0.5.0\"",
            "agentdesktop_agent_version=\"unknown\"",
            "agentdesktop_device_config_state=\"unreported\"",
            "agentdesktop_device_last_seen=\"online\"",
            "os_type=\"darwin\"",
            "os_type=\"unknown\"",
            "agentdesktop_inventory_items{agentdesktop_inventory_kind=\"mcp_server\"",
            "agentdesktop_inventory_items{agentdesktop_inventory_kind=\"skill\"",
        ] {
            assert!(
                body_ok.contains(expected),
                "missing {expected:?} in:\n{body_ok}"
            );
        }
        let darwin = body_ok
            .lines()
            .find(|line| {
                line.starts_with("agentdesktop_devices{") && line.contains("os_type=\"darwin\"")
            })
            .expect("darwin row");
        assert!(
            darwin.ends_with(" 2"),
            "two macOS devices on one row: {darwin}"
        );
        for private in [
            "secret-hostname",
            "private-mcp-name",
            "user-1",
            "device-1",
            "plan9",
            "evil",
        ] {
            assert!(
                !body_ok.contains(private),
                "{private} leaked into metrics:\n{body_ok}"
            );
        }

        // Database goes away: fleet series are dropped, in-memory ones stay.
        database.close().await;
        refresh(&telemetry.snapshot, &database, &config).await;
        let body_down = body(&telemetry);
        assert!(
            !body_down.contains("agentdesktop_devices{"),
            "stale fleet series:\n{body_down}"
        );
        assert!(
            !body_down.contains("agentdesktop_inventory_items"),
            "stale inventory:\n{body_down}"
        );
        assert!(
            body_down.contains("agentdesktop_device_messages_total{"),
            "in-memory series lost:\n{body_down}"
        );

        let _ = std::fs::remove_dir_all(directory);
    }
}
