//! Periodic re-apply of the daemon's current configuration (opt-in,
//! `daemon.reconcileInterval`).
//!
//! A managed file deleted or edited by hand, a conflict the user has fixed,
//! or a managed file whose mode was loosened stay as they are until the next
//! config push or restart. [`run_tick`] repairs that by re-applying the
//! current configuration every `reconcileInterval`, without rewriting
//! anything unchanged (see [`crate::reconcile::plan`]).

use std::{sync::Arc, time::Duration};

use agentdesktop_core::config::DaemonConfig;
use tokio::sync::watch;

use crate::reconcile::{ApplyReport, Reconciler};

/// The daemon's current configuration: the startup configuration
/// (`revision: None`), or the latest pushed configuration that passed hash,
/// UTF-8 and parse (`revision: Some(rev)`), whether its apply then succeeded
/// or failed. A push that does not parse leaves this unchanged.
#[derive(Clone)]
pub(crate) struct CurrentConfig {
    pub(crate) revision: Option<u64>,
    pub(crate) config: Arc<DaemonConfig>,
}

/// One tick's outcome, forwarded to the connection loop for reporting. A
/// tick never persists anything or changes the current configuration.
#[derive(Clone)]
pub(crate) struct TickStatus {
    pub(crate) revision: Option<u64>,
    pub(crate) report: ApplyReport,
    pub(crate) error: Option<String>,
}

/// Applies `current`'s configuration every `interval`, first one interval
/// after start (`interval_at`, `MissedTickBehavior::Delay`), skipping a tick
/// while `current` holds `None`. Stops only when `current`'s sender is
/// dropped (daemon shutdown). `interval` must be greater than zero; a zero
/// interval is refused (logged) rather than scheduled.
pub(crate) async fn run_tick_with<F>(
    current: watch::Receiver<Option<CurrentConfig>>,
    interval: Duration,
    statuses: Option<watch::Sender<Option<TickStatus>>>,
    apply: F,
) where
    F: FnMut(&DaemonConfig) -> (ApplyReport, anyhow::Result<()>),
{
    if interval.is_zero() {
        tracing::warn!("reconcile interval is zero; not scheduling periodic re-applies");
        return;
    }
    let mut current = current;
    let mut apply = apply;
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            changed = current.changed() => {
                // A new configuration is picked up at the next tick; a
                // dropped sender (daemon shutdown) ends the loop.
                if changed.is_err() {
                    return;
                }
                continue;
            }
        }
        let Some(snapshot) = current.borrow_and_update().clone() else {
            continue;
        };
        let (report, result) = apply(&snapshot.config);
        if let Some(statuses) = &statuses {
            statuses.send_replace(Some(TickStatus {
                revision: snapshot.revision,
                report,
                error: result.err().map(|error| format!("{error:#}")),
            }));
        }
    }
}

/// [`run_tick_with`] applying through a real [`Reconciler`].
pub(crate) async fn run_tick(
    reconciler: Reconciler,
    current: watch::Receiver<Option<CurrentConfig>>,
    interval: Duration,
    statuses: Option<watch::Sender<Option<TickStatus>>>,
) {
    let latest = current.clone();
    run_tick_with(current, interval, statuses, move |_| {
        // Read again under the apply lock: a push that took the lock first
        // may have replaced the configuration this tick woke up with.
        let Some((previous, report, result)) = reconciler.apply_read_under_lock(|| {
            latest
                .borrow()
                .as_ref()
                .map(|current| Arc::clone(&current.config))
        }) else {
            return (ApplyReport::default(), Ok(()));
        };
        // Outcome lines and the failure warning only when something changed,
        // so a conflict that persists does not repeat every interval.
        if crate::reconcile::should_log(previous.as_ref(), &report) {
            report.log();
            if let Err(error) = &result {
                tracing::warn!(error = %format!("{error:#}"), "reconcile tick failed");
            }
        }
        (report, result)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use agentdesktop_core::config::DaemonConfig;
    use tokio::{sync::watch, time::Duration};

    use super::{CurrentConfig, run_tick_with};
    use crate::reconcile::ApplyReport;

    fn current_config(revision: Option<u64>) -> CurrentConfig {
        CurrentConfig {
            revision,
            config: Arc::new(DaemonConfig::default()),
        }
    }

    fn counting(
        count: Arc<AtomicUsize>,
    ) -> impl FnMut(&DaemonConfig) -> (ApplyReport, anyhow::Result<()>) {
        move |_config| {
            count.fetch_add(1, Ordering::SeqCst);
            (ApplyReport::default(), Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_to_schedule_a_zero_interval() {
        let (_current_tx, current_rx) = watch::channel(Some(current_config(None)));
        run_tick_with(current_rx, Duration::ZERO, None, |_: &DaemonConfig| {
            panic!("a zero interval must not apply")
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_before_one_interval_then_one_apply_per_interval() {
        let (_current_tx, current_rx) = watch::channel(Some(current_config(None)));
        let (status_tx, mut status_rx) = watch::channel(None);
        let count = Arc::new(AtomicUsize::new(0));
        let _ticker = tokio::spawn(run_tick_with(
            current_rx,
            Duration::from_secs(60),
            Some(status_tx),
            counting(Arc::clone(&count)),
        ));

        assert!(
            tokio::time::timeout(Duration::from_secs(59), status_rx.changed())
                .await
                .is_err(),
            "must not apply before one interval elapses"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);

        status_rx.changed().await.unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "one apply after one interval"
        );

        status_rx.changed().await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2, "one apply per interval");
    }

    #[tokio::test(start_paused = true)]
    async fn skips_ticks_while_the_current_configuration_is_none() {
        let (current_tx, current_rx) = watch::channel(None);
        let count = Arc::new(AtomicUsize::new(0));
        let ticker = tokio::spawn(run_tick_with(
            current_rx,
            Duration::from_secs(60),
            None,
            counting(Arc::clone(&count)),
        ));

        tokio::time::sleep(Duration::from_secs(180)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "no tick applies while the current configuration is None"
        );

        current_tx.send(Some(current_config(None))).unwrap();
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "a tick applies once a configuration is set"
        );
        ticker.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn stops_when_the_current_configuration_sender_is_dropped() {
        let (current_tx, current_rx) = watch::channel(Some(current_config(None)));
        let count = Arc::new(AtomicUsize::new(0));
        let ticker = tokio::spawn(run_tick_with(
            current_rx,
            Duration::from_secs(60),
            None,
            counting(Arc::clone(&count)),
        ));
        drop(current_tx);

        assert!(
            tokio::time::timeout(Duration::from_secs(600), ticker)
                .await
                .expect("run_tick_with must stop once the sender is dropped")
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_apply_forwards_the_error_and_revision() {
        let (_current_tx, current_rx) = watch::channel(Some(current_config(Some(9))));
        let (status_tx, mut status_rx) = watch::channel(None);
        let _ticker = tokio::spawn(run_tick_with(
            current_rx,
            Duration::from_secs(30),
            Some(status_tx),
            |_: &DaemonConfig| (ApplyReport::default(), Err(anyhow::anyhow!("disk full"))),
        ));

        status_rx.changed().await.unwrap();
        let status = status_rx
            .borrow_and_update()
            .clone()
            .expect("a tick status must be published");
        assert_eq!(status.revision, Some(9));
        assert!(
            status
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("disk full"),
            "{:?}",
            status.error
        );
    }
}
