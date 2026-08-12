use anyhow::Context;
use sqlx::{AnyPool, any::AnyPoolOptions};
use std::{collections::BTreeMap, path::PathBuf};

use agentdesktop_core::model::{McpServer, Skill};
use agentdesktop_proto::fleet::{ConfigStatus, Hello, Inventory, TelemetryEvent, telemetry_event};
use serde::Serialize;

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
}

pub struct DevicePrincipal {
    pub issuer: String,
    pub subject: String,
    pub idp_claims: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceSummary {
    pub id: String,
    pub hostname: String,
    pub os: String,
    pub architecture: String,
    pub agent_version: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub enrolled_by_issuer: String,
    pub enrolled_by_subject: String,
    pub config_revision: Option<i64>,
    pub config_state: Option<i64>,
    pub config_error: Option<String>,
    pub config_updated_at: Option<i64>,
    pub discovery_count: i64,
    pub installed_tools: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    id: String,
    hostname: String,
    os: String,
    architecture: String,
    agent_version: String,
    created_at: i64,
    last_seen_at: Option<i64>,
    enrolled_by_issuer: String,
    enrolled_by_subject: String,
    config_revision: Option<i64>,
    config_state: Option<i64>,
    config_error: Option<String>,
    config_updated_at: Option<i64>,
    discovery_count: i64,
}

impl From<DeviceRow> for DeviceSummary {
    fn from(row: DeviceRow) -> Self {
        Self {
            id: row.id,
            hostname: row.hostname,
            os: row.os,
            architecture: row.architecture,
            agent_version: row.agent_version,
            created_at: row.created_at,
            last_seen_at: row.last_seen_at,
            enrolled_by_issuer: row.enrolled_by_issuer,
            enrolled_by_subject: row.enrolled_by_subject,
            config_revision: row.config_revision,
            config_state: row.config_state,
            config_error: row.config_error,
            config_updated_at: row.config_updated_at,
            discovery_count: row.discovery_count,
            installed_tools: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceDiscovery {
    pub kind: String,
    pub version: String,
    pub path: String,
    pub mcp_servers: Vec<McpServer>,
    pub skills: Vec<Skill>,
}

#[derive(sqlx::FromRow)]
struct DeviceDiscoveryRow {
    kind: String,
    version: String,
    path: String,
    mcp_servers_json: String,
    skills_json: String,
}

impl TryFrom<DeviceDiscoveryRow> for DeviceDiscovery {
    type Error = anyhow::Error;

    fn try_from(row: DeviceDiscoveryRow) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: row.kind,
            version: row.version,
            path: row.path,
            mcp_servers: serde_json::from_str(&row.mcp_servers_json)
                .context("decode discovered MCP servers")?,
            skills: serde_json::from_str(&row.skills_json).context("decode discovered skills")?,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceDetail {
    #[serde(flatten)]
    pub device: DeviceSummary,
    pub discoveries: Vec<DeviceDiscovery>,
    pub recent_events: Vec<TelemetryEventRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TelemetryEventRecord {
    pub id: String,
    pub timestamp_unix_ms: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct TelemetryEventRow {
    id: String,
    timestamp_unix_ms: i64,
    event_type: String,
    payload_json: String,
}

impl TryFrom<TelemetryEventRow> for TelemetryEventRecord {
    type Error = anyhow::Error;

    fn try_from(row: TelemetryEventRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            timestamp_unix_ms: row.timestamp_unix_ms,
            event_type: row.event_type,
            payload: serde_json::from_str(&row.payload_json).context("decode telemetry payload")?,
        })
    }
}

impl Database {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await
            .with_context(|| format!("connect to database {url}"))?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .context("run database migrations")?;
        Ok(Self { pool })
    }

    pub async fn enroll_device(
        &self,
        device_id: &str,
        hostname: &str,
        credential_hash: &str,
        enrolled_by_issuer: &str,
        enrolled_by_subject: &str,
        idp_claims: Option<&BTreeMap<String, serde_json::Value>>,
    ) -> anyhow::Result<()> {
        let idp_claims_json = idp_claims
            .map(serde_json::to_string)
            .transpose()
            .context("encode verified IdP claims")?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO devices
                (id, hostname, created_at, last_seen_at, enrolled_by_issuer,
                 enrolled_by_subject, idp_claims_json)
             VALUES ($1, $2, $3, $3, $4, $5, $6)",
        )
        .bind(device_id)
        .bind(hostname)
        .bind(unix_time_seconds())
        .bind(enrolled_by_issuer)
        .bind(enrolled_by_subject)
        .bind(idp_claims_json)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO device_credentials (device_id, credential_hash)
             VALUES ($1, $2)",
        )
        .bind(device_id)
        .bind(credential_hash)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn authenticate(&self, credential_hash: &str) -> anyhow::Result<Option<String>> {
        sqlx::query_scalar("SELECT device_id FROM device_credentials WHERE credential_hash = $1")
            .bind(credential_hash)
            .fetch_optional(&self.pool)
            .await
            .context("authenticate device credential")
    }

    pub async fn device_principal(&self, device_id: &str) -> anyhow::Result<DevicePrincipal> {
        let (issuer, subject, idp_claims_json): (String, String, Option<String>) = sqlx::query_as(
            "SELECT enrolled_by_issuer, enrolled_by_subject, idp_claims_json
             FROM devices WHERE id = $1",
        )
        .bind(device_id)
        .fetch_one(&self.pool)
        .await
        .context("load device enrollment principal")?;
        let idp_claims = idp_claims_json
            .map(|claims| serde_json::from_str(&claims).context("decode verified IdP claims"))
            .transpose()?;
        Ok(DevicePrincipal {
            issuer,
            subject,
            idp_claims,
        })
    }

    pub async fn list_devices(&self) -> anyhow::Result<Vec<DeviceSummary>> {
        let rows: Vec<DeviceRow> = sqlx::query_as(
            "SELECT d.id, d.hostname, d.os, d.architecture, d.agent_version,
                    d.created_at, d.last_seen_at, d.enrolled_by_issuer,
                    d.enrolled_by_subject, cs.revision AS config_revision,
                    cs.state AS config_state, cs.error AS config_error,
                    cs.updated_at AS config_updated_at, COUNT(x.kind) AS discovery_count
             FROM devices d
             LEFT JOIN device_config_status cs ON cs.device_id = d.id
             LEFT JOIN discoveries x ON x.device_id = d.id
             GROUP BY d.id, d.hostname, d.os, d.architecture, d.agent_version,
                      d.created_at, d.last_seen_at, d.enrolled_by_issuer,
                      d.enrolled_by_subject, cs.revision, cs.state, cs.error, cs.updated_at
             ORDER BY d.last_seen_at DESC, d.hostname ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("list devices")?;
        let mut devices: Vec<DeviceSummary> = rows.into_iter().map(Into::into).collect();
        let discovered_tools: Vec<(String, String)> =
            sqlx::query_as("SELECT device_id, kind FROM discoveries ORDER BY device_id, kind")
                .fetch_all(&self.pool)
                .await
                .context("list installed developer tools")?;
        let mut tools_by_device: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (device_id, kind) in discovered_tools {
            tools_by_device.entry(device_id).or_default().push(kind);
        }
        for device in &mut devices {
            device.installed_tools = tools_by_device.remove(&device.id).unwrap_or_default();
        }
        Ok(devices)
    }

    pub async fn get_device(&self, device_id: &str) -> anyhow::Result<Option<DeviceDetail>> {
        let device: Option<DeviceRow> = sqlx::query_as(
            "SELECT d.id, d.hostname, d.os, d.architecture, d.agent_version,
                    d.created_at, d.last_seen_at, d.enrolled_by_issuer,
                    d.enrolled_by_subject, cs.revision AS config_revision,
                    cs.state AS config_state, cs.error AS config_error,
                    cs.updated_at AS config_updated_at, COUNT(x.kind) AS discovery_count
             FROM devices d
             LEFT JOIN device_config_status cs ON cs.device_id = d.id
             LEFT JOIN discoveries x ON x.device_id = d.id
             WHERE d.id = $1
             GROUP BY d.id, d.hostname, d.os, d.architecture, d.agent_version,
                      d.created_at, d.last_seen_at, d.enrolled_by_issuer,
                      d.enrolled_by_subject, cs.revision, cs.state, cs.error, cs.updated_at",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await
        .context("load device")?;
        let Some(device) = device else {
            return Ok(None);
        };
        let rows: Vec<DeviceDiscoveryRow> = sqlx::query_as(
            "SELECT kind, version, path, mcp_servers_json, skills_json FROM discoveries
             WHERE device_id = $1 ORDER BY kind ASC, path ASC",
        )
        .bind(device_id)
        .fetch_all(&self.pool)
        .await
        .context("load device discoveries")?;
        let discoveries: Vec<DeviceDiscovery> = rows
            .into_iter()
            .map(DeviceDiscovery::try_from)
            .collect::<anyhow::Result<_>>()?;
        let mut device: DeviceSummary = device.into();
        device.installed_tools = discoveries
            .iter()
            .map(|discovery: &DeviceDiscovery| discovery.kind.clone())
            .collect();
        let recent_events = self.recent_telemetry(device_id, 50).await?;
        Ok(Some(DeviceDetail {
            device,
            discoveries,
            recent_events,
        }))
    }

    pub async fn delete_device(&self, device_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM devices WHERE id = $1")
            .bind(device_id)
            .execute(&self.pool)
            .await
            .context("delete device")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_hello(&self, device_id: &str, hello: &Hello) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE devices
             SET hostname = $2, os = $3, architecture = $4, agent_version = $5,
                 last_seen_at = $6
             WHERE id = $1",
        )
        .bind(device_id)
        .bind(&hello.hostname)
        .bind(&hello.os)
        .bind(&hello.architecture)
        .bind(&hello.agent_version)
        .bind(unix_time_seconds())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_heartbeat(&self, device_id: &str, timestamp: u64) -> anyhow::Result<()> {
        sqlx::query("UPDATE devices SET last_seen_at = $2 WHERE id = $1")
            .bind(device_id)
            .bind(i64::try_from(timestamp).unwrap_or(i64::MAX))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn replace_inventory(
        &self,
        device_id: &str,
        inventory: &Inventory,
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM discoveries WHERE device_id = $1")
            .bind(device_id)
            .execute(&mut *transaction)
            .await?;
        for discovery in &inventory.discoveries {
            let mcp_servers: Vec<McpServer> = discovery
                .mcp_servers
                .iter()
                .map(|server| McpServer {
                    name: server.name.clone(),
                    transport: server.transport.clone(),
                    command: (!server.command.is_empty()).then(|| server.command.clone()),
                    url: (!server.url.is_empty()).then(|| server.url.clone()),
                    enabled: server.enabled,
                    source: PathBuf::from(&server.source),
                })
                .collect();
            let skills: Vec<Skill> = discovery
                .skills
                .iter()
                .map(|skill| {
                    let front_matter = serde_json::from_slice(&skill.front_matter_json)
                        .context("decode reported skill front matter")?;
                    Ok(Skill {
                        path: PathBuf::from(&skill.path),
                        front_matter,
                    })
                })
                .collect::<anyhow::Result<_>>()?;
            let mcp_servers_json =
                serde_json::to_string(&mcp_servers).context("encode discovered MCP servers")?;
            let skills_json = serde_json::to_string(&skills).context("encode discovered skills")?;
            sqlx::query(
                "INSERT INTO discoveries
                    (device_id, kind, version, path, mcp_servers_json, skills_json)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(device_id)
            .bind(&discovery.kind)
            .bind(&discovery.version)
            .bind(&discovery.path)
            .bind(mcp_servers_json)
            .bind(skills_json)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn update_config_status(
        &self,
        device_id: &str,
        status: &ConfigStatus,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO device_config_status
                (device_id, revision, state, error, updated_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (device_id) DO UPDATE SET
                revision = excluded.revision,
                state = excluded.state,
                error = excluded.error,
                updated_at = excluded.updated_at",
        )
        .bind(device_id)
        .bind(i64::try_from(status.revision).unwrap_or(i64::MAX))
        .bind(i64::from(status.state))
        .bind(&status.error)
        .bind(unix_time_seconds())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_telemetry(
        &self,
        device_id: &str,
        telemetry: &TelemetryEvent,
    ) -> anyhow::Result<()> {
        let (event_type, payload) =
            match telemetry.event.as_ref().context("telemetry has no event")? {
                telemetry_event::Event::SessionNew(event) => {
                    if event.client_id.is_empty() || event.client_id.len() > 64 {
                        anyhow::bail!("invalid telemetry client ID");
                    }
                    if event.session_id.is_empty() || event.session_id.len() > 256 {
                        anyhow::bail!("invalid telemetry session ID");
                    }
                    (
                        "sessionNew",
                        serde_json::json!({
                            "clientId": event.client_id,
                            "sessionId": event.session_id,
                        }),
                    )
                }
                telemetry_event::Event::ToolUse(event) => {
                    if event.client_id.is_empty() || event.client_id.len() > 64 {
                        anyhow::bail!("invalid telemetry client ID");
                    }
                    if event.tool_name.is_empty() || event.tool_name.len() > 128 {
                        anyhow::bail!("invalid telemetry tool name");
                    }
                    if event.tool_use_id.len() > 256 {
                        anyhow::bail!("invalid telemetry tool use ID");
                    }
                    if event.input_json.len() > 256 * 1024 {
                        anyhow::bail!("telemetry tool input is too large");
                    }
                    let tool_input = (!event.input_json.is_empty())
                        .then(|| {
                            serde_json::from_slice(&event.input_json)
                                .context("decode telemetry tool input")
                        })
                        .transpose()?;
                    let mut payload = serde_json::json!({
                        "clientId": event.client_id,
                        "toolName": event.tool_name,
                        "toolUseId": (!event.tool_use_id.is_empty()).then_some(&event.tool_use_id),
                    });
                    if let Some(tool_input) = tool_input {
                        payload["toolInput"] = tool_input;
                    }
                    ("toolUse", payload)
                }
            };
        sqlx::query(
            "INSERT INTO telemetry_events
                (id, device_id, timestamp_unix_ms, event_type, payload_json)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(device_id)
        .bind(i64::try_from(telemetry.timestamp_unix_ms).unwrap_or(i64::MAX))
        .bind(event_type)
        .bind(serde_json::to_string(&payload).context("encode telemetry payload")?)
        .execute(&self.pool)
        .await
        .context("store telemetry event")?;
        Ok(())
    }

    async fn recent_telemetry(
        &self,
        device_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<TelemetryEventRecord>> {
        let rows: Vec<TelemetryEventRow> = sqlx::query_as(
            "SELECT id, timestamp_unix_ms, event_type, payload_json
             FROM telemetry_events
             WHERE device_id = $1
             ORDER BY timestamp_unix_ms DESC
             LIMIT $2",
        )
        .bind(device_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("load recent telemetry")?;
        rows.into_iter()
            .map(TelemetryEventRecord::try_from)
            .collect()
    }
}

fn unix_time_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use agentdesktop_proto::fleet::{
        Discovery, Inventory, McpServer, Skill, TelemetryEvent, ToolUseEvent, telemetry_event,
    };

    use super::Database;

    #[tokio::test]
    async fn persists_device_inventory_and_telemetry() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-inventory-test-{}.db",
            std::process::id()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect test database");
        let idp_claims = BTreeMap::from([
            ("sub".to_owned(), serde_json::json!("subject")),
            ("email".to_owned(), serde_json::json!("john@example.com")),
        ]);
        database
            .enroll_device(
                "device",
                "host",
                "credential",
                "issuer",
                "subject",
                Some(&idp_claims),
            )
            .await
            .expect("enroll device");
        let front_matter = BTreeMap::from([
            ("name", serde_json::json!("llm-research")),
            ("description", serde_json::json!("Compare gateways")),
        ]);
        database
            .replace_inventory(
                "device",
                &Inventory {
                    discoveries: vec![Discovery {
                        kind: "codex".to_owned(),
                        version: "1.0.0".to_owned(),
                        path: "/bin/codex".to_owned(),
                        mcp_servers: vec![McpServer {
                            name: "docs".to_owned(),
                            transport: "http".to_owned(),
                            url: "https://example.com/mcp".to_owned(),
                            enabled: true,
                            source: "/home/user/.codex/config.toml".to_owned(),
                            ..Default::default()
                        }],
                        skills: vec![Skill {
                            path: "/home/user/.codex/skills/llm-research/SKILL.md".to_owned(),
                            front_matter_json: serde_json::to_vec(&front_matter).unwrap(),
                        }],
                    }],
                },
            )
            .await
            .expect("store inventory");
        database
            .insert_telemetry(
                "device",
                &TelemetryEvent {
                    timestamp_unix_ms: 1_700_000_000_000,
                    event: Some(telemetry_event::Event::ToolUse(ToolUseEvent {
                        client_id: "claude-code".to_owned(),
                        tool_name: "Bash".to_owned(),
                        tool_use_id: "tool-1".to_owned(),
                        input_json: serde_json::to_vec(&serde_json::json!({
                            "command": "cargo test"
                        }))
                        .unwrap(),
                    })),
                },
            )
            .await
            .expect("store telemetry");

        let device = database
            .get_device("device")
            .await
            .expect("load device")
            .expect("device exists");
        assert_eq!(device.discoveries[0].mcp_servers[0].name, "docs");
        assert_eq!(
            device.discoveries[0].skills[0].path,
            PathBuf::from("/home/user/.codex/skills/llm-research/SKILL.md")
        );
        assert_eq!(
            device.discoveries[0].skills[0].front_matter["name"],
            "llm-research"
        );
        assert_eq!(device.recent_events.len(), 1);
        assert_eq!(device.recent_events[0].event_type, "toolUse");
        assert_eq!(device.recent_events[0].payload["toolName"], "Bash");
        let principal = database
            .device_principal("device")
            .await
            .expect("load device principal");
        assert_eq!(principal.idp_claims.unwrap()["email"], "john@example.com");

        drop(database);
        let _ = std::fs::remove_file(path);
    }
}
