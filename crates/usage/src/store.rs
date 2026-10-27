//! Usage rollup storage. Queries run on SQLite and PostgreSQL; sums are cast
//! back to BIGINT because PostgreSQL sums BIGINT columns as NUMERIC.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use anyhow::Context;
use sqlx::{AnyPool, AssertSqlSafe, Row, any::AnyPoolOptions, any::AnyRow};

use super::{
    DAILY_RETENTION, DAY, Dimension, Filter, HOUR, HOURLY_RETENTION, Measures, RECEIPT_RETENTION,
    RollupKey, Totals, UsageRecord,
};

/// Usage tables in a SQLite or PostgreSQL database.
#[derive(Clone)]
pub struct UsageStore {
    pool: AnyPool,
}

impl UsageStore {
    /// Usage tables in a database whose own migrations create them.
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }

    /// Opens the SQLite database at `path`, creating it and the usage tables
    /// when they do not exist.
    pub async fn open_sqlite(path: &Path) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(4)
            .connect(&sqlite_url(path)?)
            .await
            .with_context(|| format!("open usage database {}", path.display()))?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .context("create usage tables")?;
        Ok(Self { pool })
    }
}

/// A `sqlite:` URL naming `path`. Every byte that could end or change the
/// path is percent-encoded; after `sqlite://`, a Windows drive letter would
/// parse as a host.
fn sqlite_url(path: &Path) -> anyhow::Result<String> {
    let path = path
        .to_str()
        .with_context(|| format!("usage database path {} is not UTF-8", path.display()))?;
    let mut url = String::from("sqlite:");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url.push_str("?mode=rwc");
    Ok(url)
}

/// `DO UPDATE` assignments that add a conflicting rollup's measures.
macro_rules! add_measures {
    () => {
        "requests = usage_rollups.requests + excluded.requests,
         errors = usage_rollups.errors + excluded.errors,
         input_tokens = usage_rollups.input_tokens + excluded.input_tokens,
         output_tokens = usage_rollups.output_tokens + excluded.output_tokens,
         cache_read_tokens = usage_rollups.cache_read_tokens + excluded.cache_read_tokens,
         cache_write_tokens = usage_rollups.cache_write_tokens + excluded.cache_write_tokens,
         cost_nanos = usage_rollups.cost_nanos + excluded.cost_nanos,
         unpriced_requests = usage_rollups.unpriced_requests + excluded.unpriced_requests"
    };
}

/// Select-list producing a [`Totals`], in field order.
const TOTALS: &str = "\
    CAST(COALESCE(SUM(requests), 0) AS BIGINT), \
    CAST(COALESCE(SUM(errors), 0) AS BIGINT), \
    CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT), \
    CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT), \
    CAST(COALESCE(SUM(cache_read_tokens), 0) AS BIGINT), \
    CAST(COALESCE(SUM(cache_write_tokens), 0) AS BIGINT), \
    CAST(COALESCE(SUM(CASE WHEN billing = 'subscription' THEN 0 ELSE cost_nanos END), 0) AS BIGINT), \
    CAST(COALESCE(SUM(CASE WHEN billing = 'subscription' THEN cost_nanos ELSE 0 END), 0) AS BIGINT), \
    CAST(COALESCE(SUM(CASE WHEN billing = 'subscription' THEN requests ELSE 0 END), 0) AS BIGINT), \
    CAST(COALESCE(SUM(unpriced_requests), 0) AS BIGINT)";
const TOTALS_COLUMNS: usize = 10;

/// Stores records not seen before and returns how many there were.
pub(super) async fn ingest(
    store: &UsageStore,
    records: &[UsageRecord],
    received_at: i64,
) -> anyhow::Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let mut transaction = store.pool.begin().await?;
    let mut rollups = BTreeMap::<&RollupKey, Measures>::new();
    let mut stored = 0;
    for record in records {
        let inserted = sqlx::query(
            "INSERT INTO usage_receipts (id, received_at) VALUES ($1, $2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(record.id.as_str())
        .bind(received_at)
        .execute(&mut *transaction)
        .await
        .context("store usage record identifier")?
        .rows_affected();
        if inserted == 1 {
            stored += 1;
            rollups
                .entry(&record.key)
                .or_default()
                .add(&record.measures);
        }
    }
    for (key, measures) in rollups {
        sqlx::query(concat!(
            "INSERT INTO usage_rollups
                (bucket_start, bucket_seconds, subject, device_id, client_id, provider, model,
                 billing, requests, errors, input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, cost_nanos, unpriced_requests)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (bucket_start, bucket_seconds, subject, device_id, client_id, provider,
                          model, billing)
             DO UPDATE SET ",
            add_measures!()
        ))
        .bind(key.bucket_start)
        .bind(HOUR)
        .bind(key.subject.as_str())
        .bind(key.device_id.as_str())
        .bind(key.client_id.as_str())
        .bind(key.provider.as_str())
        .bind(key.model.as_str())
        .bind(key.billing.as_str())
        .bind(measures.requests)
        .bind(measures.errors)
        .bind(measures.input_tokens)
        .bind(measures.output_tokens)
        .bind(measures.cache_read_tokens)
        .bind(measures.cache_write_tokens)
        .bind(measures.cost_nanos)
        .bind(measures.unpriced_requests)
        .execute(&mut *transaction)
        .await
        .context("store usage rollup")?;
    }
    transaction.commit().await?;
    Ok(stored)
}

