//! LLM token and cost usage reported by agentgateway.
//!
//! agentgateway exports one OTLP log record per proxied request
//! (`frontendPolicies.accessLog.otlp`): to the controller's fleet API, or to
//! a daemon without a controller when the gateway runs on the same machine.
//! LLM requests are summed into hourly rollups per user, device, tool,
//! provider, model, and billing. Hourly rollups older than 90 days are
//! compacted into daily rollups, which are kept for 13 months.

mod otlp;
mod report;
mod store;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentdesktop_core::model;
use agentdesktop_proto::fleet;
use tracing::warn;

pub use otlp::{UsageIngest, UsageTokenCheck, load_token};
pub use report::{
    UsageBreakdownRow, UsageError, UsageFilterOption, UsageFilterOptions, UsageQuery, UsageReport,
    device_usage, report,
};
pub use store::{Labels, UsageStore};

/// The usage tables, as created in a database of their own.
pub const SCHEMA: &str = include_str!("../migrations/0001_usage.sql");

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;
/// Hourly rollups older than this are compacted into daily rollups.
const HOURLY_RETENTION: i64 = 90 * DAY;
/// Daily rollups older than this are deleted.
const DAILY_RETENTION: i64 = 395 * DAY;
/// Record identifiers are kept this long to drop records sent again.
const RECEIPT_RETENTION: i64 = 7 * DAY;
/// Largest accepted UTC offset, in minutes (UTC+14:00).
const MAX_UTC_OFFSET_MINUTES: i32 = 14 * 60;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Compacts and expires usage rollups now and then every hour.
pub fn spawn_maintenance(store: UsageStore) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(error) = store::compact(&store, unix_now()).await {
                warn!(error = %format!("{error:#}"), "usage maintenance failed");
            }
        }
    });
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Who pays for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Billing {
    /// Per-token API pricing.
    Api,
    /// A subscription; the gateway's catalog price is what the request would
    /// have cost at API rates.
    Subscription,
}

impl Billing {
    fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Subscription => "subscription",
        }
    }
}

/// The values usage is summed by within an hour.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RollupKey {
    bucket_start: i64,
    subject: String,
    device_id: String,
    client_id: String,
    provider: String,
    model: String,
    billing: Billing,
}

/// Additive measures stored with each rollup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Measures {
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_nanos: i64,
    unpriced_requests: i64,
}

impl Measures {
    fn add(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.errors = self.errors.saturating_add(other.errors);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cost_nanos = self.cost_nanos.saturating_add(other.cost_nanos);
        self.unpriced_requests = self
            .unpriced_requests
            .saturating_add(other.unpriced_requests);
    }
}

/// One LLM request read from an access-log record.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UsageRecord {
    /// Digest of the record, used to drop a record sent again.
    id: String,
    key: RollupKey,
    measures: Measures,
}

/// Summed usage, with spend split by billing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Totals {
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_nanos: i64,
    subscription_cost_nanos: i64,
    subscription_requests: i64,
    unpriced_requests: i64,
}

impl Totals {
    fn add(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.errors = self.errors.saturating_add(other.errors);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cost_nanos = self.cost_nanos.saturating_add(other.cost_nanos);
        self.subscription_cost_nanos = self
            .subscription_cost_nanos
            .saturating_add(other.subscription_cost_nanos);
        self.subscription_requests = self
            .subscription_requests
            .saturating_add(other.subscription_requests);
        self.unpriced_requests = self
            .unpriced_requests
            .saturating_add(other.unpriced_requests);
    }

    /// Ranking order for breakdowns: spend, then tokens, then requests.
    fn rank(&self) -> (i64, i64, i64) {
        (
            self.cost_nanos.saturating_add(self.subscription_cost_nanos),
            self.input_tokens.saturating_add(self.output_tokens),
            self.requests,
        )
    }

