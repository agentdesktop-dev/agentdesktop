//! Removal of device records that have been inactive for a while.
//!
//! Every enrollment creates a device record. A device that enrolls again,
//! because its sign-in expired, it was reinstalled, or its stored identity
//! became unreadable, gets a new record under a new device ID, and the old one
//! never connects again. Nothing removes it, so the device list fills with
//! offline copies of the same machines. With `inactiveDevices` set, the
//! controller removes the records it has not heard from for `removeAfter`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::{database::Database, fleet_limits::DeviceStreams};

/// How often inactive device records are looked for.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Removes inactive device records every hour. The first run is an hour after
/// startup, not at once: until connected devices have reconnected to a freshly
/// started controller, none of them has an open stream to be kept by.
pub(crate) fn spawn(database: Database, streams: DeviceStreams, remove_after: Duration) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + SWEEP_INTERVAL, SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            let is_connected = |device_id: &str| streams.has_open_stream(device_id);
            if let Err(error) = sweep(&database, is_connected, unix_now(), remove_after).await {
                warn!(error = %format!("{error:#}"), "inactive device removal failed");
            }
        }
    });
}

/// Removes the devices last seen more than `remove_after` before `now` (Unix
/// seconds), except those `is_connected` reports as having an open stream.
/// Heartbeats carry the device's clock, so the open-stream check is what keeps
/// a connected device whose clock is behind. Returns how many were removed.
pub(crate) async fn sweep(
    database: &Database,
    is_connected: impl Fn(&str) -> bool,
    now: i64,
    remove_after: Duration,
) -> anyhow::Result<usize> {
    let seen_before = now.saturating_sub(i64::try_from(remove_after.as_secs()).unwrap_or(i64::MAX));
    let mut removed = 0;
    for (device_id, hostname) in database.inactive_devices(seen_before).await? {
        if is_connected(&device_id) {
            continue;
        }
        if database
            .delete_inactive_device(&device_id, seen_before)
            .await?
        {
            removed += 1;
            info!(%device_id, %hostname, "removed inactive device");
        }
    }
    Ok(removed)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::sweep;
    use crate::database::Database;

    const DAY: i64 = 24 * 60 * 60;

    async fn test_database() -> (Database, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-inactive-devices-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect test database");
        (database, path)
    }

    /// Enrolls `device_id` and sets its last contact to `last_seen`.
    async fn device(database: &Database, device_id: &str, last_seen: i64) {
        database
            .enroll_device(
                device_id,
                &format!("{device_id}-host"),
                "issuer",
                "subject",
                None,
            )
            .await
            .expect("enroll device");
        database
            .update_heartbeat(device_id, last_seen.try_into().unwrap())
            .await
            .expect("set last contact");
    }

    async fn exists(database: &Database, device_id: &str) -> bool {
        database
            .get_device(device_id)
            .await
            .expect("load device")
            .is_some()
    }

    #[tokio::test]
    async fn removes_only_devices_inactive_longer_than_remove_after() {
        let (database, path) = test_database().await;
        let now = 1_800_000_000;
        device(&database, "active", now - 60).await;
        device(&database, "recent", now - 29 * DAY).await;
        device(&database, "stale", now - 31 * DAY).await;
        device(&database, "superseded", now - 90 * DAY).await;

        let removed = sweep(
            &database,
            |_| false,
            now,
            Duration::from_secs(30 * 24 * 60 * 60),
        )
        .await
        .expect("sweep");

        assert_eq!(removed, 2);
        assert!(exists(&database, "active").await);
        assert!(exists(&database, "recent").await);
        assert!(!exists(&database, "stale").await);
        assert!(!exists(&database, "superseded").await);

        // Nothing is left to remove on the next run.
        let removed = sweep(
            &database,
            |_| false,
            now,
            Duration::from_secs(30 * 24 * 60 * 60),
        )
        .await
        .expect("second sweep");
        assert_eq!(removed, 0);

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn keeps_a_device_with_an_open_stream_whatever_its_last_contact() {
        let (database, path) = test_database().await;
        let now = 1_800_000_000;
        // A connected device whose clock is far behind reports old heartbeats.
        device(&database, "connected", now - 365 * DAY).await;
        device(&database, "gone", now - 365 * DAY).await;

        let removed = sweep(
            &database,
            |device_id| device_id == "connected",
            now,
            Duration::from_secs(30 * 24 * 60 * 60),
        )
        .await
        .expect("sweep");

        assert_eq!(removed, 1);
        assert!(exists(&database, "connected").await);
        assert!(!exists(&database, "gone").await);

        drop(database);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn keeps_a_device_that_connects_after_it_was_listed() {
        let (database, path) = test_database().await;
        let now = 1_800_000_000;
        device(&database, "returning", now - 40 * DAY).await;
        let seen_before = now - 30 * DAY;
        assert_eq!(
            database.inactive_devices(seen_before).await.unwrap(),
            [("returning".to_owned(), "returning-host".to_owned())]
        );

        // It reconnects between the listing and the delete.
        database
            .update_heartbeat("returning", now.try_into().unwrap())
            .await
            .unwrap();

        assert!(
            !database
                .delete_inactive_device("returning", seen_before)
                .await
                .unwrap()
        );
        assert!(exists(&database, "returning").await);

        drop(database);
        let _ = std::fs::remove_file(path);
    }
}
