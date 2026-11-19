//! Wrap-mode launches and their posture: the resolver for the
//! admin-configured managed executable, the Windows-only violation scanner
//! with its in-memory ledger, and the per-program posture daemon-info
//! reports.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use agentdesktop_core::{
    config::{ContainmentMode, ContainmentPolicy, DaemonConfig},
    model::ProgramPosture,
};

use super::ProgramFs;
use super::profile::{self, ProgramProfile};

/// The last `\`-separated segment of `path`, string-level: the daemon
/// validates Windows paths on every platform (the controller runs on Linux,
/// where `\` is not a path separator).
fn win_file_name(path: &Path) -> String {
    let text = super::win(path);
    text.rsplit('\\').next().unwrap_or_default().to_owned()
}

/// Resolves `containment.programs.<p>.executable` into the executable
/// launched in wrap mode. Unlike native resolution (`resolve_program_for`),
/// `~\.local\bin` and the daemon `PATH` are NOT searched: only `configured`
/// is considered. `configured` is canonicalized, must be a file, must NOT lie
/// under `home` (the vendor install is admin-only, never user-writable), and
/// its file name must equal `profile.exe_name`. A missing or disqualified
/// executable is refused naming the `containment.programs.<p>.executable`
/// field (the caller turns this into a 409).
pub(crate) fn resolve_managed_program(
    profile: &ProgramProfile,
    configured: &Path,
    home: &Path,
    fs: &dyn ProgramFs,
) -> Result<PathBuf, String> {
    let field = format!(
        "containment.programs.{}.executable",
        profile.containment_field
    );
    if !fs.is_file(configured) {
        return Err(format!(
            "{field} ({}) is not a file on this device",
            configured.display()
        ));
    }
    let canonical = fs
        .canonicalize(configured)
        .map(super::strip_verbatim)
        .map_err(|error| format!("{field} ({}): {error}", configured.display()))?;
    let name = win_file_name(&canonical);
    if !name.eq_ignore_ascii_case(profile.exe_name) {
        return Err(format!(
            "{field} must name the program's executable {} (found {name})",
            profile.exe_name
        ));
    }
    // Both sides canonical where the filesystem can say, like the native
    // resolver's root check.
    let mut homes = vec![home.to_owned()];
    if let Ok(canonical_home) = fs.canonicalize(home) {
        homes.push(super::strip_verbatim(canonical_home));
    }
    if homes.iter().any(|root| super::is_under(&canonical, root)) {
        return Err(format!(
            "{field} ({}) must not be under the user's home directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// One fact about a running process, read host-side (Windows only in
/// production; a plain data struct here so the detector is pure and
/// Linux-testable).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) struct ProcessFact {
    pub pid: u32,
    /// Process start time, any monotonically-stable identity value the host
    /// provides (Windows: `GetProcessTimes` creation time). Combined with
    /// `pid` it identifies one process lifetime for the counting rule.
    pub start_time: u64,
    pub image: PathBuf,
    pub app_container: bool,
    /// `None` when the peer's token SID could not be read; such a process
    /// never counts as a violation (fail closed on an unreadable identity).
    pub user_sid: Option<String>,
}

/// One wrap-mode program the scanner watches, with the daemon's own user
/// SID (every entry of one scan tick carries the same SID; carried per
/// entry so the pure detector stays a two-argument function over `snapshot`
/// and `wrap` alone).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) struct WrapEntry {
    pub program: &'static str,
    pub executable: PathBuf,
    pub daemon_user_sid: String,
}

/// A detected native run of a wrap-mode program's managed executable.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) struct Violation {
    pub program: &'static str,
    pub pid: u32,
    pub start_time: u64,
}

/// The form two Windows paths are compared in: `\\?\` stripped, `/` folded
/// into `\`, lower-cased (NTFS is case-insensitive by default).
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
fn comparable_image(path: &Path) -> String {
    super::win(&super::strip_verbatim(path.to_owned())).to_lowercase()
}

/// Pure per-tick detection, run every `VIOLATION_SCAN_INTERVAL`. A
/// process is a violation when (1) its full image path, canonical and
/// case-insensitive, equals a wrap entry's `executable`; (2) its token is NOT
/// an AppContainer (boxed runs are not violations); (3) it runs as the
/// daemon's user. Does not dedup across ticks: see [`ViolationLedger`] for
/// the counting semantics (distinct `(pid, start_time)` since daemon
/// start; a run shorter than one scan interval is never sampled).
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) fn violations(snapshot: &[ProcessFact], wrap: &[WrapEntry]) -> Vec<Violation> {
    let mut found = Vec::new();
    for fact in snapshot {
        if fact.app_container {
            continue;
        }
        let Some(user_sid) = fact.user_sid.as_deref() else {
            continue;
        };
        let image = comparable_image(&fact.image);
        for entry in wrap {
            if user_sid == entry.daemon_user_sid && image == comparable_image(&entry.executable) {
                found.push(Violation {
                    program: entry.program,
                    pid: fact.pid,
                    start_time: fact.start_time,
                });
            }
        }
    }
    found
}

