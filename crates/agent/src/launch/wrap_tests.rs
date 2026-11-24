//! Spec-derived tests for PR 6, slice A: the wrap-mode resolver (D8, AC-7)
//! and the pure violation detector (D3/N8, AC-3). The implementation may not
//! weaken or delete them without a ledger entry.
//! See `specs/PR-6-packaging-wrap.md` and `specs/PR-6-test-changes.md`.

use std::path::{Path, PathBuf};

use super::ProgramFs;
use super::profile::{CLAUDE_CODE, COPILOT, OPENCODE};
use super::wrap::{ProcessFact, Violation, ViolationLedger, WrapEntry, resolve_managed_program};

struct FakeFs(Vec<PathBuf>);

impl ProgramFs for FakeFs {
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        Ok(path.to_owned())
    }

    fn is_file(&self, path: &Path) -> bool {
        self.0.contains(&path.to_owned())
    }
}

// --- AC-7: resolve_managed_program -----------------------------------------

#[test]
fn a_configured_executable_resolves_when_it_is_a_file_not_under_home_with_the_right_name() {
    let configured = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe");
    let fs = FakeFs(vec![configured.clone()]);
    let home = PathBuf::from(r"C:\Users\dev");
    assert_eq!(
        resolve_managed_program(&CLAUDE_CODE, &configured, &home, &fs),
        Ok(configured)
    );
}

#[test]
fn local_bin_and_path_copies_are_ignored_the_configured_path_is_used() {
    let configured = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe");
    let home = PathBuf::from(r"C:\Users\dev");
    let local_bin_decoy = home.join(r".local\bin\claude.exe");
    let path_decoy = PathBuf::from(r"C:\tools\claude.exe");
    let fs = FakeFs(vec![configured.clone(), local_bin_decoy, path_decoy]);
    assert_eq!(
        resolve_managed_program(&CLAUDE_CODE, &configured, &home, &fs),
        Ok(configured),
        "~\\.local\\bin and PATH are not searched; the configured path is used verbatim"
    );
}

#[test]
fn a_missing_configured_executable_is_refused_naming_the_field() {
    let configured = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe");
    let home = PathBuf::from(r"C:\Users\dev");
    let fs = FakeFs(vec![]);
    let error = resolve_managed_program(&CLAUDE_CODE, &configured, &home, &fs).unwrap_err();
    assert!(
        error.contains("containment.programs.claudeCode.executable"),
        "{error}"
    );
}

#[test]
fn a_configured_executable_under_home_is_refused() {
    let home = PathBuf::from(r"C:\Users\dev");
    let configured = home.join(r".local\bin\claude.exe");
    let fs = FakeFs(vec![configured.clone()]);
    let error = resolve_managed_program(&CLAUDE_CODE, &configured, &home, &fs).unwrap_err();
    assert!(
        error.contains("containment.programs.claudeCode.executable"),
        "{error}"
    );
}

#[test]
fn a_configured_executable_whose_file_name_is_not_the_profiles_exe_name_is_refused() {
    let home = PathBuf::from(r"C:\Users\dev");
    let configured = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\not-claude.exe");
    let fs = FakeFs(vec![configured.clone()]);
    let error = resolve_managed_program(&CLAUDE_CODE, &configured, &home, &fs).unwrap_err();
    assert!(
        error.contains("containment.programs.claudeCode.executable"),
        "{error}"
    );
}

#[test]
fn the_field_name_follows_the_profile_opencode_and_copilot() {
    let home = PathBuf::from(r"C:\Users\dev");
    let fs = FakeFs(vec![]);
    let opencode_error =
        resolve_managed_program(&OPENCODE, Path::new(r"C:\x\opencode.exe"), &home, &fs)
            .unwrap_err();
    assert!(opencode_error.contains("containment.programs.openCode.executable"));
    let copilot_error =
        resolve_managed_program(&COPILOT, Path::new(r"C:\x\copilot.exe"), &home, &fs).unwrap_err();
    assert!(copilot_error.contains("containment.programs.copilot.executable"));
}

// --- AC-3: violations() ------------------------------------------------------

const DAEMON_SID: &str = "S-1-5-21-1-2-3-1001";

