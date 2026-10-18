//! Usage reports for the admin UI and for devices.

use std::collections::BTreeMap;

use agentdesktop_core::model;
use agentdesktop_proto::fleet;
use serde::{Deserialize, Serialize};

use super::{Dimension, Filter, Labels, Totals, UsageStore, Window, store, unix_now};

/// Rows returned per breakdown, largest spend first.
const MAX_BREAKDOWN_ROWS: usize = 200;

#[derive(Debug)]
pub enum UsageError {
    /// The request named an unknown range, offset, or dimension.
    InvalidQuery(&'static str),
    Storage(anyhow::Error),
}

impl From<anyhow::Error> for UsageError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

/// Parameters of the admin usage report. Filters match exact values; an
/// empty value selects usage the gateway did not attribute.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQuery {
    /// `24h`, `7d` (default), `30d`, or `90d`.
    pub range: Option<String>,
    /// Comma-separated breakdowns: `user`, `device`, `tool`, `provider`, `model`.
    pub by: Option<String>,
    /// Offset from UTC that daily buckets follow.
    pub utc_offset_minutes: Option<i32>,
    pub user: Option<String>,
    pub device: Option<String>,
    pub tool: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    /// Whether the controller accepts usage from agentgateway.
    pub enabled: bool,
    pub range: String,
    pub from_unix_seconds: i64,
    pub to_unix_seconds: i64,
    pub bucket_seconds: i64,
    pub totals: model::UsageTotals,
    /// Totals for the equally long period before this one. Usage older than
    /// 90 days is kept per UTC day, so that far back the period starts at
    /// the nearest UTC midnight.
    pub previous_totals: model::UsageTotals,
    pub active_users: i64,
    pub active_devices: i64,
    pub buckets: Vec<model::UsageBucket>,
    /// Requested breakdowns by dimension name.
    pub breakdowns: BTreeMap<&'static str, Vec<UsageBreakdownRow>>,
    pub filter_options: UsageFilterOptions,
}

/// Values the user and device filters offer. Users ignore both of those
/// filters and devices only their own, so devices narrow to the chosen user.
#[derive(Debug, Serialize)]
pub struct UsageFilterOptions {
    pub user: Vec<UsageFilterOption>,
    pub device: Vec<UsageFilterOption>,
}

