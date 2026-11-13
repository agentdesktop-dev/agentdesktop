//! Launch plan: the resolved launch inputs before engine-specific envelope.

use std::{collections::BTreeMap, path::PathBuf};

use agentdesktop_core::config::ContainmentEgress;

/// A resolved launch: canonical paths, validated args, and final environment.
pub(crate) struct LaunchPlan {
    /// The canonical executable path.
    pub program: PathBuf,
    /// Already validated and quoted arguments.
    pub args: Vec<String>,
    /// The window title from the profile.
    pub window_title: &'static str,
    /// The working directory.
    pub cwd: PathBuf,
    /// Final environment including relay URL and pairing.
    pub env: BTreeMap<String, String>,
    /// Readwrite paths (existence-filtered).
    pub readwrite: Vec<PathBuf>,
    /// Readonly paths (existence-filtered).
    pub readonly: Vec<PathBuf>,
    /// Denied paths (existence-filtered and ordered).
    pub denied: Vec<PathBuf>,
    /// Egress policy.
    pub egress: ContainmentEgress,
    /// How the boxed program's console is provided.
    pub console: ConsolePlan,
}

/// The console a launch's boxed program runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConsolePlan {
    /// A console window of its own (`start /WAIT`), as before.
    #[default]
    Window,
    /// The requesting client's console; the executor is spawned attached to
    /// it and the program's exit code is propagated.
    #[cfg_attr(not(windows), allow(dead_code))]
    Attach { client_pid: u32 },
}
