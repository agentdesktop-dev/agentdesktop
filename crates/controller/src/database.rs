use anyhow::Context;
use sqlx::{AnyPool, any::AnyPoolOptions};
use std::collections::BTreeMap;

use agentdesktop_proto::fleet::{ConfigStatus, Hello, Inventory};
use serde::Serialize;

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
}

pub struct DevicePrincipal {
    pub issuer: String,
    pub subject: String,
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

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct DeviceDiscovery {
    pub kind: String,
    pub version: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceDetail {
    #[serde(flatten)]
    pub device: DeviceSummary,
    pub discoveries: Vec<DeviceDiscovery>,
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
    ) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO devices
                (id, hostname, created_at, last_seen_at, enrolled_by_issuer, enrolled_by_subject)
             VALUES ($1, $2, $3, $3, $4, $5)",
        )
        .bind(device_id)
        .bind(hostname)
        .bind(unix_time_seconds())
        .bind(enrolled_by_issuer)
        .bind(enrolled_by_subject)
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
        let (issuer, subject) = sqlx::query_as(
            "SELECT enrolled_by_issuer, enrolled_by_subject FROM devices WHERE id = $1",
        )
        .bind(device_id)
        .fetch_one(&self.pool)
        .await
        .context("load device enrollment principal")?;
        Ok(DevicePrincipal { issuer, subject })
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
        let discoveries = sqlx::query_as(
            "SELECT kind, version, path FROM discoveries
             WHERE device_id = $1 ORDER BY kind ASC, path ASC",
        )
        .bind(device_id)
        .fetch_all(&self.pool)
        .await
        .context("load device discoveries")?;
        let mut device: DeviceSummary = device.into();
        device.installed_tools = discoveries
            .iter()
            .map(|discovery: &DeviceDiscovery| discovery.kind.clone())
            .collect();
        Ok(Some(DeviceDetail {
            device,
            discoveries,
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
            sqlx::query(
                "INSERT INTO discoveries (device_id, kind, version, path)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(device_id)
            .bind(&discovery.kind)
            .bind(&discovery.version)
            .bind(&discovery.path)
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
}

fn unix_time_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}
