//! Prometheus metrics for the fleet controller.
//!
//! Fleet state (devices, versions, configuration status) is read from the
//! database at scrape time, so it is correct after a restart. Live stream and
//! event counters are kept in memory and start from zero when the controller
//! starts, as Prometheus counters do.
//!
//! Labels never carry hostnames, users, tool names, or MCP server names: every
//! label value comes from a small closed set, or from the OS and agent version
//! of enrolled devices.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use tracing::{info, warn};

use crate::{daemon_config::DaemonConfigStore, database::Database};

/// A device counts as online when it was seen within this many seconds, the
/// same threshold the management UI overview uses.
const ONLINE_SECONDS: i64 = 90;

/// Stored value of `ConfigState::Applied` and `ConfigState::Failed`.
const CONFIG_STATE_APPLIED: i64 = 1;
const CONFIG_STATE_FAILED: i64 = 2;

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Event types accepted by [`Metrics::telemetry_event`], matching the stored
/// `telemetry_events.event_type` values.
const TELEMETRY_SESSION_NEW: &str = "session.new";
const TELEMETRY_TOOL_USE: &str = "tool.use";

/// In-memory counters shared by the fleet service and the metrics endpoint.
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Counters>,
}

#[derive(Default)]
struct Counters {
    connected_streams: AtomicU64,
    telemetry_session_new: AtomicU64,
    telemetry_tool_use: AtomicU64,
    message_errors: AtomicU64,
}

