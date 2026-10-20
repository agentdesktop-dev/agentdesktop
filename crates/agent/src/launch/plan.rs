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
}
