//! Launch plan: the resolved launch inputs before engine-specific envelope.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

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
    /// How the plan's paths and command line are written.
    pub style: PathStyle,
    /// The relay on the host, when the engine must name it as the box's only
    /// way out (a box that does not share host loopback).
    pub host_proxy: Option<SocketAddr>,
}

/// How a launch writes paths, search paths and its command line. Profiles
/// keep one spelling of every relative path (`\`-joined); the style renders
/// it for the host the box runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathStyle {
    /// `\` separators, `;` search path, `cmd.exe` command line.
    #[default]
    Windows,
    /// `/` separators, `:` search path, `sh` command line.
    Posix,
}

impl PathStyle {
    /// The style of the host this binary runs on.
    pub const fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    pub const fn sep(self) -> char {
        match self {
            Self::Windows => '\\',
            Self::Posix => '/',
        }
    }

    /// A path as the request carries it.
    pub fn render(self, path: &Path) -> String {
        match self {
            Self::Windows => path.to_string_lossy().replace('/', "\\"),
            Self::Posix => path.to_string_lossy().into_owned(),
        }
    }

    /// `base` joined with a profile-relative path (`\`-joined segments).
    pub fn join(self, base: &str, relative: &str) -> String {
        let sep = self.sep();
        relative.split('\\').fold(base.to_owned(), |path, segment| {
            format!("{}{sep}{segment}", path.trim_end_matches(sep))
        })
    }
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