    fn to_model(self) -> model::UsageTotals {
        model::UsageTotals {
            requests: unsigned(self.requests),
            errors: unsigned(self.errors),
            input_tokens: unsigned(self.input_tokens),
            output_tokens: unsigned(self.output_tokens),
            cache_read_tokens: unsigned(self.cache_read_tokens),
            cache_write_tokens: unsigned(self.cache_write_tokens),
            cost_usd: dollars(self.cost_nanos),
            subscription_cost_usd: dollars(self.subscription_cost_nanos),
            subscription_requests: unsigned(self.subscription_requests),
            unpriced_requests: unsigned(self.unpriced_requests),
        }
    }

    fn to_proto(self) -> fleet::UsageTotals {
        fleet::UsageTotals {
            requests: unsigned(self.requests),
            errors: unsigned(self.errors),
            input_tokens: unsigned(self.input_tokens),
            output_tokens: unsigned(self.output_tokens),
            cache_read_tokens: unsigned(self.cache_read_tokens),
            cache_write_tokens: unsigned(self.cache_write_tokens),
            cost_nanos: self.cost_nanos,
            subscription_cost_nanos: self.subscription_cost_nanos,
            subscription_requests: unsigned(self.subscription_requests),
            unpriced_requests: unsigned(self.unpriced_requests),
        }
    }
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn dollars(nanos: i64) -> f64 {
    nanos as f64 / 1e9
}

/// A rollup column usage can be grouped or filtered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Dimension {
    User,
    Device,
    Tool,
    Provider,
    Model,
}

impl Dimension {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "user" => Some(Self::User),
            "device" => Some(Self::Device),
            "tool" => Some(Self::Tool),
            "provider" => Some(Self::Provider),
            "model" => Some(Self::Model),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Device => "device",
            Self::Tool => "tool",
            Self::Provider => "provider",
            Self::Model => "model",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::User => "subject",
            Self::Device => "device_id",
            Self::Tool => "client_id",
            Self::Provider => "provider",
            Self::Model => "model",
        }
    }
}

/// Exact-match conditions on rollup columns.
#[derive(Clone, Debug, Default)]
struct Filter(Vec<(Dimension, String)>);

impl Filter {
    fn with(mut self, dimension: Dimension, value: impl Into<String>) -> Self {
        self.0.push((dimension, value.into()));
        self
    }

    fn without(&self, dimensions: &[Dimension]) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(dimension, _)| !dimensions.contains(dimension))
                .cloned()
                .collect(),
        )
    }
}

/// A reporting period split into equal hourly or daily buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window {
    start: i64,
    end: i64,
    bucket_seconds: i64,
    utc_offset_seconds: i64,
}

impl Window {
    /// `24h` is the last 24 hours, hour by hour. `7d`, `30d`, and `90d` are
    /// that many local days ending today.
    fn preset(range: &str, now: i64, utc_offset_minutes: i32) -> Option<Self> {
        if !(-MAX_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&utc_offset_minutes) {
            return None;
        }
        let utc_offset_seconds = i64::from(utc_offset_minutes) * 60;
        let (bucket_seconds, buckets) = match range {
            "24h" => (HOUR, 24),
            "7d" => (DAY, 7),
            "30d" => (DAY, 30),
            "90d" => (DAY, 90),
            _ => return None,
        };
        let end = if bucket_seconds == HOUR {
            now.div_euclid(HOUR) * HOUR + HOUR
        } else {
            local_day_start(now, utc_offset_seconds) + DAY
        };
        Some(Self {
            start: end - buckets * bucket_seconds,
            end,
            bucket_seconds,
            utc_offset_seconds,
        })
    }

    /// The equally long period just before this one. Usage older than the
    /// hourly retention is kept per UTC day, so a period reaching that far
    /// starts at the nearest UTC midnight instead of splitting a day.
    fn previous(&self, now: i64) -> Self {
        let mut start = self.start - (self.end - self.start);
        if start < now - HOURLY_RETENTION {
            start = (start + DAY / 2).div_euclid(DAY) * DAY;
        }
        Self {
            start,
            end: self.start,
            ..*self
        }
    }

