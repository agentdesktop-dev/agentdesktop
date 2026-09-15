//! Filesystem watching for the files an inventory was built from.
//!
//! Interval refreshes bound how stale the inventory can get; this bounds how
//! long a change takes to show up. A developer adding an MCP server should not
//! wait out the interval before the fleet view reflects it.

use std::{
    collections::BTreeMap,
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
    watched: BTreeMap<PathBuf, RecursiveMode>,
}

impl InventoryWatch {
    /// Builds a watcher and the channel it signals on.
    ///
    /// The channel deliberately has capacity one, and the watcher owns that
    /// choice rather than its caller. A scan walks user home directories and
    /// answers every change that arrived before it started, so activity while a
    /// signal is already pending is dropped rather than queued: otherwise a
    /// broad watched directory could enqueue batches faster than they are
    /// consumed and run one full scan per stale token.
    ///
    /// Events are not inspected beyond debouncing. A scan is the only way to
    /// know whether the inventory actually changed, and the caller already
    /// discards a scan that matches the published snapshot.
    pub(crate) fn new() -> anyhow::Result<(Self, mpsc::Receiver<()>)> {
        let (changes, receiver) = mpsc::channel(1);
        let debouncer = notify_debouncer_full::new_debouncer(
            DEBOUNCE,
            None,
            move |result: DebounceEventResult| match result {
                Ok(_) => {
                    let _ = changes.try_send(());
                }
                Err(errors) => warn!(?errors, "inventory watch error"),
            },
        )?;
        Ok((
            Self {
                debouncer,
                watched: BTreeMap::new(),
            },
            receiver,
        ))
    }

