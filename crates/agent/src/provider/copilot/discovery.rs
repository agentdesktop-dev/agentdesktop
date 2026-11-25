//! GitHub Copilot CLI discovery: the `copilot` executable on `PATH` (or in the
//! usual npm global locations) and its version. Nothing is executed: discovery
//! also runs in a system daemon, and a file found in a user's home must never
//! run as root. The MCP and skills inventory under `~/.copilot` stays with the
//! VS Code adapter, which already reads it.
//!
//! The installed launcher is not always the build that runs. Copilot CLI
//! downloads newer builds into a per-user package cache, and its launcher
//! starts the newest cached build unless the installed one is newer. The
//! version reported is therefore the higher of the newest cached build and
//! the npm package manifest of `@github/copilot`.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::Agent;

use super::Copilot;
use crate::provider::metadata;

const PACKAGE: &str = "@github/copilot";

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("copilot", executable_candidates())?;
    Some(Agent {
        version: running_version(
            cached_build_version(&package_cache_roots()),
            npm_version(&executable),
        ),
        executable,
        kind: Copilot::ID.to_owned(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".npm-global/bin/copilot"));
        candidates.insert(home.join(".local/bin/copilot"));
        #[cfg(windows)]
        candidates.insert(home.join("AppData/Roaming/npm/copilot.cmd"));
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/copilot"),
        PathBuf::from("/usr/local/bin/copilot"),
    ]);
    candidates.into_iter().collect()
}

/// The build the launcher starts: the newest cached build, unless the
/// installed one is newer (after an upgrade that nobody has run yet). An
/// installed version that cannot be read loses to any cached build.
pub(super) fn running_version(cached: Option<String>, installed: Option<String>) -> Option<String> {
    match (cached, installed) {
        (Some(cached), Some(installed)) => {
            Some(if version_order(&installed) > version_order(&cached) {
                installed
            } else {
                cached
            })
        }
        (cached, installed) => cached.or(installed),
    }
}

/// Directories where Copilot CLI keeps the builds it downloads, for every
/// user home on this device. They mirror the locations its launcher searches:
/// the per-OS cache directory, `~/.cache`, and `~/.copilot`.
fn package_cache_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        #[cfg(target_os = "macos")]
        roots.insert(home.join("Library/Caches/copilot/pkg"));
        #[cfg(windows)]
        roots.insert(home.join("AppData/Local/copilot/pkg"));
        roots.insert(home.join(".cache/copilot/pkg"));
        roots.insert(home.join(".copilot/pkg"));
    }
    roots.into_iter().collect()
}

/// The prefix of the per-platform directory inside a package cache
/// (`darwin-arm64`, `win32-x64`, `linux-x64`, `linuxmusl-x64`).
#[cfg(target_os = "macos")]
pub(super) const CACHE_PLATFORM: &str = "darwin";
#[cfg(windows)]
pub(super) const CACHE_PLATFORM: &str = "win32";
#[cfg(not(any(target_os = "macos", windows)))]
pub(super) const CACHE_PLATFORM: &str = "linux";

/// The newest complete build in the given package caches.
///
/// A cache holds `universal/<version>/` and `<platform>-<arch>/<version>/`
/// directories, and a build counts once its `app.js` exists. With several
/// users on one device this is the newest build any of them has; a user who
/// turned automatic updates off still runs the installed build, which this
/// cannot see.
pub(super) fn cached_build_version(roots: &[PathBuf]) -> Option<String> {
    roots
        .iter()
        .flat_map(|root| subdirectories(root))
        .filter(|(name, _)| name == "universal" || name.starts_with(CACHE_PLATFORM))
        .flat_map(|(_, platform)| subdirectories(&platform))
        .filter(|(_, build)| build.join("app.js").is_file())
        .filter_map(|(name, _)| Some((version_order(&name)?, name)))
        .max()
        .map(|(_, name)| name)
}

fn subdirectories(directory: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect()
}

/// One dot-separated part of a pre-release (`beta.2` has two). Numbers order
/// numerically and below text, as in semantic versioning.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum PreRelease {
    Number(u64),
    Text(String),
}

/// Orders versions as semantic versioning does: `major.minor.patch`
/// numerically, a pre-release (`1.2.3-beta.1`) below the release it leads up
/// to, and build metadata (`+build`) ignored. A name that does not start with
/// three numbers is not a build.
fn version_order(name: &str) -> Option<([u64; 3], bool, Vec<PreRelease>)> {
    let name = name.split_once('+').map_or(name, |(version, _)| version);
    let (core, pre_release) = match name.split_once('-') {
        Some((core, pre_release)) => (core, Some(pre_release)),
        None => (name, None),
    };
    let mut numbers = core.split('.').map(|part| part.parse::<u64>().ok());
    let version = [numbers.next()??, numbers.next()??, numbers.next()??];
    if numbers.next().is_some() {
        return None;
    }
    let parts = pre_release
        .into_iter()
        .flat_map(|pre_release| pre_release.split('.'))
        .map(|part| match part.parse() {
            Ok(number) => PreRelease::Number(number),
            Err(_) => PreRelease::Text(part.to_owned()),
        })
        .collect();
    Some((version, pre_release.is_none(), parts))
}

/// The version from the `@github/copilot` manifest next to the launcher. A
/// standalone binary has no manifest and reports no version.
pub(super) fn npm_version(executable: &Path) -> Option<String> {
    metadata::npm_package_version(executable, PACKAGE)
}
