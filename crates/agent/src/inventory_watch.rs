//! Filesystem watching for the files an inventory was built from.
//!
//! Interval refreshes bound how stale the inventory can get; this bounds how
//! long a change takes to show up. A developer adding an MCP server should not
//! wait out the interval before the fleet view reflects it.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use agentdesktop_core::model::Discovery;
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Matches the window the controller uses for its own configuration watch.
///
/// Editors frequently write a settings file as a burst of rename and write
/// events, and a scan walks user home directories, so coalescing is worth more
/// than reacting to the first event.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Watches the directories holding the files the current inventory came from.
pub(crate) struct InventoryWatch {
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    watched: BTreeSet<PathBuf>,
}

impl InventoryWatch {
    /// Sends `()` on `changes` whenever a watched directory reports activity.
    ///
    /// Events are not inspected beyond debouncing: a scan is the only way to
    /// know whether the inventory actually changed, and the caller already
    /// discards a scan that matches the published snapshot.
    pub(crate) fn new(changes: mpsc::UnboundedSender<()>) -> anyhow::Result<Self> {
        let debouncer = notify_debouncer_full::new_debouncer(
            DEBOUNCE,
            None,
            move |result: DebounceEventResult| match result {
                Ok(_) => {
                    let _ = changes.send(());
                }
                Err(errors) => warn!(?errors, "inventory watch error"),
            },
        )?;
        Ok(Self {
            debouncer,
            watched: BTreeSet::new(),
        })
    }

    /// Points the watch at the directories behind `discovery`.
    ///
    /// Called again after every published snapshot, so a tool that gains or
    /// loses configuration files is followed without restarting the daemon.
    pub(crate) fn sync(&mut self, discovery: &Discovery) {
        let wanted = source_directories(discovery);
        let stale: Vec<_> = self.watched.difference(&wanted).cloned().collect();
        let added: Vec<_> = wanted.difference(&self.watched).cloned().collect();

        for path in stale {
            let _ = self.debouncer.unwatch(&path);
        }
        let mut watched = self.watched.clone();
        for path in added {
            match self.debouncer.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    debug!(path = %path.display(), "watching inventory source directory");
                }
                Err(error) => {
                    // A directory can disappear between the scan and this call,
                    // and a daemon does not necessarily have access to every
                    // user's home. Neither is fatal; the interval still runs.
                    debug!(path = %path.display(), %error, "cannot watch inventory source directory");
                    watched.remove(&path);
                    continue;
                }
            }
            watched.insert(path);
        }
        watched.retain(|path| wanted.contains(path));
        self.watched = watched;
    }

    #[cfg(test)]
    pub(crate) fn watched(&self) -> &BTreeSet<PathBuf> {
        &self.watched
    }
}

/// Directories whose contents produced the inventory.
///
/// Parent directories rather than the files themselves, so that an editor
/// replacing a file atomically, and a sibling file appearing next to one
/// already discovered, both register. Executables are deliberately not
/// watched: an upgrade changes a version rather than a configuration, and
/// install directories are noisy enough to make every scan a wasted one.
fn source_directories(discovery: &Discovery) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for agent in &discovery.agents {
        for server in &agent.mcp_servers {
            insert_parent(&mut directories, &server.source);
        }
        for skill in &agent.skills {
            insert_parent(&mut directories, &skill.path);
        }
    }
    directories
}

fn insert_parent(directories: &mut BTreeSet<PathBuf>, file: &Path) {
    if let Some(parent) = file.parent().filter(|parent| parent.is_dir()) {
        directories.insert(parent.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use agentdesktop_core::model::{Agent, McpServer, Skill};

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-watch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn inventory(servers: Vec<McpServer>, skills: Vec<Skill>) -> Discovery {
        Discovery {
            agents: vec![Agent {
                kind: "cursor".to_owned(),
                executable: PathBuf::from("/usr/local/bin/cursor"),
                version: None,
                mcp_servers: servers,
                skills,
            }],
            model_runtimes: Vec::new(),
        }
    }

    fn server(source: &Path) -> McpServer {
        McpServer {
            name: "docs".to_owned(),
            transport: "http".to_owned(),
            command: None,
            url: Some("https://example.test/mcp".to_owned()),
            enabled: true,
            source: source.to_path_buf(),
        }
    }

    #[test]
    fn collects_the_directories_holding_discovered_sources() {
        let root = temp_root("sources");
        fs::create_dir_all(root.join("skill")).unwrap();
        let config = root.join("mcp.json");
        let skill = root.join("skill/SKILL.md");
        fs::write(&config, "{}").unwrap();
        fs::write(&skill, "---\nname: s\n---\n").unwrap();

        let directories = source_directories(&inventory(
            vec![server(&config)],
            vec![Skill {
                path: skill.clone(),
                front_matter: Default::default(),
            }],
        ));

        assert!(directories.contains(&root));
        assert!(directories.contains(&root.join("skill")));
        // The executable's directory is deliberately not watched.
        assert!(!directories.contains(&PathBuf::from("/usr/local/bin")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_sources_whose_directory_is_gone() {
        let directories = source_directories(&inventory(
            vec![server(Path::new("/nonexistent-agentdesktop/mcp.json"))],
            Vec::new(),
        ));
        assert!(directories.is_empty());
    }

    #[test]
    fn follows_the_inventory_when_sources_appear_and_disappear() {
        let root = temp_root("resync");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("mcp.json"), "{}").unwrap();
        fs::write(second.join("mcp.json"), "{}").unwrap();

        let (changes, _receiver) = mpsc::unbounded_channel();
        let mut watch = InventoryWatch::new(changes).expect("create watcher");

        watch.sync(&inventory(
            vec![server(&first.join("mcp.json"))],
            Vec::new(),
        ));
        assert_eq!(watch.watched(), &BTreeSet::from([first.clone()]));

        watch.sync(&inventory(
            vec![server(&second.join("mcp.json"))],
            Vec::new(),
        ));
        assert_eq!(
            watch.watched(),
            &BTreeSet::from([second.clone()]),
            "a source that left the inventory must stop being watched"
        );

        watch.sync(&inventory(Vec::new(), Vec::new()));
        assert!(watch.watched().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// The timeout is generous because macOS delivers through FSEvents, which
    /// batches with its own latency on top of the debounce window.
    #[tokio::test]
    async fn reports_a_write_to_a_watched_directory() {
        let root = temp_root("events");
        let config = root.join("mcp.json");
        fs::write(&config, "{}").unwrap();

        let (changes, mut receiver) = mpsc::unbounded_channel();
        let mut watch = InventoryWatch::new(changes).expect("create watcher");
        watch.sync(&inventory(vec![server(&config)], Vec::new()));

        fs::write(&config, r#"{"mcpServers":{}}"#).unwrap();

        let change = tokio::time::timeout(Duration::from_secs(20), receiver.recv()).await;
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            change.expect("a write to a watched directory must be reported"),
            Some(())
        );
    }
}