/// Decrements the connected-device gauge when a device stream ends.
pub struct StreamGuard(Arc<Counters>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.connected_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    /// Counts a device stream as connected until the guard is dropped.
    pub fn stream_connected(&self) -> StreamGuard {
        self.inner.connected_streams.fetch_add(1, Ordering::Relaxed);
        StreamGuard(self.inner.clone())
    }

    /// Counts a stored telemetry event by its stored type.
    pub fn telemetry_event(&self, event_type: &str) {
        let counter = match event_type {
            TELEMETRY_SESSION_NEW => &self.inner.telemetry_session_new,
            TELEMETRY_TOOL_USE => &self.inner.telemetry_tool_use,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a device message that could not be stored.
    pub fn message_error(&self) {
        self.inner.message_errors.fetch_add(1, Ordering::Relaxed);
    }

    async fn render(
        &self,
        database: &Database,
        daemon_config: &DaemonConfigStore,
    ) -> anyhow::Result<String> {
        let devices = database.list_devices().await?;
        let (mcp_servers, skills) = database.inventory_totals().await?;
        let active_revision = daemon_config.current().map(|config| config.revision);
        let now = unix_time_seconds();

        let online = devices
            .iter()
            .filter(|device| {
                device
                    .last_seen_at
                    .is_some_and(|last_seen| now - last_seen <= ONLINE_SECONDS)
            })
            .count();
        let (mut applied, mut failed, mut unreported, mut behind) = (0, 0, 0, 0);
        let mut versions: BTreeMap<(&str, &str), u64> = BTreeMap::new();
        for device in &devices {
            match device.config_state {
                Some(CONFIG_STATE_APPLIED) => applied += 1,
                Some(CONFIG_STATE_FAILED) => failed += 1,
                _ => unreported += 1,
            }
            if let (Some(active), Some(revision)) = (active_revision, device.config_revision)
                && i64::try_from(active).ok() != Some(revision)
            {
                behind += 1;
            }
            *versions
                .entry((device.os.as_str(), device.agent_version.as_str()))
                .or_default() += 1;
        }
        let oldest_seen = devices
            .iter()
            .filter_map(|device| device.last_seen_at)
            .min()
            .map_or(0, |last_seen| (now - last_seen).max(0));

        let mut out = String::new();
        family(
            &mut out,
            "agentdesktop_build_info",
            "gauge",
            "Controller build; the value is always 1.",
        );
        sample(
            &mut out,
            "agentdesktop_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))],
            1,
        );
        family(
            &mut out,
            "agentdesktop_devices",
            "gauge",
            "Enrolled devices.",
        );
        sample(&mut out, "agentdesktop_devices", &[], devices.len());
        family(
            &mut out,
            "agentdesktop_devices_online",
            "gauge",
            "Devices seen within the last 90 seconds.",
        );
        sample(&mut out, "agentdesktop_devices_online", &[], online);
        family(
            &mut out,
            "agentdesktop_devices_connected",
            "gauge",
            "Devices with an open stream to this controller.",
        );
        sample(
            &mut out,
            "agentdesktop_devices_connected",
            &[],
            self.inner.connected_streams.load(Ordering::Relaxed),
        );
        family(
            &mut out,
            "agentdesktop_device_oldest_seen_age_seconds",
            "gauge",
            "Seconds since the least recently seen device was last seen; 0 with no devices.",
        );
        sample(
            &mut out,
            "agentdesktop_device_oldest_seen_age_seconds",
            &[],
            oldest_seen,
        );
        family(
            &mut out,
            "agentdesktop_devices_by_version",
            "gauge",
            "Enrolled devices by operating system and agent version.",
        );
        for ((os, agent_version), count) in &versions {
            sample(
                &mut out,
                "agentdesktop_devices_by_version",
                &[("os", os), ("agent_version", agent_version)],
                count,
            );
        }
        family(
            &mut out,
            "agentdesktop_devices_by_config_state",
            "gauge",
            "Enrolled devices by the outcome of their last configuration apply.",
        );
        for (state, count) in [
            ("applied", applied),
            ("failed", failed),
            ("unreported", unreported),
        ] {
            sample(
                &mut out,
                "agentdesktop_devices_by_config_state",
                &[("state", state)],
                count,
            );
        }
        family(
            &mut out,
            "agentdesktop_devices_config_behind",
            "gauge",
            "Devices whose reported configuration revision differs from the active one.",
        );
        sample(&mut out, "agentdesktop_devices_config_behind", &[], behind);
        family(
            &mut out,
            "agentdesktop_config_active_revision",
            "gauge",
            "Revision of the configuration distributed to devices; 0 when none.",
        );
        sample(
            &mut out,
            "agentdesktop_config_active_revision",
            &[],
            active_revision.unwrap_or(0),
        );
        family(
            &mut out,
            "agentdesktop_inventory_mcp_servers",
            "gauge",
            "MCP servers reported by all devices, counting a server once per tool that configures it.",
        );
        sample(
            &mut out,
            "agentdesktop_inventory_mcp_servers",
            &[],
            mcp_servers,
        );
        family(
            &mut out,
            "agentdesktop_inventory_skills",
            "gauge",
            "Skills reported by all devices.",
        );
        sample(&mut out, "agentdesktop_inventory_skills", &[], skills);
        family(
            &mut out,
            "agentdesktop_telemetry_events_total",
            "counter",
            "Telemetry events stored since the controller started.",
        );
        sample(
            &mut out,
            "agentdesktop_telemetry_events_total",
            &[("type", TELEMETRY_SESSION_NEW)],
            self.inner.telemetry_session_new.load(Ordering::Relaxed),
        );
        sample(
            &mut out,
            "agentdesktop_telemetry_events_total",
            &[("type", TELEMETRY_TOOL_USE)],
            self.inner.telemetry_tool_use.load(Ordering::Relaxed),
        );
        family(
            &mut out,
            "agentdesktop_device_message_errors_total",
            "counter",
            "Device messages that could not be stored since the controller started.",
        );
        sample(
            &mut out,
            "agentdesktop_device_message_errors_total",
            &[],
            self.inner.message_errors.load(Ordering::Relaxed),
        );
        Ok(out)
    }
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (key, label)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(out, "{key}=\"{}\"", escape_label(label));
        }
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

/// Escapes a label value for the Prometheus text format.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

#[derive(Clone)]
struct MetricsState {
    metrics: Metrics,
    database: Database,
    daemon_config: DaemonConfigStore,
}

/// Serves `GET /metrics` on `address` until the listener fails.
pub async fn serve(
    address: SocketAddr,
    metrics: Metrics,
    database: Database,
    daemon_config: DaemonConfigStore,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(MetricsState {
            metrics,
            database,
            daemon_config,
        });
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(listen = %address, "controller metrics listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn scrape(State(state): State<MetricsState>) -> Response {
    match state
        .metrics
        .render(&state.database, &state.daemon_config)
        .await
    {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response(),
        Err(error) => {
            warn!(%error, "failed to render metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Metrics, escape_label};

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn the_connected_gauge_follows_the_stream_guard() {
        let metrics = Metrics::default();
        let first = metrics.stream_connected();
        let second = metrics.stream_connected();
        assert_eq!(
            metrics
                .inner
                .connected_streams
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        drop(first);
        drop(second);
        assert_eq!(
            metrics
                .inner
                .connected_streams
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn scrape_reports_fleet_state_without_identifying_labels() {
        use agentdesktop_proto::fleet::{Discovery, Hello, Inventory, McpServer, Skill};

        use crate::{daemon_config::DaemonConfigStore, database::Database};

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
        database
            .enroll_device("device-1", "secret-hostname", "issuer", "user-1", None)
            .await
            .unwrap();
        database
            .update_hello(
                "device-1",
                &Hello {
                    device_id: "device-1".to_owned(),
                    hostname: "secret-hostname".to_owned(),
                    os: "macos".to_owned(),
                    architecture: "aarch64".to_owned(),
                    agent_version: "0.3.0".to_owned(),
                },
            )
            .await
            .unwrap();
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

        let metrics = Metrics::default();
        let _stream = metrics.stream_connected();
        metrics.telemetry_event("tool.use");
        metrics.message_error();
        let body = metrics
            .render(&database, &DaemonConfigStore::new(None))
            .await
            .expect("render metrics");

        for expected in [
            "agentdesktop_devices 1\n",
            "agentdesktop_devices_online 1\n",
            "agentdesktop_devices_connected 1\n",
            "agentdesktop_devices_by_version{os=\"macos\",agent_version=\"0.3.0\"} 1\n",
            "agentdesktop_devices_by_config_state{state=\"unreported\"} 1\n",
            "agentdesktop_inventory_mcp_servers 3\n",
            "agentdesktop_inventory_skills 1\n",
            "agentdesktop_telemetry_events_total{type=\"tool.use\"} 1\n",
            "agentdesktop_telemetry_events_total{type=\"session.new\"} 0\n",
            "agentdesktop_device_message_errors_total 1\n",
            "agentdesktop_config_active_revision 0\n",
        ] {
            assert!(body.contains(expected), "missing {expected:?} in:\n{body}");
        }
        for private in ["secret-hostname", "private-mcp-name", "user-1", "device-1"] {
            assert!(!body.contains(private), "{private} leaked into metrics");
        }

        drop(database);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unknown_telemetry_types_are_not_counted() {
        let metrics = Metrics::default();
        metrics.telemetry_event("tool.use");
        metrics.telemetry_event("tool.use");
        metrics.telemetry_event("session.new");
        metrics.telemetry_event("anything.else");
        let counters = &metrics.inner;
        let load = |counter: &std::sync::atomic::AtomicU64| {
            counter.load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!(load(&counters.telemetry_tool_use), 2);
        assert_eq!(load(&counters.telemetry_session_new), 1);
    }
}