/// Compacts hourly rollups past their retention into UTC days and deletes
/// expired daily rollups and record identifiers.
pub(super) async fn compact(store: &UsageStore, now: i64) -> anyhow::Result<()> {
    // Whole days only, so a day is never split between granularities.
    let hourly_before = (now - HOURLY_RETENTION).div_euclid(DAY) * DAY;
    let mut transaction = store.pool.begin().await?;
    sqlx::query(concat!(
        "INSERT INTO usage_rollups
            (bucket_start, bucket_seconds, subject, device_id, client_id, provider, model,
             billing, requests, errors, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_nanos, unpriced_requests)
         SELECT (bucket_start / 86400) * 86400, 86400, subject, device_id, client_id,
                provider, model, billing,
                CAST(SUM(requests) AS BIGINT), CAST(SUM(errors) AS BIGINT),
                CAST(SUM(input_tokens) AS BIGINT), CAST(SUM(output_tokens) AS BIGINT),
                CAST(SUM(cache_read_tokens) AS BIGINT), CAST(SUM(cache_write_tokens) AS BIGINT),
                CAST(SUM(cost_nanos) AS BIGINT), CAST(SUM(unpriced_requests) AS BIGINT)
         FROM usage_rollups
         WHERE bucket_seconds = 3600 AND bucket_start < $1
         GROUP BY (bucket_start / 86400) * 86400, subject, device_id, client_id, provider,
                  model, billing
         ON CONFLICT (bucket_start, bucket_seconds, subject, device_id, client_id, provider,
                      model, billing)
         DO UPDATE SET ",
        add_measures!()
    ))
    .bind(hourly_before)
    .execute(&mut *transaction)
    .await
    .context("compact hourly usage into days")?;
    sqlx::query("DELETE FROM usage_rollups WHERE bucket_seconds = 3600 AND bucket_start < $1")
        .bind(hourly_before)
        .execute(&mut *transaction)
        .await
        .context("delete compacted hourly usage")?;
    transaction.commit().await?;
    sqlx::query("DELETE FROM usage_rollups WHERE bucket_seconds = 86400 AND bucket_start < $1")
        .bind(now - DAILY_RETENTION)
        .execute(&store.pool)
        .await
        .context("delete expired daily usage")?;
    sqlx::query("DELETE FROM usage_receipts WHERE received_at < $1")
        .bind(now - RECEIPT_RETENTION)
        .execute(&store.pool)
        .await
        .context("delete expired usage record identifiers")?;
    Ok(())
}

pub(super) struct Summary {
    pub(super) totals: Totals,
    pub(super) users: i64,
    pub(super) devices: i64,
}

/// Totals and distinct known users and devices over `[start, end)`.
pub(super) async fn summary(
    store: &UsageStore,
    start: i64,
    end: i64,
    filter: &Filter,
) -> anyhow::Result<Summary> {
    let (conditions, values) = conditions(start, end, filter);
    let rows = fetch(
        store,
        format!(
            "SELECT {TOTALS},
                    COUNT(DISTINCT CASE WHEN subject <> '' THEN subject END),
                    COUNT(DISTINCT CASE WHEN device_id <> '' THEN device_id END)
             FROM usage_rollups WHERE {conditions}"
        ),
        values,
    )
    .await
    .context("summarize usage")?;
    let row = rows.first().context("usage summary returned no row")?;
    Ok(Summary {
        totals: totals(row, 0)?,
        users: row.try_get(TOTALS_COLUMNS)?,
        devices: row.try_get(TOTALS_COLUMNS + 1)?,
    })
}

/// Totals per stored rollup start over `[start, end)`, oldest first.
pub(super) async fn series(
    store: &UsageStore,
    start: i64,
    end: i64,
    filter: &Filter,
) -> anyhow::Result<Vec<(i64, Totals)>> {
    let (conditions, values) = conditions(start, end, filter);
    let rows = fetch(
        store,
        format!(
            "SELECT bucket_start, {TOTALS} FROM usage_rollups WHERE {conditions}
             GROUP BY bucket_start ORDER BY bucket_start"
        ),
        values,
    )
    .await
    .context("load usage over time")?;
    rows.iter()
        .map(|row| Ok((row.try_get(0)?, totals(row, 1)?)))
        .collect()
}