/// Cadence of the Windows-only, user-mode-daemon-only scan, independent of
/// `inventoryInterval`.
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) const VIOLATION_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// In-memory count of distinct `(pid, start_time)` native violations seen
/// since daemon start, per program; resets on daemon restart (documented).
#[derive(Debug, Default)]
pub(crate) struct ViolationLedger {
    programs: BTreeMap<String, ProgramViolations>,
}

#[derive(Debug, Default)]
struct ProgramViolations {
    seen: BTreeSet<(u32, u64)>,
    last_unix_seconds: Option<u64>,
}

impl ViolationLedger {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Folds one tick's detections into the ledger, deduping by
    /// `(pid, start_time)` per program so the same live process is never
    /// counted twice across ticks. A deduped sighting still refreshes
    /// `last_violation_unix_seconds`: the process is still running natively.
    #[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
    pub(crate) fn record(&mut self, violations: &[Violation], now_unix_seconds: u64) {
        for violation in violations {
            let program = self
                .programs
                .entry(violation.program.to_owned())
                .or_default();
            program.seen.insert((violation.pid, violation.start_time));
            program.last_unix_seconds = Some(now_unix_seconds);
        }
    }

    /// Distinct violations counted for `program` since daemon start.
    pub(crate) fn count(&self, program: &str) -> u64 {
        self.programs
            .get(program)
            .map_or(0, |program| program.seen.len() as u64)
    }

    /// Unix time of the most recent violation recorded for `program`.
    pub(crate) fn last_violation_unix_seconds(&self, program: &str) -> Option<u64> {
        self.programs
            .get(program)
            .and_then(|program| program.last_unix_seconds)
    }
}

/// The wrap-mode entries of the applied configuration's `containment` block,
/// each carrying the daemon's own user SID for the detector's rule (3).
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) fn wrap_entries(
    policy: Option<&ContainmentPolicy>,
    daemon_user_sid: &str,
) -> Vec<WrapEntry> {
    let Some(policy) = policy else {
        return Vec::new();
    };
    wrap_programs()
        .into_iter()
        .filter_map(|profile| {
            let entry = (profile.containment_policy)(&policy.programs)?;
            if entry.mode != ContainmentMode::Wrap {
                return None;
            }
            Some(WrapEntry {
                program: profile.program,
                executable: canonical_executable(entry.executable.clone()?),
                daemon_user_sid: daemon_user_sid.to_owned(),
            })
        })
        .collect()
}

/// The scan compares canonical with canonical: the launch-side resolver
/// canonicalizes the configured executable, and the host reports the
/// kernel's full image path, so a policy spelled with an 8.3 short name, a
/// `.`/`..` segment or a doubled separator must still match. Canonicalized
/// once where the scan's wrap entries are built, not per process. Best
/// effort: when the path does not resolve on this device the configured
/// spelling is kept (no process can be running from a path that does not
/// exist, so nothing is undercounted).
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
fn canonical_executable(configured: PathBuf) -> PathBuf {
    std::fs::canonicalize(&configured)
        .map(super::strip_verbatim)
        .unwrap_or(configured)
}

/// The profiled programs a `containment` policy can name, in posture order.
fn wrap_programs() -> [&'static ProgramProfile; 3] {
    [&profile::CLAUDE_CODE, &profile::OPENCODE, &profile::COPILOT]
}

/// The scan loop of a Windows user-mode daemon whose
/// `daemon.containment` is present — report-only, every
/// [`VIOLATION_SCAN_INTERVAL`]. Each tick reads the applied configuration
/// through `api::load_effective_config` (one small file read), takes one
/// process snapshot and folds the detections into the ledger. `snapshot` and
/// `now_unix_seconds` are parameters so tests can inject both.
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) async fn scan_loop(
    config: DaemonConfig,
    state_dir: PathBuf,
    daemon_user_sid: String,
    ledger: Arc<Mutex<ViolationLedger>>,
    snapshot: impl Fn(&[WrapEntry]) -> Result<Vec<ProcessFact>, String> + Send + 'static,
    now_unix_seconds: impl Fn() -> u64 + Send + 'static,
) {
    let mut interval = tokio::time::interval(VIOLATION_SCAN_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let effective = match crate::api::load_effective_config(&config, &state_dir) {
            Ok(effective) => effective,
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "violation scan could not read the applied configuration; tick skipped"
                );
                continue;
            }
        };
        let entries = wrap_entries(effective.containment.as_ref(), &daemon_user_sid);
        if entries.is_empty() {
            continue;
        }
        let facts = match snapshot(&entries) {
            Ok(facts) => facts,
            Err(error) => {
                tracing::warn!(%error, "violation scan tick skipped");
                continue;
            }
        };
        let found = violations(&facts, &entries);
        if found.is_empty() {
            continue;
        }
        let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.record(&found, now_unix_seconds());
    }
}

