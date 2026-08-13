use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use agentdesktop_proto::fleet::DaemonConfig;
use anyhow::Context;
use notify::{Event, EventKind, RecursiveMode};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

/// Live, last-known-good daemon configuration shared by controller surfaces.
#[derive(Clone)]
pub struct DaemonConfigStore {
    sender: watch::Sender<Option<DaemonConfig>>,
    reload_error: Arc<RwLock<Option<String>>>,
}

impl DaemonConfigStore {
    pub fn new(initial: Option<DaemonConfig>) -> Self {
        let (sender, _) = watch::channel(initial);
        Self {
            sender,
            reload_error: Arc::new(RwLock::new(None)),
        }
    }

    pub fn current(&self) -> Option<DaemonConfig> {
        self.sender.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<DaemonConfig>> {
        self.sender.subscribe()
    }

    pub fn reload_error(&self) -> Option<String> {
        self.reload_error
            .read()
            .expect("daemon-config reload status lock poisoned")
            .clone()
    }

    fn publish(&self, next: DaemonConfig) -> bool {
        let unchanged = self.sender.borrow().as_ref().is_some_and(|current| {
            current.revision == next.revision && current.sha256 == next.sha256
        });
        *self
            .reload_error
            .write()
            .expect("daemon-config reload status lock poisoned") = None;
        if unchanged {
            return false;
        }
        self.sender.send_replace(Some(next));
        true
    }

    fn record_error(&self, error: &anyhow::Error) {
        *self
            .reload_error
            .write()
            .expect("daemon-config reload status lock poisoned") = Some(format!("{error:#}"));
    }
}

/// Watches a daemon-configuration file and publishes validated changes.
///
/// The parent directory is always watched so atomic replacement continues to
/// work after an inode watch is invalidated. Each debounced batch also
/// re-resolves symlinks, covering Kubernetes projected-volume rotations.
pub fn watch(path: PathBuf, revision: u64, store: DaemonConfigStore) -> anyhow::Result<()> {
    let path = normalize_watch_path(&path)?;
    let parent = path
        .parent()
        .context("daemon configuration path has no parent")?
        .to_path_buf();
    let (event_sender, mut events) = mpsc::unbounded_channel();
    let mut watcher =
        notify_debouncer_full::new_debouncer(Duration::from_millis(250), None, move |result| {
            let _ = event_sender.send(result);
        })
        .context("create daemon configuration watcher")?;

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch daemon configuration directory {}", parent.display()))?;
    info!(path = %path.display(), "watching daemon configuration");
    tokio::spawn(async move {
        let mut target = resolve_target(&path);
        while let Some(result) = events.recv().await {
            let batch = match result {
                Ok(batch) => batch,
                Err(errors) => {
                    warn!(?errors, "daemon configuration watch error");
                    continue;
                }
            };
            let next_target = resolve_target(&path);
            let target_rotated = target != next_target;
            let relevant = target_rotated
                || batch.iter().any(|event| {
                    event_triggers_reload(
                        &event.event,
                        &path,
                        target.as_deref(),
                        next_target.as_deref(),
                    )
                })
                || batch
                    .iter()
                    .any(|event| directory_structure_changed(&event.event, &parent));
            target = next_target;
            if !relevant {
                continue;
            }

            match load(&path, revision) {
                Ok(config) => {
                    if store.publish(config) {
                        info!(path = %path.display(), revision, "reloaded daemon configuration");
                    }
                }
                Err(error) => {
                    store.record_error(&error);
                    error!(
                        path = %path.display(),
                        error = %format!("{error:#}"),
                        "failed to reload daemon configuration; retaining last good configuration"
                    );
                }
            }
        }
        drop(watcher);
    });
    Ok(())
}

pub fn load(path: &Path, revision: u64) -> anyhow::Result<DaemonConfig> {
    let yaml = std::fs::read(path)
        .with_context(|| format!("read daemon configuration from {}", path.display()))?;
    let text = std::str::from_utf8(&yaml).context("daemon configuration is not UTF-8")?;
    agentdesktop_core::config::parse_daemon(text).context("validate daemon configuration")?;
    let sha256 = Sha256::digest(&yaml).to_vec();
    Ok(DaemonConfig {
        revision,
        yaml,
        sha256,
    })
}

fn directory_structure_changed(event: &Event, parent: &Path) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) && event.paths.iter().any(|path| path.parent() == Some(parent))
}

fn normalize_watch_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory")?
            .join(path)
    };
    let parent = absolute
        .parent()
        .context("daemon configuration path has no parent")?;
    let file_name = absolute
        .file_name()
        .context("daemon configuration path has no file name")?;
    let parent = std::fs::canonicalize(parent).with_context(|| {
        format!(
            "resolve daemon configuration directory {}",
            parent.display()
        )
    })?;
    Ok(parent.join(file_name))
}

fn resolve_target(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn event_triggers_reload(
    event: &Event,
    path: &Path,
    previous_target: Option<&Path>,
    current_target: Option<&Path>,
) -> bool {
    if !matches!(
        event.kind,
        EventKind::Modify(_)
            | EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write
            ))
    ) {
        return false;
    }
    event.paths.iter().any(|event_path| {
        event_path == path
            || previous_target.is_some_and(|target| event_path == target)
            || current_target.is_some_and(|target| event_path == target)
    })
}

#[cfg(test)]
mod tests {
    use tokio::time::{Instant, sleep, timeout};

    use super::*;

    #[tokio::test]
    async fn watches_atomic_replacements_and_retains_last_good_config() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-config-watch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("daemon.yaml");
        std::fs::write(&path, "programs:\n  claudeCode: {}\n").expect("write initial config");
        let initial = load(&path, 7).expect("load initial config");
        let initial_hash = initial.sha256.clone();
        let store = DaemonConfigStore::new(Some(initial));
        watch(path.clone(), 7, store.clone()).expect("start watcher");
        sleep(Duration::from_millis(50)).await;

        let invalid = directory.join("invalid.tmp");
        std::fs::write(&invalid, "programs: [").expect("write invalid replacement");
        std::fs::rename(&invalid, &path).expect("replace with invalid config");
        let deadline = Instant::now() + Duration::from_secs(3);
        while store.reload_error().is_none() && Instant::now() < deadline {
            sleep(Duration::from_millis(25)).await;
        }
        assert!(store.reload_error().is_some());
        assert_eq!(
            store.current().expect("last good config").sha256,
            initial_hash
        );

        let mut updates = store.subscribe();
        let valid = directory.join("valid.tmp");
        std::fs::write(
            &valid,
            "programs:\n  claudeCode:\n    permissions:\n      defaultMode: plan\n",
        )
        .expect("write valid replacement");
        std::fs::rename(&valid, &path).expect("replace with valid config");
        timeout(Duration::from_secs(3), updates.changed())
            .await
            .expect("watch update timeout")
            .expect("watch channel remains open");
        assert_ne!(
            store.current().expect("updated config").sha256,
            initial_hash
        );
        assert!(store.reload_error().is_none());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }
}