    /// Points the watch at the directories behind `discovery`.
    ///
    /// Called again after every published snapshot, so a tool that gains or
    /// loses configuration files is followed without restarting the daemon.
    pub(crate) fn sync(&mut self, discovery: &Discovery) {
        let wanted = source_directories(discovery);
        let stale: Vec<_> = self
            .watched
            .iter()
            .filter(|(path, mode)| wanted.get(*path) != Some(mode))
            .map(|(path, _)| path.clone())
            .collect();
        let added: Vec<_> = wanted
            .iter()
            .filter(|(path, mode)| self.watched.get(*path) != Some(mode))
            .map(|(path, mode)| (path.clone(), *mode))
            .collect();

        for path in stale {
            let _ = self.debouncer.unwatch(&path);
            self.watched.remove(&path);
        }
        for (path, mode) in added {
            match self.debouncer.watch(&path, mode) {
                Ok(()) => {
                    debug!(path = %path.display(), ?mode, "watching inventory source directory");
                    self.watched.insert(path, mode);
                }
                Err(error) => {
                    // A directory can disappear between the scan and this call,
                    // and a daemon does not necessarily have access to every
                    // user's home. Neither is fatal; the interval still runs.
                    debug!(path = %path.display(), %error, "cannot watch inventory source directory");
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn watched(&self) -> &BTreeMap<PathBuf, RecursiveMode> {
        &self.watched
    }
}

/// Directories whose contents produced the inventory, and how deeply to watch.
///
/// Parent directories rather than the files themselves, so that an editor
/// replacing a file atomically, and a sibling file appearing next to one
/// already discovered, both register. Executables are deliberately not
/// watched: an upgrade changes a version rather than a configuration, and
/// install directories are noisy enough to make every scan a wasted one.
fn source_directories(discovery: &Discovery) -> BTreeMap<PathBuf, RecursiveMode> {
    let mut directories = BTreeMap::new();
    for agent in &discovery.agents {
        for server in &agent.mcp_servers {
            if let Some(parent) = existing_directory(server.source.parent()) {
                insert(&mut directories, parent, RecursiveMode::NonRecursive);
            }
        }
        for skill in &agent.skills {
            // A skill walk recurses, so the discovered file can sit well below
            // the root it was found from; watching only its immediate parent
            // would miss a skill added anywhere else in that tree.
            match skill_root(&skill.path) {
                Some(root) => insert(&mut directories, root, RecursiveMode::Recursive),
                None => {
                    if let Some(parent) = existing_directory(skill.path.parent()) {
                        insert(&mut directories, parent, RecursiveMode::NonRecursive);
                    }
                }
            }
        }
    }
    directories
}

/// Root a recursive skill walk started from, when it can be identified.
///
/// Every root the providers scan ends in a `skills` component — `.claude/skills`,
/// `.codex/skills`, `.agents/skills`, `.cursor/skills`, `.github/skills` — so the
/// nearest such ancestor is the tree a new sibling skill would appear in. When a
/// path does not match that shape the caller falls back to the file's own
/// directory, which is what this module did before and is never worse.
fn skill_root(file: &Path) -> Option<PathBuf> {
    let mut directory = file.parent();
    while let Some(candidate) = directory {
        if candidate.file_name().is_some_and(|name| name == "skills") && candidate.is_dir() {
            return Some(candidate.to_path_buf());
        }
        directory = candidate.parent();
    }
    None
}

fn existing_directory(directory: Option<&Path>) -> Option<PathBuf> {
    directory
        .filter(|directory| directory.is_dir())
        .map(Path::to_path_buf)
}

/// Keeps the deeper watch when a directory is wanted both ways.
fn insert(directories: &mut BTreeMap<PathBuf, RecursiveMode>, path: PathBuf, mode: RecursiveMode) {
    let entry = directories.entry(path).or_insert(mode);
    if mode == RecursiveMode::Recursive {
        *entry = RecursiveMode::Recursive;
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

    fn skill(path: &Path) -> Skill {
        Skill {
            path: path.to_path_buf(),
            front_matter: Default::default(),
        }
    }

    #[test]
    fn watches_the_directory_holding_a_discovered_mcp_configuration() {
        let root = temp_root("mcp");
        let config = root.join("mcp.json");
        fs::write(&config, "{}").unwrap();

        let directories = source_directories(&inventory(vec![server(&config)], Vec::new()));

        assert_eq!(directories.get(&root), Some(&RecursiveMode::NonRecursive));
        // The executable's directory is deliberately not watched.
        assert!(!directories.contains_key(&PathBuf::from("/usr/local/bin")));
        let _ = fs::remove_dir_all(&root);
    }

    /// A skill walk recurses, so a discovered `SKILL.md` can sit several levels
    /// below the root it came from. Watching only its own directory would miss
    /// a skill added anywhere else in that tree, which is the ordinary case.
    #[test]
    fn watches_the_whole_skill_tree_rather_than_one_skill_directory() {
        let root = temp_root("skills");
        let skills = root.join(".codex/skills");
        let nested = skills.join(".system/review-agent");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("SKILL.md");
        fs::write(&file, "---\nname: review-agent\n---\n").unwrap();

        let directories = source_directories(&inventory(Vec::new(), vec![skill(&file)]));

        assert_eq!(directories.get(&skills), Some(&RecursiveMode::Recursive));
        assert!(
            !directories.contains_key(&nested),
            "the tree is watched recursively, so the leaf needs no watch of its own"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_the_skill_directory_when_there_is_no_skills_root() {
        let root = temp_root("rootless");
        let file = root.join("SKILL.md");
        fs::write(&file, "---\nname: loose\n---\n").unwrap();

        let directories = source_directories(&inventory(Vec::new(), vec![skill(&file)]));

        assert_eq!(directories.get(&root), Some(&RecursiveMode::NonRecursive));
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

        let (mut watch, _changes) = InventoryWatch::new().expect("create watcher");

        watch.sync(&inventory(
            vec![server(&first.join("mcp.json"))],
            Vec::new(),
        ));
        assert_eq!(watch.watched().keys().collect::<Vec<_>>(), vec![&first]);

        watch.sync(&inventory(
            vec![server(&second.join("mcp.json"))],
            Vec::new(),
        ));
        assert_eq!(
            watch.watched().keys().collect::<Vec<_>>(),
            vec![&second],
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

        let (mut watch, mut changes) = InventoryWatch::new().expect("create watcher");
        watch.sync(&inventory(vec![server(&config)], Vec::new()));

        fs::write(&config, r#"{"mcpServers":{}}"#).unwrap();

        let change = tokio::time::timeout(Duration::from_secs(20), changes.recv()).await;
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            change.expect("a write to a watched directory must be reported"),
            Some(())
        );
    }

    /// Bursts must not queue a scan each. The watcher owns its channel so this
    /// cannot be undone by a caller passing a deeper one.
    #[tokio::test]
    async fn coalesces_activity_into_a_single_pending_scan() {
        let root = temp_root("burst");
        let config = root.join("mcp.json");
        fs::write(&config, "{}").unwrap();

        let (mut watch, mut changes) = InventoryWatch::new().expect("create watcher");
        watch.sync(&inventory(vec![server(&config)], Vec::new()));

        for index in 0..25 {
            fs::write(root.join(format!("noise-{index}.json")), "{}").unwrap();
        }

        let first = tokio::time::timeout(Duration::from_secs(20), changes.recv()).await;
        assert_eq!(first.expect("the burst must be reported"), Some(()));
        // Whatever the burst produced, at most one signal was ever held.
        let mut pending = 0;
        while changes.try_recv().is_ok() {
            pending += 1;
        }
        let _ = fs::remove_dir_all(&root);
        assert!(
            pending <= 1,
            "a burst queued {pending} extra scans; capacity-one coalescing is not working"
        );
    }
}