#[derive(Debug, Serialize)]
pub struct UsageFilterOption {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdownRow {
    /// Stored value; empty when the gateway did not attribute the usage.
    pub key: String,
    /// User email or device hostname, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub totals: model::UsageTotals,
}

/// Fleet-wide usage for the admin UI.
pub async fn report(
    store: &UsageStore,
    query: &UsageQuery,
    enabled: bool,
    labels: &Labels,
) -> Result<UsageReport, UsageError> {
    let range = query.range.as_deref().unwrap_or("7d");
    let now = unix_now();
    let window = Window::preset(range, now, query.utc_offset_minutes.unwrap_or(0)).ok_or(
        UsageError::InvalidQuery("unknown usage range or UTC offset"),
    )?;
    let dimensions = query
        .by
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            Dimension::parse(name).ok_or(UsageError::InvalidQuery("unknown usage breakdown"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut filter = Filter::default();
    for (dimension, value) in [
        (Dimension::User, &query.user),
        (Dimension::Device, &query.device),
        (Dimension::Tool, &query.tool),
        (Dimension::Provider, &query.provider),
        (Dimension::Model, &query.model),
    ] {
        if let Some(value) = value {
            filter = filter.with(dimension, value.as_str());
        }
    }

    let current = store::summary(store, window.start, window.end, &filter).await?;
    let previous_window = window.previous(now);
    let previous =
        store::summary(store, previous_window.start, previous_window.end, &filter).await?;
    let buckets = buckets(
        &window,
        store::series(store, window.start, window.end, &filter).await?,
    );
    let mut breakdowns = BTreeMap::new();
    for dimension in dimensions {
        let rows: Vec<UsageBreakdownRow> =
            ranked(store::breakdown(store, window.start, window.end, &filter, dimension).await?)
                .into_iter()
                .map(|(key, totals)| UsageBreakdownRow {
                    label: labels.get(dimension, &key),
                    key,
                    totals: totals.to_model(),
                })
                .collect();
        breakdowns.insert(dimension.name(), rows);
    }
    let options = |rows: Vec<(String, Totals)>, dimension: Dimension| -> Vec<UsageFilterOption> {
        ranked(rows)
            .into_iter()
            .map(|(key, _)| UsageFilterOption {
                label: labels.get(dimension, &key),
                key,
            })
            .collect()
    };
    let filter_options = UsageFilterOptions {
        user: options(
            store::breakdown(
                store,
                window.start,
                window.end,
                &filter.without(&[Dimension::User, Dimension::Device]),
                Dimension::User,
            )
            .await?,
            Dimension::User,
        ),
        device: options(
            store::breakdown(
                store,
                window.start,
                window.end,
                &filter.without(&[Dimension::Device]),
                Dimension::Device,
            )
            .await?,
            Dimension::Device,
        ),
    };
    Ok(UsageReport {
        enabled,
        range: range.to_owned(),
        from_unix_seconds: window.start,
        to_unix_seconds: window.end,
        bucket_seconds: window.bucket_seconds,
        totals: current.totals.to_model(),
        previous_totals: previous.totals.to_model(),
        active_users: current.users,
        active_devices: current.devices,
        buckets: buckets
            .into_iter()
            .map(|(start, totals)| model::UsageBucket {
                start_unix_seconds: u64::try_from(start).unwrap_or_default(),
                totals: totals.to_model(),
            })
            .collect(),
        breakdowns,
        filter_options,
    })
}

/// Usage of one device, which the caller has authenticated. `None` reports
/// everything stored, for a daemon that stores only its own usage.
pub async fn device_usage(
    store: &UsageStore,
    device_id: Option<&str>,
    request: &fleet::DeviceUsageRequest,
    enabled: bool,
) -> Result<fleet::DeviceUsageResponse, UsageError> {
    let window = Window::preset(&request.range, unix_now(), request.utc_offset_minutes).ok_or(
        UsageError::InvalidQuery("unknown usage range or UTC offset"),
    )?;
    let filter = match device_id {
        Some(device_id) => Filter::default().with(Dimension::Device, device_id),
        None => Filter::default(),
    };
    let summary = store::summary(store, window.start, window.end, &filter).await?;
    let buckets = buckets(
        &window,
        store::series(store, window.start, window.end, &filter).await?,
    );
    let groups = |rows: Vec<(String, Totals)>| -> Vec<fleet::UsageGroup> {
        ranked(rows)
            .into_iter()
            .map(|(key, totals)| fleet::UsageGroup {
                key,
                totals: Some(totals.to_proto()),
            })
            .collect()
    };
    Ok(fleet::DeviceUsageResponse {
        enabled,
        from_unix_seconds: u64::try_from(window.start).unwrap_or_default(),
        to_unix_seconds: u64::try_from(window.end).unwrap_or_default(),
        bucket_seconds: u64::try_from(window.bucket_seconds).unwrap_or_default(),
        totals: Some(summary.totals.to_proto()),
        buckets: buckets
            .into_iter()
            .map(|(start, totals)| fleet::UsageBucket {
                start_unix_seconds: u64::try_from(start).unwrap_or_default(),
                totals: Some(totals.to_proto()),
            })
            .collect(),
        by_tool: groups(
            store::breakdown(store, window.start, window.end, &filter, Dimension::Tool).await?,
        ),
        by_model: groups(
            store::breakdown(store, window.start, window.end, &filter, Dimension::Model).await?,
        ),
    })
}

/// Every bucket of the window, including empty ones, from stored rollups.
fn buckets(window: &Window, rollups: Vec<(i64, Totals)>) -> Vec<(i64, Totals)> {
    let mut buckets: Vec<(i64, Totals)> = (0..window.bucket_count())
        .map(|index| (window.bucket_start(index), Totals::default()))
        .collect();
    for (rollup_start, totals) in rollups {
        if let Some((_, bucket)) = window
            .bucket_index(rollup_start)
            .and_then(|index| buckets.get_mut(index))
        {
            bucket.add(&totals);
        }
    }
    buckets
}

fn ranked(mut rows: Vec<(String, Totals)>) -> Vec<(String, Totals)> {
    rows.sort_by(|(left_key, left), (right_key, right)| {
        right
            .rank()
            .cmp(&left.rank())
            .then_with(|| left_key.cmp(right_key))
    });
    rows.truncate(MAX_BREAKDOWN_ROWS);
    rows
}

#[cfg(test)]
mod tests {
    use agentdesktop_proto::{
        fleet::DeviceUsageRequest,
        otlp_logs::{
            ExportLogsPartialSuccess, ExportLogsServiceRequest, LogRecord, ResourceLogs, ScopeLogs,
            any_value::Value, logs_service_server::LogsService,
        },
    };
    use tonic::Request;

    use super::{UsageError, UsageFilterOption, UsageQuery, device_usage, report};
    use crate::{
        DAY, Filter, HOUR, Labels, UsageIngest, UsageStore, Window,
        otlp::tests::{DEVICE_ID, attribute, llm_record, standalone_record, text},
        store::{
            self,
            tests::{pool, test_store},
        },
        unix_now,
    };

    async fn export(
        store: &UsageStore,
        records: Vec<LogRecord>,
    ) -> Option<ExportLogsPartialSuccess> {
        let ingest = UsageIngest {
            store: store.clone(),
        };
        ingest
            .export(Request::new(ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    scope_logs: vec![ScopeLogs {
                        log_records: records,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            }))
            .await
            .expect("export usage")
            .into_inner()
            .partial_success
    }

    #[tokio::test]
    async fn exported_usage_is_reported_once_with_subscription_spend_apart() {
        let (store, _directory) = test_store().await;
        let mut labels = Labels::default();
        labels.insert_device(DEVICE_ID, "build-mac");
        labels.insert_user("user-123", "john@example.com");
        let now = unix_now();
        let api = llm_record(now - 60, Vec::new());
        let subscription = llm_record(
            now - 120,
            vec![
                attribute("agentdesktop.billing", text("subscription")),
                attribute("agentdesktop.client", text("claude-desktop")),
            ],
        );
        let other_device = llm_record(
            now - 180,
            vec![attribute(
                "agentdesktop.device",
                text("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            )],
        );
        let not_llm = LogRecord {
            attributes: vec![attribute("http.response.status_code", Value::IntValue(200))],
            ..Default::default()
        };
        let mut malformed = llm_record(now, Vec::new());
        malformed
            .attributes
            .retain(|attribute| attribute.key != "gen_ai.usage.output_tokens");
        malformed
            .attributes
            .push(attribute("gen_ai.usage.output_tokens", Value::IntValue(-5)));

        let partial = export(
            &store,
            vec![
                api.clone(),
                subscription.clone(),
                other_device,
                not_llm,
                malformed,
            ],
        )
        .await
        .expect("the malformed record is reported");
        assert_eq!(partial.rejected_log_records, 1);
        // The same records again are not counted twice.
        assert!(export(&store, vec![api, subscription]).await.is_none());

        let fleet = report(
            &store,
            &UsageQuery {
                range: Some("24h".to_owned()),
                by: Some("user,device,tool".to_owned()),
                ..Default::default()
            },
            true,
            &labels,
        )
        .await
        .unwrap();
        assert_eq!(fleet.totals.requests, 3);
        assert_eq!(fleet.totals.subscription_requests, 1);
        assert!((fleet.totals.cost_usd - 0.0162).abs() < 1e-9);
        assert!((fleet.totals.subscription_cost_usd - 0.0081).abs() < 1e-9);
        assert_eq!(fleet.totals.input_tokens, 3600);
        assert_eq!(fleet.active_users, 1);
        assert_eq!(fleet.active_devices, 2);
        assert_eq!(fleet.buckets.len(), 24);
        assert_eq!(
            fleet
                .buckets
                .iter()
                .map(|bucket| bucket.totals.requests)
                .sum::<u64>(),
            3
        );
        let users = &fleet.breakdowns["user"];
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].label.as_deref(), Some("john@example.com"));
        let devices = &fleet.breakdowns["device"];
        assert_eq!(
            devices[0].key, DEVICE_ID,
            "the device with more spend is first"
        );
        assert_eq!(devices[0].label.as_deref(), Some("build-mac"));
        assert_eq!(devices[1].label, None);
        let tools = &fleet.breakdowns["tool"];
        assert_eq!(tools[0].key, "claude-code");
        assert_eq!(tools[1].key, "claude-desktop");

        let filtered = report(
            &store,
            &UsageQuery {
                range: Some("24h".to_owned()),
                tool: Some("claude-desktop".to_owned()),
                ..Default::default()
            },
            true,
            &labels,
        )
        .await
        .unwrap();
        assert_eq!(filtered.totals.requests, 1);
        assert!(filtered.breakdowns.is_empty());

        let device = device_usage(
            &store,
            Some(DEVICE_ID),
            &DeviceUsageRequest {
                range: "7d".to_owned(),
                utc_offset_minutes: -300,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(device.totals.unwrap().requests, 2, "only this device");
        assert_eq!(device.bucket_seconds, DAY as u64);
        assert_eq!(device.buckets.len(), 7);
        assert_eq!(device.by_tool.len(), 2);
        assert_eq!(device.by_model[0].key, "claude-sonnet-4-5-20250929");
    }

    #[tokio::test]
    async fn a_daemon_reports_everything_it_stored() {
        let (store, _directory) = test_store().await;
        let now = unix_now();
        assert!(
            export(
                &store,
                vec![
                    standalone_record(now - 60, "claude-cli/2.1.260 (external, cli)"),
                    standalone_record(now - 120, "codex_cli_rs/0.153.4"),
                    standalone_record(now - 180, "curl/8.7.1"),
                ],
            )
            .await
            .is_none()
        );
        let usage = device_usage(
            &store,
            None,
            &DeviceUsageRequest {
                range: "24h".to_owned(),
                utc_offset_minutes: 0,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(usage.totals.unwrap().requests, 3);
        let mut tools: Vec<&str> = usage
            .by_tool
            .iter()
            .map(|group| group.key.as_str())
            .collect();
        tools.sort_unstable();
        assert_eq!(tools, ["", "claude-code", "codex"]);
    }

    #[tokio::test]
    async fn filter_options_list_every_user_and_the_chosen_users_devices() {
        let (store, _directory) = test_store().await;
        let now = unix_now();
        let laptop = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let workstation = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let records = vec![
            llm_record(now - 60, Vec::new()),
            llm_record(
                now - 120,
                vec![attribute("agentdesktop.device", text(laptop))],
            ),
            llm_record(
                now - 180,
                vec![
                    attribute("agentdesktop.subject", text("user-456")),
                    attribute("agentdesktop.device", text(workstation)),
                    attribute("agentdesktop.client", text("codex")),
                ],
            ),
        ];
        assert!(export(&store, records).await.is_none());
        let keys = |options: &[UsageFilterOption]| {
            let mut keys: Vec<String> = options.iter().map(|option| option.key.clone()).collect();
            keys.sort();
            keys
        };

        let chosen = report(
            &store,
            &UsageQuery {
                range: Some("24h".to_owned()),
                user: Some("user-123".to_owned()),
                device: Some(DEVICE_ID.to_owned()),
                ..Default::default()
            },
            true,
            &Labels::default(),
        )
        .await
        .unwrap();
        assert_eq!(chosen.totals.requests, 1);
        assert_eq!(
            keys(&chosen.filter_options.user),
            ["user-123", "user-456"],
            "every user, whatever the user and device filters"
        );
        assert_eq!(
            keys(&chosen.filter_options.device),
            [DEVICE_ID, laptop],
            "the chosen user's devices"
        );

        let codex = report(
            &store,
            &UsageQuery {
                range: Some("24h".to_owned()),
                tool: Some("codex".to_owned()),
                ..Default::default()
            },
            true,
            &Labels::default(),
        )
        .await
        .unwrap();
        assert_eq!(keys(&codex.filter_options.user), ["user-456"]);
        assert_eq!(keys(&codex.filter_options.device), [workstation]);
    }

    #[tokio::test]
    async fn invalid_queries_are_rejected() {
        let (store, _directory) = test_store().await;
        for query in [
            UsageQuery {
                range: Some("1y".to_owned()),
                ..Default::default()
            },
            UsageQuery {
                by: Some("user,team".to_owned()),
                ..Default::default()
            },
            UsageQuery {
                utc_offset_minutes: Some(24 * 60),
                ..Default::default()
            },
        ] {
            assert!(matches!(
                report(&store, &query, true, &Labels::default()).await,
                Err(UsageError::InvalidQuery(_))
            ));
        }
    }

    #[tokio::test]
    async fn old_hourly_usage_is_compacted_into_days_and_expired() {
        let (store, _directory) = test_store().await;
        let now = unix_now();
        let old = now - 100 * DAY;
        let day_start = old.div_euclid(DAY) * DAY;
        // Two hours of the same day, then another hour of it sent late.
        let records = [day_start + HOUR, day_start + 5 * HOUR]
            .into_iter()
            .map(|at| llm_record(at, Vec::new()))
            .collect();
        assert!(export(&store, records).await.is_none());
        store::compact(&store, now).await.unwrap();
        assert!(
            export(&store, vec![llm_record(day_start + 7 * HOUR, Vec::new())])
                .await
                .is_none()
        );
        store::compact(&store, now).await.unwrap();

        let rows: Vec<(i64, i64, i64)> =
            sqlx::query_as("SELECT bucket_start, bucket_seconds, requests FROM usage_rollups")
                .fetch_all(pool(&store))
                .await
                .unwrap();
        assert_eq!(rows, vec![(day_start, DAY, 3)]);
        let totals = store::summary(&store, day_start, day_start + DAY, &Filter::default())
            .await
            .unwrap()
            .totals;
        assert_eq!(totals.input_tokens, 3 * 1200);
        assert_eq!(totals.cost_nanos, 3 * 8_100_000);

        store::compact(&store, now + 400 * DAY).await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_rollups")
            .fetch_one(pool(&store))
            .await
            .unwrap();
        assert_eq!(remaining, 0, "daily usage expires after 13 months");
    }

    #[tokio::test]
    async fn the_previous_ninety_days_count_whole_compacted_days() {
        let (store, _directory) = test_store().await;
        let now = unix_now();
        // UTC-05:00: the previous 90 days start at 05:00 UTC, in usage already
        // compacted into UTC days.
        let window = Window::preset("90d", now, -300).unwrap();
        let first_day = window.start - 90 * DAY;
        let records = [
            first_day - 12 * HOUR,
            first_day + 10 * HOUR,
            window.start - HOUR,
        ]
        .into_iter()
        .map(|at| llm_record(at, Vec::new()))
        .collect();
        assert!(export(&store, records).await.is_none());
        store::compact(&store, now).await.unwrap();

        let fleet = report(
            &store,
            &UsageQuery {
                range: Some("90d".to_owned()),
                utc_offset_minutes: Some(-300),
                ..Default::default()
            },
            true,
            &Labels::default(),
        )
        .await
        .unwrap();
        assert_eq!(fleet.totals.requests, 0);
        assert_eq!(
            fleet.previous_totals.requests, 2,
            "the first day counts in full, the day before it not at all"
        );
    }
}
