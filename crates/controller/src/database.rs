use anyhow::Context;
use sqlx::{AnyPool, any::AnyPoolOptions};

use agentplane_proto::fleet::{ConfigStatus, Hello, Inventory};

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
}

pub struct DevicePrincipal {
    pub issuer: String,
    pub subject: String,
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