fn claude_entry() -> WrapEntry {
    WrapEntry {
        program: "claude-code",
        executable: PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe"),
        daemon_user_sid: DAEMON_SID.to_owned(),
    }
}

fn native_fact() -> ProcessFact {
    ProcessFact {
        pid: 4242,
        start_time: 1,
        image: PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe"),
        app_container: false,
        user_sid: Some(DAEMON_SID.to_owned()),
    }
}

#[test]
fn a_native_same_path_process_counts() {
    let violations = super::wrap::violations(&[native_fact()], &[claude_entry()]);
    assert_eq!(
        violations,
        vec![Violation {
            program: "claude-code",
            pid: 4242,
            start_time: 1,
        }]
    );
}

#[test]
fn a_boxed_same_path_process_does_not_count() {
    let mut fact = native_fact();
    fact.app_container = true;
    assert_eq!(super::wrap::violations(&[fact], &[claude_entry()]), vec![]);
}

#[test]
fn a_different_path_with_the_same_image_name_does_not_count() {
    let mut fact = native_fact();
    fact.image = PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe");
    assert_eq!(super::wrap::violations(&[fact], &[claude_entry()]), vec![]);
}

#[test]
fn a_different_users_process_does_not_count() {
    let mut fact = native_fact();
    fact.user_sid = Some("S-1-5-21-9-9-9-9999".to_owned());
    assert_eq!(super::wrap::violations(&[fact], &[claude_entry()]), vec![]);
}

#[test]
fn an_unreadable_identity_never_counts() {
    let mut fact = native_fact();
    fact.user_sid = None;
    assert_eq!(super::wrap::violations(&[fact], &[claude_entry()]), vec![]);
}