/// The production snapshot source: facts about native processes whose image
/// name matches a wrap entry's. Windows-only; elsewhere a typed error (the
/// daemon never spawns the scan off Windows).
#[cfg(windows)]
pub(crate) fn native_process_snapshot(wrap: &[WrapEntry]) -> Result<Vec<ProcessFact>, String> {
    super::host::windows::wrap_process_snapshot(wrap)
}

/// The production snapshot source, off Windows: the violation scan reads
/// Windows process tokens and is Windows-only.
#[cfg(not(windows))]
#[cfg_attr(not(windows), allow(dead_code))] // scan consumer: the Windows daemon (and tests)
pub(crate) fn native_process_snapshot(_wrap: &[WrapEntry]) -> Result<Vec<ProcessFact>, String> {
    Err("the violation scan is Windows-only".to_owned())
}

/// Per-program posture for `ContainmentInfo.programs`, keyed by launch
/// id, one entry per wrap-mode program the applied `containment` policy
/// names. `violations`/`lastViolationUnixSeconds` come from the device-local
/// ledger; `aliasesPresent` from the launcher payloads and the machine PATH.
pub(crate) fn program_postures(
    policy: &ContainmentPolicy,
    ledger: &ViolationLedger,
) -> BTreeMap<String, ProgramPosture> {
    let mut programs = BTreeMap::new();
    for profile in wrap_programs() {
        let Some(entry) = (profile.containment_policy)(&policy.programs) else {
            continue;
        };
        if entry.mode != ContainmentMode::Wrap {
            continue;
        }
        programs.insert(
            profile.program.to_owned(),
            ProgramPosture {
                mode: entry.mode,
                executable: entry
                    .executable
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                aliases_present: launchers_present(profile.exe_name),
                violations: ledger.count(profile.program),
                last_violation_unix_seconds: ledger.last_violation_unix_seconds(profile.program),
            },
        );
    }
    programs
}

/// `aliasesPresent`: every launcher payload file for the program exists
/// (`<launchers>\<stem>.cmd` and the extensionless `<stem>` under
/// `%ProgramFiles%\Agent Desktop\launchers`) AND the launcher directory
/// appears in the machine PATH value (HKLM Session-Manager `Path`). It does
/// not prove PATH order within a given live session.
// The directory names must match the `Directory` elements in
// `scripts/windows-package/claude-code-managed.wxs` (`Agent Desktop` is the
// product MSI's Tauri productName install directory).
#[cfg(windows)]
fn launchers_present(exe_name: &str) -> bool {
    let Some(program_files) = std::env::var_os("ProgramFiles") else {
        return false;
    };
    let launchers = PathBuf::from(program_files)
        .join("Agent Desktop")
        .join("launchers");
    let stem = exe_name
        .strip_suffix(".exe")
        .or_else(|| exe_name.strip_suffix(".EXE"))
        .unwrap_or(exe_name);
    if !(launchers.join(format!("{stem}.cmd")).is_file() && launchers.join(stem).is_file()) {
        return false;
    }
    let Some(machine_path) = machine_path_value() else {
        return false;
    };
    let wanted = super::win(&launchers).to_lowercase();
    let wanted = wanted.trim_end_matches('\\');
    machine_path.split(';').any(|entry| {
        let entry = entry.trim().trim_matches('"').to_lowercase();
        entry.trim_end_matches('\\') == wanted
    })
}

/// The machine `Path` value (HKLM Session-Manager `Environment`), unexpanded.
#[cfg(windows)]
fn machine_path_value() -> Option<String> {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    super::host::windows::policy_values(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    )
    .into_iter()
    .find(|(name, _)| name.eq_ignore_ascii_case("Path"))
    .and_then(|(_, value)| value.as_str().map(str::to_owned))
}

/// Off Windows no launcher package exists; posture is absent there anyway
/// (the ledger is `Some` only in a Windows user-mode daemon).
#[cfg(not(windows))]
fn launchers_present(_exe_name: &str) -> bool {
    false
}