/// Totals per value of `dimension` over `[start, end)`.
pub(super) async fn breakdown(
    store: &UsageStore,
    start: i64,
    end: i64,
    filter: &Filter,
    dimension: Dimension,
) -> anyhow::Result<Vec<(String, Totals)>> {
    let (conditions, values) = conditions(start, end, filter);
    let column = dimension.column();
    let rows = fetch(
        store,
        format!(
            "SELECT {column}, {TOTALS} FROM usage_rollups WHERE {conditions}
             GROUP BY {column}"
        ),
        values,
    )
    .await
    .with_context(|| format!("break usage down by {}", dimension.name()))?;
    rows.iter()
        .map(|row| Ok((row.try_get(0)?, totals(row, 1)?)))
        .collect()
}

/// Display names for user subjects and device identifiers.
#[derive(Debug, Default)]
pub struct Labels {
    users: HashMap<String, String>,
    devices: HashMap<String, String>,
}

impl Labels {
    pub fn insert_user(&mut self, subject: impl Into<String>, name: impl Into<String>) {
        self.users.insert(subject.into(), name.into());
    }

    pub fn insert_device(&mut self, device_id: impl Into<String>, hostname: impl Into<String>) {
        self.devices.insert(device_id.into(), hostname.into());
    }

    pub fn user(&self, subject: &str) -> Option<&str> {
        self.users.get(subject).map(String::as_str)
    }

    pub fn device(&self, device_id: &str) -> Option<&str> {
        self.devices.get(device_id).map(String::as_str)
    }

    pub(super) fn get(&self, dimension: Dimension, key: &str) -> Option<String> {
        match dimension {
            Dimension::User => self.user(key),
            Dimension::Device => self.device(key),
            Dimension::Tool | Dimension::Provider | Dimension::Model => None,
        }
        .map(str::to_owned)
    }
}

enum SqlValue {
    Integer(i64),
    Text(String),
}

/// `WHERE` conditions for a period and filter, with their bound values.
/// Column names come from [`Dimension::column`]; values are always bound.
fn conditions(start: i64, end: i64, filter: &Filter) -> (String, Vec<SqlValue>) {
    let mut conditions = String::from("bucket_start >= $1 AND bucket_start < $2");
    let mut values = vec![SqlValue::Integer(start), SqlValue::Integer(end)];
    for (dimension, value) in &filter.0 {
        values.push(SqlValue::Text(value.clone()));
        conditions.push_str(&format!(" AND {} = ${}", dimension.column(), values.len()));
    }
    (conditions, values)
}

async fn fetch(
    store: &UsageStore,
    sql: String,
    values: Vec<SqlValue>,
) -> Result<Vec<AnyRow>, sqlx::Error> {
    let mut query = sqlx::query(AssertSqlSafe(sql));
    for value in values {
        query = match value {
            SqlValue::Integer(value) => query.bind(value),
            SqlValue::Text(value) => query.bind(value),
        };
    }
    query.fetch_all(&store.pool).await
}

fn totals(row: &AnyRow, offset: usize) -> anyhow::Result<Totals> {
    let value = |index: usize| -> anyhow::Result<i64> {
        row.try_get(offset + index).context("decode usage totals")
    };
    Ok(Totals {
        requests: value(0)?,
        errors: value(1)?,
        input_tokens: value(2)?,
        output_tokens: value(3)?,
        cache_read_tokens: value(4)?,
        cache_write_tokens: value(5)?,
        cost_nanos: value(6)?,
        subscription_cost_nanos: value(7)?,
        subscription_requests: value(8)?,
        unpriced_requests: value(9)?,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use std::{path::Path, str::FromStr};

    use sqlx::{any::AnyConnectOptions, sqlite::SqliteConnectOptions};

    use super::{UsageStore, sqlite_url};

    /// A store in a new SQLite file that lives as long as the directory.
    pub(crate) async fn test_store() -> (UsageStore, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("create test directory");
        let store = UsageStore::open_sqlite(&directory.path().join("usage.db"))
            .await
            .expect("open test usage database");
        (store, directory)
    }

    pub(crate) fn pool(store: &UsageStore) -> &sqlx::AnyPool {
        &store.pool
    }

    #[test]
    fn sqlite_urls_name_the_exact_file() {
        for path in [
            "/Users/john/.local/state/agentdesktop/usage.db",
            "/tmp/odd dir/50% #1?/usage.db",
            r"C:\ProgramData\agentdesktop\usage.db",
            r"\\server\share\agentdesktop\usage.db",
        ] {
            let url = sqlite_url(Path::new(path)).unwrap();
            let options = AnyConnectOptions::from_str(&url).unwrap();
            let sqlite = SqliteConnectOptions::try_from(&options).unwrap();
            assert_eq!(sqlite.get_filename(), Path::new(path), "{url}");
        }
    }

    #[tokio::test]
    async fn opening_again_keeps_stored_usage_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.db");
        let first = UsageStore::open_sqlite(&path).await.unwrap();
        sqlx::query("INSERT INTO usage_receipts (id, received_at) VALUES ('kept', 1)")
            .execute(pool(&first))
            .await
            .unwrap();
        first.pool.close().await;
        let again = UsageStore::open_sqlite(&path).await.unwrap();
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_receipts")
            .fetch_one(pool(&again))
            .await
            .unwrap();
        assert_eq!(kept, 1);
    }
}