    fn bucket_count(&self) -> usize {
        usize::try_from((self.end - self.start) / self.bucket_seconds).unwrap_or_default()
    }

    fn bucket_start(&self, index: usize) -> i64 {
        self.start + i64::try_from(index).unwrap_or_default() * self.bucket_seconds
    }

    /// The bucket holding a stored rollup that starts at `rollup_start`.
    fn bucket_index(&self, rollup_start: i64) -> Option<usize> {
        let start = if self.bucket_seconds == DAY {
            local_day_start(rollup_start, self.utc_offset_seconds)
        } else {
            rollup_start.div_euclid(HOUR) * HOUR
        };
        if start < self.start || start >= self.end {
            return None;
        }
        usize::try_from((start - self.start) / self.bucket_seconds).ok()
    }
}

fn local_day_start(timestamp: i64, utc_offset_seconds: i64) -> i64 {
    (timestamp + utc_offset_seconds).div_euclid(DAY) * DAY - utc_offset_seconds
}

#[cfg(test)]
mod tests {
    use super::{DAY, HOUR, Window};

    // 2026-03-10T15:20:00Z
    const NOW: i64 = 1_773_156_000;

    #[test]
    fn hourly_window_ends_with_the_current_hour() {
        let window = Window::preset("24h", NOW, 0).unwrap();
        assert_eq!(window.end, NOW.div_euclid(HOUR) * HOUR + HOUR);
        assert_eq!(window.end - window.start, 24 * HOUR);
        assert_eq!(window.bucket_count(), 24);
        assert_eq!(window.bucket_index(NOW.div_euclid(HOUR) * HOUR), Some(23));
        assert_eq!(window.bucket_index(window.start - HOUR), None);
    }

    #[test]
    fn daily_window_follows_local_midnight() {
        // UTC-05:00: local midnight is 05:00 UTC.
        let window = Window::preset("7d", NOW, -300).unwrap();
        assert_eq!(window.end.rem_euclid(DAY), 5 * HOUR);
        assert_eq!(window.bucket_count(), 7);
        assert!(window.end > NOW && window.end - NOW <= DAY);
        // 03:00 UTC today is still yesterday locally.
        let today_utc = NOW.div_euclid(DAY) * DAY;
        assert_eq!(window.bucket_index(today_utc + 3 * HOUR), Some(5));
        assert_eq!(window.bucket_index(today_utc + 6 * HOUR), Some(6));
        assert_eq!(window.previous(NOW).end, window.start);
        assert_eq!(
            window.previous(NOW).end - window.previous(NOW).start,
            window.end - window.start
        );
    }

    #[test]
    fn a_previous_period_past_hourly_retention_starts_at_a_utc_midnight() {
        // UTC-05:00 days start at 05:00 UTC, but usage that old is kept per
        // UTC day.
        let window = Window::preset("90d", NOW, -300).unwrap();
        let previous = window.previous(NOW);
        assert_eq!(previous.end, window.start);
        assert_eq!(previous.start, window.start - 90 * DAY - 5 * HOUR);
        // UTC+02:00 days start two hours before a UTC midnight.
        let window = Window::preset("90d", NOW, 120).unwrap();
        assert_eq!(
            window.previous(NOW).start,
            window.start - 90 * DAY + 2 * HOUR
        );
    }

    #[test]
    fn unknown_ranges_and_offsets_are_rejected() {
        assert!(Window::preset("1y", NOW, 0).is_none());
        assert!(Window::preset("7d", NOW, 15 * 60).is_none());
        assert!(Window::preset("7d", NOW, -15 * 60).is_none());
        assert!(Window::preset("7d", NOW, 14 * 60).is_some());
    }
}