#[test]
fn case_and_verbatim_prefix_differences_still_match() {
    let mut fact = native_fact();
    fact.image = PathBuf::from(r"\\?\C:\PROGRAM FILES\ANTHROPIC\CLAUDECODE\CLAUDE.EXE");
    let violations = super::wrap::violations(&[fact], &[claude_entry()]);
    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn a_process_of_a_different_wrap_program_is_not_matched() {
    let mut fact = native_fact();
    fact.image = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\opencode.exe");
    let entry = WrapEntry {
        program: "opencode",
        executable: PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\opencode.exe"),
        daemon_user_sid: DAEMON_SID.to_owned(),
    };
    // An opencode process does not match a DIFFERENT wrap program's entry
    // (claude-code), only its own.
    assert_eq!(
        super::wrap::violations(std::slice::from_ref(&fact), &[claude_entry()]),
        vec![]
    );
    fact.image = entry.executable.clone();
    assert_eq!(
        super::wrap::violations(&[fact], &[entry]),
        vec![Violation {
            program: "opencode",
            pid: 4242,
            start_time: 1,
        }]
    );
}

/// The scan's wrap entries canonicalize the configured executable, so a
/// policy spelled with `.`/`..` segments or a doubled separator (the same
/// class as a Windows 8.3 short name or a forward-slash spelling, which only
/// a Windows filesystem can produce) still matches the kernel-reported
/// canonical image path.
#[test]
fn a_non_canonical_configured_spelling_still_matches_the_canonical_image() {
    use agentdesktop_core::config::{
        ContainmentPolicy, ContainmentProgramPolicy, ContainmentProgramsConfig,
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("claude.exe"), b"sentinel").unwrap();
    let configured = dir
        .path()
        .join("sub")
        .join("..")
        .join(".")
        .join("claude.exe");
    let policy = ContainmentPolicy {
        programs: ContainmentProgramsConfig {
            claude_code: Some(ContainmentProgramPolicy {
                mode: agentdesktop_core::config::ContainmentMode::Wrap,
                executable: Some(configured.clone()),
            }),
            open_code: None,
            copilot: None,
        },
    };

    let entries = super::wrap::wrap_entries(Some(&policy), DAEMON_SID);

    let canonical = std::fs::canonicalize(dir.path().join("claude.exe")).unwrap();
    assert_ne!(
        configured, canonical,
        "the configured spelling is non-canonical"
    );
    let mut fact = native_fact();
    fact.image = canonical;
    let violations = super::wrap::violations(&[fact], &entries);
    assert_eq!(violations.len(), 1, "{violations:?} from {entries:?}");
}

// --- N8: ViolationLedger counting semantics ---------------------------------

#[test]
fn the_ledger_dedups_the_same_pid_and_start_time_seen_in_two_ticks() {
    let mut ledger = ViolationLedger::new();
    let violation = Violation {
        program: "claude-code",
        pid: 4242,
        start_time: 1,
    };
    ledger.record(std::slice::from_ref(&violation), 1_700_000_000);
    ledger.record(&[violation], 1_700_000_030);
    assert_eq!(ledger.count("claude-code"), 1);
    assert_eq!(
        ledger.last_violation_unix_seconds("claude-code"),
        Some(1_700_000_030)
    );
}

#[test]
fn the_ledger_counts_distinct_pids_separately() {
    let mut ledger = ViolationLedger::new();
    ledger.record(
        &[Violation {
            program: "claude-code",
            pid: 1,
            start_time: 1,
        }],
        1_700_000_000,
    );
    ledger.record(
        &[Violation {
            program: "claude-code",
            pid: 2,
            start_time: 1,
        }],
        1_700_000_031,
    );
    assert_eq!(ledger.count("claude-code"), 2);
}

#[test]
fn the_ledger_resets_on_a_fresh_instance() {
    let ledger = ViolationLedger::new();
    assert_eq!(ledger.count("claude-code"), 0);
    assert_eq!(ledger.last_violation_unix_seconds("claude-code"), None);
}

// --- AC-3: the scan loop (writer wiring; injected snapshot and clock) --------

/// AC-3: the scan loop ticks every `VIOLATION_SCAN_INTERVAL` (30 s), reads
/// the applied configuration per tick, and folds detections into the ledger
/// with across-tick dedup. Snapshot and clock are injected; tokio's paused
/// test clock drives the cadence.
#[tokio::test(start_paused = true)]
async fn the_scan_loop_ticks_every_thirty_seconds_and_folds_into_the_ledger() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    let config = agentdesktop_core::config::parse_daemon(
        "containment:\n  programs:\n    claudeCode:\n      executable: 'C:\\Program Files\\Anthropic\\ClaudeCode\\claude.exe'\n",
    )
    .unwrap();
    let ledger = Arc::new(Mutex::new(ViolationLedger::new()));
    let ticks = Arc::new(AtomicU64::new(0));
    let now = Arc::new(AtomicU64::new(1_700_000_000));
    let snapshot = {
        let ticks = ticks.clone();
        move |entries: &[WrapEntry]| {
            assert_eq!(
                entries,
                &[claude_entry()],
                "entries come from the applied config"
            );
            ticks.fetch_add(1, Ordering::SeqCst);
            Ok(vec![native_fact()])
        }
    };
    tokio::spawn(super::wrap::scan_loop(
        config,
        std::env::temp_dir(),
        DAEMON_SID.to_owned(),
        ledger.clone(),
        snapshot,
        {
            let now = now.clone();
            move || now.load(Ordering::SeqCst)
        },
    ));

    // The first tick fires on start.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), 1);
    {
        let ledger = ledger.lock().unwrap();
        assert_eq!(ledger.count("claude-code"), 1);
        assert_eq!(
            ledger.last_violation_unix_seconds("claude-code"),
            Some(1_700_000_000)
        );
    }

    // No second tick before the 30 s interval has elapsed...
    now.store(1_700_000_030, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_secs(28)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), 1, "no tick before 30 s");

    // ... and exactly one at it: the same (pid, start_time) is deduped, the
    // last-seen stamp moves to the injected clock's new value.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), 2, "one tick per 30 s");
    {
        let ledger = ledger.lock().unwrap();
        assert_eq!(ledger.count("claude-code"), 1, "deduped across ticks");
        assert_eq!(
            ledger.last_violation_unix_seconds("claude-code"),
            Some(1_700_000_030)
        );
    }

    // The cadence continues.
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), 3);
}

/// D3: off Windows the production snapshot source is a typed error (the
/// daemon never spawns the scan there).
#[cfg(not(windows))]
#[test]
fn the_native_snapshot_source_is_windows_only() {
    let error = super::wrap::native_process_snapshot(&[claude_entry()]).unwrap_err();
    assert!(error.contains("Windows-only"), "{error}");
}
