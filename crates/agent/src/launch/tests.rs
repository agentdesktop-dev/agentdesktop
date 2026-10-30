//! Spec-derived tests for PR 1 (`specs/PR-1.md`). The implementation may not
//! weaken or delete them without a ledger entry.
//!
//! Tests that need Windows semantics are under `#[cfg(windows)]` and run only
//! in CI's Windows job. Everything else runs on both platforms: Windows-style
//! paths are plain strings in a `PathBuf` where the code under test joins
//! them by string (`build_request`), and real paths are built from
//! `tempfile::TempDir` roots or with the `p()` helper below.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use agentdesktop_core::{
    config::ContainmentStartupConfig,
    model::{ContainmentInfo, Launch, LaunchRequest, LaunchStatus, LlmGatewayCredential},
};
use async_trait::async_trait;
use axum::http::StatusCode;

use super::engine::mxc::build_request;
use super::*;

// --- Log capture helper for session logging test (AC 2)

#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.0.clone())
    }
}

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    async fn wait_for_text(&self, needle: &str) -> String {
        for _ in 0..100 {
            let text = self.text();
            if text.contains(needle) {
                return text;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.text()
    }
}

fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

/// A platform-appropriate absolute root: `/` or `C:\`.
fn root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\")
    } else {
        PathBuf::from("/")
    }
}

/// `p("home/dev/work")` is `/home/dev/work` or `C:\home\dev\work`.
fn p(relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(root(), |path, part| path.join(part))
}

// ===================================================== containment_info (AC 2)

fn executor_cfg(path: &str) -> ContainmentStartupConfig {
    ContainmentStartupConfig {
        executor: PathBuf::from(path),
        egress: agentdesktop_core::config::ContainmentEgress::default(),
        engine: agentdesktop_core::config::ContainmentEngine::default(),
        egress_upstream: None,
        git: None,
    }
}

#[test]
fn containment_info_is_absent_when_unconfigured() {
    assert_eq!(
        containment_info(None, true, true, false, &|_| true, "mxc"),
        None
    );
}

#[test]
fn containment_info_is_available_for_a_user_mode_windows_daemon_with_the_executor() {
    let cfg = executor_cfg(r"C:\mxc\x64\wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, true, false, &|_| true, "mxc").unwrap();
    assert_eq!(
        info,
        ContainmentInfo {
            executor: r"C:\mxc\x64\wxc-exec.exe".into(),
            engine: "mxc".to_owned(),
            available: true,
            error: None,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            managed_settings_error: None,
            programs: BTreeMap::new(),
        }
    );
}

#[test]
fn containment_info_needs_user_mode() {
    let cfg = executor_cfg(r"C:\mxc\wxc-exec.exe");
    let info = containment_info(Some(&cfg), false, true, false, &|_| true, "mxc").unwrap();
    assert!(!info.available);
    assert_eq!(info.error.as_deref(), Some("requires user mode"));
    assert_eq!(info.executor, r"C:\mxc\wxc-exec.exe");
}

#[test]
fn containment_info_is_windows_only() {
    let cfg = executor_cfg("/opt/mxc/wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, false, false, &|_| true, "mxc").unwrap();
    assert!(!info.available);
    assert_eq!(info.error.as_deref(), Some("Windows only"));
}

#[test]
fn containment_info_names_a_missing_executor() {
    let cfg = executor_cfg(r"C:\missing\wxc-exec.exe");
    let seen = RefCell::new(Vec::new());
    let info = containment_info(
        Some(&cfg),
        true,
        true,
        false,
        &|path| {
            seen.borrow_mut().push(path.to_owned());
            false
        },
        "mxc",
    )
    .unwrap();
    assert!(!info.available);
    assert_eq!(
        info.error.as_deref(),
        Some(r"executor not found: C:\missing\wxc-exec.exe")
    );
    assert_eq!(
        &*seen.borrow(),
        &[PathBuf::from(r"C:\missing\wxc-exec.exe")]
    );
}

/// PR 6a, D13 G1: the Claude settings override no longer gates containment
/// globally; it is a Claude-only launch refusal (see
/// `a_claude_settings_override_refuses_only_claude_code_launches`).
#[test]
fn containment_info_stays_available_when_the_claude_settings_path_is_overridden() {
    let cfg = executor_cfg(r"C:\mxc\wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, true, true, &|_| true, "mxc").unwrap();
    assert!(info.available, "{info:?}");
    assert_eq!(info.error, None);
}

// ============================================================ peer_allowed (AC 4)

fn peer(sid: Option<&str>, app_container: bool) -> PipePeer {
    PipePeer {
        user_sid: sid.map(str::to_owned),
        app_container,
    }
}

#[test]
fn peer_allowed_admits_the_daemons_own_user_outside_an_app_container() {
    assert!(peer_allowed(
        Some(&peer(Some("S-1-5-21-1-2-3-1001"), false)),
        Some("S-1-5-21-1-2-3-1001")
    ));
}

#[test]
fn peer_allowed_refuses_another_user() {
    assert!(!peer_allowed(
        Some(&peer(Some("S-1-5-21-1-2-3-1002"), false)),
        Some("S-1-5-21-1-2-3-1001")
    ));
}

#[test]
fn peer_allowed_refuses_an_app_container_of_the_same_user() {
    assert!(!peer_allowed(
        Some(&peer(Some("S-1-5-21-1-2-3-1001"), true)),
        Some("S-1-5-21-1-2-3-1001")
    ));
}

#[test]
fn peer_allowed_refuses_a_missing_peer_and_an_unreadable_sid() {
    assert!(!peer_allowed(None, Some("S-1-5-21-1-2-3-1001")));
    assert!(!peer_allowed(
        Some(&peer(None, false)),
        Some("S-1-5-21-1-2-3-1001")
    ));
}

// ================================================================ validate_cwd (AC 5)

#[derive(Default)]
struct FakeFs {
    /// Junctions and symlinks: raw path to canonical path.
    mapping: BTreeMap<PathBuf, PathBuf>,
    /// Paths that do not exist.
    missing: BTreeSet<PathBuf>,
    /// Canonical paths that are files.
    files: BTreeSet<PathBuf>,
}

impl FakeFs {
    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        if self.missing.contains(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let resolved = self
            .mapping
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_owned());
        if self.missing.contains(&resolved) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(resolved)
    }
}

impl CwdFs for FakeFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.resolve(path)
    }

    fn kind(&self, path: &Path) -> io::Result<PathKind> {
        let resolved = self.resolve(path)?;
        Ok(PathKind {
            is_dir: !self.files.contains(&resolved),
        })
    }
}

/// The real filesystem, for the `..` and symlink tests and the router tests.
struct RealFs;

impl CwdFs for RealFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn kind(&self, path: &Path) -> io::Result<PathKind> {
        Ok(PathKind {
            is_dir: std::fs::metadata(path)?.is_dir(),
        })
    }
}

impl ProgramFs for RealFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// The harness's program filesystem: the real filesystem, plus
/// wrap-configured executables. The harness runs where the policy's
/// `C:\...` path cannot exist as a real file, so a Windows-absolute path
/// outside the harness root is answered as an existing file, canonical as
/// given; native resolution still goes to the real filesystem. The wrap
/// resolver's own refusals are covered by the pure tests in `wrap_tests.rs`.
struct WrapAwareProgramFs {
    /// The harness's canonical temp root; real files live under it.
    root: PathBuf,
}

impl WrapAwareProgramFs {
    fn pretend(&self, path: &Path) -> bool {
        let text = path.to_string_lossy();
        let windows_absolute =
            text.starts_with(r"\\") || (text.len() > 2 && text.as_bytes()[1] == b':');
        windows_absolute && !path.starts_with(&self.root)
    }
}

impl ProgramFs for WrapAwareProgramFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.pretend(path) {
            return Ok(path.to_owned());
        }
        std::fs::canonicalize(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        self.pretend(path) || path.is_file()
    }
}

fn cwd_context() -> CwdContext {
    let home = p("home/dev");
    CwdContext {
        state_dir: p("var/state/agentdesktop"),
        system_dirs: vec![p("windows"), p("programfiles"), p("programdata")],
        denied: vec![home.join(".ssh"), home.join(".aws"), home.join(".kube")],
        home,
        app_data: vec![],
    }
}

fn accepted(cwd: &Path) -> PathBuf {
    validate_cwd(cwd, &cwd_context(), &FakeFs::default())
        .unwrap_or_else(|error| panic!("{} should be accepted: {error}", cwd.display()))
}

fn rejected(cwd: &Path) {
    let result = validate_cwd(cwd, &cwd_context(), &FakeFs::default());
    assert!(result.is_err(), "{} should be rejected", cwd.display());
}

#[test]
fn cwd_in_a_project_under_home_is_accepted_and_returned_canonical() {
    assert_eq!(accepted(&p("home/dev/work/app")), p("home/dev/work/app"));
}

#[test]
fn cwd_may_be_a_sibling_whose_name_merely_starts_like_a_denied_directory() {
    accepted(&p("home/dev/.claude-work/app"));
    accepted(&p("home/dev/AppData2/app"));
    accepted(&p("home/dev-other/app"));
    accepted(&p("windows-lab/app"));
}

#[test]
fn cwd_may_not_be_a_drive_root() {
    rejected(&root());
}

#[test]
fn cwd_may_not_be_the_home_directory_or_an_ancestor_of_it() {
    rejected(&p("home/dev"));
    rejected(&p("home"));
}

#[test]
fn cwd_may_not_be_under_a_system_directory() {
    rejected(&p("windows"));
    rejected(&p("windows/temp/work"));
    rejected(&p("programfiles/tool"));
    rejected(&p("programdata/vendor"));
}

#[test]
fn cwd_may_not_be_under_the_state_directory() {
    rejected(&p("var/state/agentdesktop"));
    rejected(&p("var/state/agentdesktop/launches/0123456789abcdef"));
}

#[test]
fn cwd_may_not_be_under_appdata_or_the_claude_directory() {
    rejected(&p("home/dev/AppData"));
    rejected(&p("home/dev/AppData/Local/Temp"));
    rejected(&p("home/dev/.claude"));
    rejected(&p("home/dev/.claude/skills"));
}

#[test]
fn cwd_may_not_be_under_a_denied_path() {
    rejected(&p("home/dev/.ssh"));
    rejected(&p("home/dev/.aws/sso"));
    rejected(&p("home/dev/.kube"));
}

#[test]
fn cwd_may_not_be_under_a_redirected_appdata_path() {
    // Windows Folder Redirection can point %APPDATA% and %LOCALAPPDATA% outside
    // the home directory. The workspace must not be under them.
    let mut ctx = cwd_context();
    ctx.app_data.push(p("shares/users/dev/AppData/Roaming"));
    ctx.app_data.push(p("shares/users/dev/AppData/Local"));
    let rejected = |cwd: &Path| {
        assert!(
            validate_cwd(cwd, &ctx, &FakeFs::default()).is_err(),
            "{} should be rejected",
            cwd.display()
        )
    };
    rejected(&p("shares/users/dev/AppData/Roaming"));
    rejected(&p("shares/users/dev/AppData/Roaming/Microsoft/Windows"));
    rejected(&p("shares/users/dev/AppData/Local"));
    rejected(&p("shares/users/dev/AppData/Local/Temp"));
    // Workspace above the redirected path is also rejected (bidirectional check).
    rejected(&p("shares/users/dev"));
    rejected(&p("shares/users"));
}

#[test]
fn cwd_must_exist_and_be_a_directory() {
    let mut fs = FakeFs::default();
    fs.missing.insert(p("home/dev/gone"));
    fs.files.insert(p("home/dev/notes.txt"));
    let ctx = cwd_context();
    assert!(validate_cwd(&p("home/dev/gone"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("home/dev/notes.txt"), &ctx, &fs).is_err());
}

#[test]
fn cwd_with_a_character_cmd_exe_interprets_is_rejected() {
    for name in ["say\"hi", "50%off", "wow!"] {
        rejected(&p("home/dev/work").join(name));
    }
}

#[test]
fn cwd_is_judged_after_canonicalisation() {
    let ctx = cwd_context();
    let mut fs = FakeFs::default();
    fs.mapping
        .insert(p("home/dev/work/link"), p("windows/system32"));
    fs.mapping
        .insert(p("home/dev/work/ssh"), p("home/dev/.ssh"));
    fs.mapping
        .insert(p("home/dev/work/alias"), p("home/dev/work/real"));
    assert!(validate_cwd(&p("home/dev/work/link"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("home/dev/work/ssh"), &ctx, &fs).is_err());
    assert_eq!(
        validate_cwd(&p("home/dev/work/alias"), &ctx, &fs).unwrap(),
        p("home/dev/work/real"),
        "the request uses the canonical path"
    );
}

#[test]
fn context_directories_are_canonicalised_too() {
    // The configured home is a junction to the real home.
    let mut ctx = cwd_context();
    ctx.home = p("users/dev-link");
    let mut fs = FakeFs::default();
    fs.mapping.insert(p("users/dev-link"), p("home/dev"));
    assert!(validate_cwd(&p("home/dev"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("home/dev/.claude/x"), &ctx, &fs).is_err());
}

#[test]
fn a_context_directory_that_does_not_exist_is_used_raw() {
    let mut ctx = cwd_context();
    ctx.system_dirs.push(p("programfiles-x86"));
    let mut fs = FakeFs::default();
    fs.missing.insert(p("programfiles-x86"));
    assert!(
        validate_cwd(&p("programfiles-x86/tool"), &ctx, &fs).is_err(),
        "the raw path still guards"
    );
    assert!(
        validate_cwd(&p("home/dev/work/app"), &ctx, &fs).is_ok(),
        "a missing context directory does not fail the launch"
    );
}

#[test]
fn cwd_with_dot_dot_is_judged_by_where_it_really_points() {
    let temp = tempfile::tempdir().unwrap();
    let base = strip_verbatim(std::fs::canonicalize(temp.path()).unwrap());
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join("projects/app")).unwrap();
    let ctx = CwdContext {
        home: home.clone(),
        state_dir: base.join("state"),
        system_dirs: vec![],
        denied: vec![],
        app_data: vec![],
    };
    let sneaky = home.join("projects/app/../../.claude");
    assert!(validate_cwd(&sneaky, &ctx, &RealFs).is_err());
    let fine = home.join("projects/app/../app");
    assert_eq!(
        validate_cwd(&fine, &ctx, &RealFs).unwrap(),
        home.join("projects/app")
    );
}

#[cfg(unix)]
#[test]
fn cwd_through_a_real_symlink_is_judged_by_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join("projects/real")).unwrap();
    std::os::unix::fs::symlink(home.join(".claude"), home.join("projects/to-claude")).unwrap();
    std::os::unix::fs::symlink(home.join("projects/real"), home.join("projects/to-real")).unwrap();
    let ctx = CwdContext {
        home: home.clone(),
        state_dir: base.join("state"),
        system_dirs: vec![],
        denied: vec![],
        app_data: vec![],
    };
    assert!(validate_cwd(&home.join("projects/to-claude"), &ctx, &RealFs).is_err());
    assert_eq!(
        validate_cwd(&home.join("projects/to-real"), &ctx, &RealFs).unwrap(),
        home.join("projects/real")
    );
}

#[cfg(windows)]
#[test]
fn cwd_rules_compare_case_insensitively_on_windows() {
    let mut ctx = cwd_context();
    ctx.system_dirs = vec![PathBuf::from(r"C:\Windows")];
    ctx.home = PathBuf::from(r"C:\Users\Dev");
    let fs = FakeFs::default();
    assert!(validate_cwd(Path::new(r"c:\wInDoWs\temp\work"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new(r"C:\USERS\DEV"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new(r"C:\users\dev\.CLAUDE\skills"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new(r"C:\Users\Dev\work\app"), &ctx, &fs).is_ok());
}

#[cfg(windows)]
#[test]
fn cwd_may_not_be_a_unc_share_root() {
    let ctx = cwd_context();
    let fs = FakeFs::default();
    assert!(validate_cwd(Path::new(r"\\server\share"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new(r"\\server\share\"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new(r"\\server\share\team\app"), &ctx, &fs).is_ok());
}

#[test]
fn cwd_may_not_contain_the_state_directory() {
    let mut ctx = cwd_context();
    ctx.state_dir = p("home/dev/work/app/.state");
    let fs = FakeFs::default();
    assert!(validate_cwd(&p("home/dev/work/app"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("home/dev/work"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("home/dev"), &ctx, &fs).is_err());
}

#[test]
fn cwd_may_not_contain_a_guarded_directory_two_levels_deep() {
    let mut ctx = cwd_context();
    ctx.state_dir = p("var/local/state/agentdesktop");
    let fs = FakeFs::default();
    assert!(validate_cwd(&p("var/local"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("var"), &ctx, &fs).is_err());
}

#[test]
fn appdata_and_claude_are_canonicalised_before_guarding() {
    // The workspace is inside the real target of home/AppData, which is a
    // junction to another directory.
    let mut ctx = cwd_context();
    ctx.home = p("home/dev");
    let mut fs = FakeFs::default();
    fs.mapping
        .insert(p("home/dev/AppData"), p("users/dev/appdata-real"));
    fs.mapping
        .insert(p("home/dev/.claude"), p("users/dev/.claude-real"));
    assert!(validate_cwd(&p("users/dev/appdata-real/local"), &ctx, &fs).is_err());
    assert!(validate_cwd(&p("users/dev/.claude-real/skills"), &ctx, &fs).is_err());
}

#[cfg(unix)]
#[test]
fn appdata_symlink_target_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let home = base.join("home");
    let appdata_target = base.join("appdata-target");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&appdata_target).unwrap();
    std::fs::create_dir_all(appdata_target.join("local")).unwrap();
    std::os::unix::fs::symlink(&appdata_target, home.join("AppData")).unwrap();
    let ctx = CwdContext {
        home: home.clone(),
        state_dir: base.join("state"),
        system_dirs: vec![],
        denied: vec![],
        app_data: vec![],
    };
    assert!(
        validate_cwd(&appdata_target.join("local"), &ctx, &RealFs).is_err(),
        "workspace inside AppData symlink target should be rejected"
    );
}

// ======================================================== resolve_program (AC 6)

#[derive(Default)]
struct FakeProgramFs {
    files: BTreeSet<PathBuf>,
    /// Raw path to canonical path.
    canonical: BTreeMap<PathBuf, PathBuf>,
    /// Prepended to every canonical path (`\\?\` on Windows).
    prefix: &'static str,
}

impl ProgramFs for FakeProgramFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let canonical = self
            .canonical
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_owned());
        Ok(PathBuf::from(format!(
            "{}{}",
            self.prefix,
            canonical.display()
        )))
    }

    fn is_file(&self, path: &Path) -> bool {
        self.files.contains(path)
    }
}

const NOT_FOUND: &str = "Claude Code native install (claude.exe) not found for this user; npm installs are not supported in the box";

fn program_fs(files: &[PathBuf]) -> FakeProgramFs {
    FakeProgramFs {
        files: files.iter().cloned().collect(),
        ..Default::default()
    }
}

#[test]
fn only_a_cmd_or_extensionless_claude_is_not_found() {
    let home = p("home/dev");
    let npm = p("npm");
    let fs = program_fs(&[
        npm.join("claude.cmd"),
        npm.join("claude"),
        home.join(".local/bin/claude.cmd"),
    ]);
    assert_eq!(
        resolve_program(&home, &[npm], &fs),
        Err(NOT_FOUND.to_owned())
    );
}

#[test]
fn a_claude_exe_of_another_users_home_is_not_found() {
    let fs = program_fs(&[p("home/other/.local/bin/claude.exe")]);
    assert_eq!(
        resolve_program(&p("home/dev"), &[p("windows/system32")], &fs),
        Err(NOT_FOUND.to_owned())
    );
}

#[test]
fn a_candidate_that_canonicalises_outside_home_and_path_is_not_used() {
    let home = p("home/dev");
    let candidate = home.join(".local").join("bin").join("claude.exe");
    let mut fs = program_fs(std::slice::from_ref(&candidate));
    fs.canonical.insert(candidate, p("home/other/claude.exe"));
    assert_eq!(resolve_program(&home, &[], &fs), Err(NOT_FOUND.to_owned()));
}

#[test]
fn the_native_exe_in_home_wins_over_an_npm_claude_earlier_on_path() {
    let home = p("home/dev");
    let npm = p("npm");
    let own = home.join(".local").join("bin").join("claude.exe");
    let fs = program_fs(&[npm.join("claude.cmd"), npm.join("claude"), own.clone()]);
    assert_eq!(
        resolve_program(&home, std::slice::from_ref(&npm), &fs),
        Ok(own)
    );
}

#[test]
fn the_home_candidate_beats_a_claude_exe_on_path() {
    let home = p("home/dev");
    let own = home.join(".local").join("bin").join("claude.exe");
    let bin = p("tools");
    let fs = program_fs(&[bin.join("claude.exe"), own.clone()]);
    assert_eq!(resolve_program(&home, &[bin], &fs), Ok(own));
}

#[test]
fn without_a_home_install_the_first_path_directory_with_claude_exe_is_used() {
    let home = p("home/dev");
    let (first, second, third) = (p("a"), p("b"), p("c"));
    let fs = program_fs(&[second.join("claude.exe"), third.join("claude.exe")]);
    assert_eq!(
        resolve_program(&home, &[first, second.clone(), third], &fs),
        Ok(second.join("claude.exe"))
    );
}

#[test]
fn a_directory_named_claude_exe_is_not_a_regular_file() {
    let home = p("home/dev");
    // `is_file` is false for it: nothing is in the file set.
    let fs = program_fs(&[]);
    assert_eq!(
        resolve_program(&home, &[p("tools")], &fs),
        Err(NOT_FOUND.to_owned())
    );
}

#[cfg(windows)]
#[test]
fn the_extended_length_prefix_is_stripped_from_the_resolved_path() {
    let home = PathBuf::from(r"C:\Users\dev");
    let own = PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe");
    let mut fs = program_fs(std::slice::from_ref(&own));
    fs.prefix = r"\\?\";
    assert_eq!(resolve_program(&home, &[], &fs), Ok(own));
}

#[cfg(windows)]
#[test]
fn resolution_compares_case_insensitively_on_windows() {
    let home = PathBuf::from(r"C:\Users\Dev");
    let own = home.join(".local").join("bin").join("claude.exe");
    let mut fs = program_fs(std::slice::from_ref(&own));
    fs.canonical.insert(
        own.clone(),
        PathBuf::from(r"c:\users\dev\.local\bin\claude.exe"),
    );
    assert!(resolve_program(&home, &[], &fs).is_ok());
}

// ================================================================ check_args (AC 6)

const REJECTED_ARG: &str = "argument contains a character cmd.exe interprets";

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn a_plain_argument_is_passed_as_is() {
    assert_eq!(
        check_args(&args(&[
            "--model",
            "opus",
            "--allowedTools=PowerShell",
            r"C:\dir\file.txt"
        ])),
        Ok(args(&[
            "--model",
            "opus",
            "--allowedTools=PowerShell",
            r"C:\dir\file.txt"
        ]))
    );
    assert_eq!(check_args(&[]), Ok(vec![]));
}

#[test]
fn an_argument_with_a_space_is_wrapped_in_double_quotes() {
    assert_eq!(
        check_args(&args(&["hello world", r"C:\my dir\file.txt"])),
        Ok(args(&["\"hello world\"", "\"C:\\my dir\\file.txt\""]))
    );
}

#[test]
fn trailing_backslashes_before_the_closing_quote_are_doubled() {
    assert_eq!(
        check_args(&args(&[r"C:\my dir\", r"C:\my dir\\"])),
        Ok(args(&[r#""C:\my dir\\""#, r#""C:\my dir\\\\""#]))
    );
    assert_eq!(
        check_args(&args(&[r"C:\dir\"])),
        Ok(args(&[r"C:\dir\"])),
        "an unquoted argument keeps its backslashes"
    );
}

#[test]
fn an_empty_argument_becomes_a_pair_of_quotes() {
    assert_eq!(check_args(&args(&["", "x"])), Ok(args(&["\"\"", "x"])));
}

#[test]
fn each_character_cmd_exe_interprets_rejects_the_argument() {
    for bad in ["\"", "%", "^", "&", "|", "<", ">", "!"] {
        for argument in [
            format!("a{bad}b"),
            bad.to_owned(),
            format!("with space {bad}"),
        ] {
            assert_eq!(
                check_args(std::slice::from_ref(&argument)),
                Err(REJECTED_ARG.to_owned()),
                "{argument:?}"
            );
        }
    }
}

#[test]
fn a_control_character_rejects_the_argument() {
    for argument in ["a\nb", "a\rb", "a\u{0}b", "a\u{7}b", "a\u{1b}[31m"] {
        assert_eq!(
            check_args(&[argument.to_owned()]),
            Err(REJECTED_ARG.to_owned()),
            "{argument:?}"
        );
    }
}

#[test]
fn one_bad_argument_rejects_the_whole_list() {
    assert_eq!(
        check_args(&args(&["--model", "x", "a&b", "ok"])),
        Err(REJECTED_ARG.to_owned())
    );
}

// ============================================================ build_request (AC 7)
//
// Windows-style paths are plain strings here; the builder joins them with `\`
// and replaces `/` with `\`, on every platform. `exists` is called with the
// `\`-joined form.

const LAUNCH_ID: &str = "0123456789abcdef";
const HOME: &str = r"C:\Users\dev";
const CWD: &str = r"C:\lab\ws pr1";
const LAUNCH_DIR: &str = r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef";

fn base_env() -> BTreeMap<String, String> {
    [
        ("SystemRoot", r"C:\Windows"),
        ("windir", r"C:\Windows"),
        ("SystemDrive", "C:"),
        ("ComSpec", r"C:\Windows\system32\cmd.exe"),
        ("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
        ("USERPROFILE", HOME),
        ("HOMEDRIVE", "C:"),
        ("HOMEPATH", r"\Users\dev"),
        ("APPDATA", r"C:\Users\dev\AppData\Roaming"),
        ("LOCALAPPDATA", r"C:\Users\dev\AppData\Local"),
        ("ProgramFiles", r"C:\Program Files"),
        ("ProgramData", r"C:\ProgramData"),
        ("USERNAME", "dev"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// `process.env` as a map (the request carries `KEY=VALUE` entries).
fn env_map(request: &serde_json::Value) -> BTreeMap<String, String> {
    list(request, "/process/env")
        .into_iter()
        .map(|entry| {
            let (key, value) = entry.split_once('=').expect("KEY=VALUE");
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

/// The paths of the snapshot scenario that exist on the host.
fn snapshot_existing() -> BTreeSet<String> {
    [
        r"C:\lab\ws pr1\.claude",
        r"C:\Users\dev\.local\bin",
        r"C:\Users\dev\.local\share\claude",
        r"C:\Program Files\Git",
        r"C:\lab\agentdesktop",
        r"C:\Program Files\ClaudeCode",
        r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef\credential",
        r"C:\Users\dev\.claude\settings.json",
        r"C:\Users\dev\.claude\plugins",
        r"C:\lab\ws pr1\.claude\settings.local.json",
        r"C:\lab\ws pr1\.mcp.json",
        r"C:\lab\ws pr1\.git",
        r"C:\lab\ws pr1\.git\HEAD",
        r"C:\lab\ws pr1\.git\config",
        r"C:\lab\ws pr1\.git\hooks",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn snapshot_directories() -> BTreeSet<String> {
    [
        r"C:\lab\ws pr1\.claude",
        r"C:\Users\dev\.local\bin",
        r"C:\Users\dev\.local\share\claude",
        r"C:\Program Files\Git",
        r"C:\lab\agentdesktop",
        r"C:\Program Files\ClaudeCode",
        r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef\credential",
        r"C:\Users\dev\.claude\plugins",
        r"C:\lab\ws pr1\.git",
        r"C:\lab\ws pr1\.git\hooks",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn launch_input<'a>(
    exists: &'a dyn Fn(&Path) -> bool,
    is_dir: &'a dyn Fn(&Path) -> bool,
) -> LaunchInput<'a> {
    LaunchInput {
        launch_id: LAUNCH_ID.into(),
        executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
        client_exe_dir: PathBuf::from(r"C:\lab\agentdesktop"),
        cwd: PathBuf::from(CWD),
        args: args(&[
            "--debug-file",
            r#""C:\lab\ws pr1\pr1-l2.log""#,
            "--allowedTools",
            "PowerShell",
        ]),
        home: PathBuf::from(HOME),
        // The user-mode state directory really contains a `/` on Windows.
        state_dir: PathBuf::from(r"C:\Users\dev\.local/state\agentdesktop"),
        launch_dir: PathBuf::from(
            r"C:\Users\dev\.local/state\agentdesktop\launches\0123456789abcdef",
        ),
        git_dir: Some(PathBuf::from(r"C:\Program Files\Git")),
        program_files_claude: Some(PathBuf::from(r"C:\Program Files\ClaudeCode")),
        egress: agentdesktop_core::config::ContainmentEgress::Allow,
        relay_base_url: "http://127.0.0.1:4000/sandbox".into(),
        pairing: "test-pairing-value".into(),
        base_env: base_env(),
        // The snapshot scenario has no ssh hosts (G5: the writer defines the
        // ssh additions to `plan()`'s input; ledgered).
        ssh_enabled: false,
        exists,
        is_dir,
        console: crate::launch::plan::ConsolePlan::Window,
    }
}

fn request_json(
    existing: &BTreeSet<String>,
    directories: &BTreeSet<String>,
    tweak: impl FnOnce(&mut LaunchInput),
) -> serde_json::Value {
    let exists = |path: &Path| existing.contains(path.to_str().expect("UTF-8 path"));
    let is_dir = |path: &Path| directories.contains(path.to_str().expect("UTF-8 path"));
    let mut input = launch_input(&exists, &is_dir);
    tweak(&mut input);
    serde_json::to_value(build_request(&input)).expect("the request serializes")
}

fn list(value: &serde_json::Value, pointer: &str) -> Vec<String> {
    value
        .pointer(pointer)
        .unwrap_or_else(|| panic!("{pointer} in {value}"))
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} is a list"))
        .iter()
        .map(|item| item.as_str().expect("a path string").to_owned())
        .collect()
}

fn readonly(value: &serde_json::Value) -> Vec<String> {
    list(value, "/filesystem/readonlyPaths")
}

fn denied(value: &serde_json::Value) -> Vec<String> {
    list(value, "/filesystem/deniedPaths")
}

fn readwrite(value: &serde_json::Value) -> Vec<String> {
    list(value, "/filesystem/readwritePaths")
}

/// The integrity roots and the request's protections must not drift apart:
/// every recursively checked root is read-only or denied in the real request.
#[test]
fn the_integrity_roots_are_protected_in_the_request() {
    let value = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let mut protected: std::collections::BTreeSet<String> = readonly(&value).into_iter().collect();
    protected.extend(denied(&value));
    let roots = super::integrity::integrity_paths(&PathBuf::from(HOME), &PathBuf::from(CWD));
    let root_paths: std::collections::BTreeSet<String> = roots
        .iter()
        .map(|root| root.path.to_str().unwrap().replace('/', "\\"))
        .collect();
    for root in &roots {
        if root.mode != super::integrity::RootMode::Recursive {
            continue; // the identity-only folders are writable by design
        }
        let path = root.path.to_str().unwrap().replace('/', "\\");
        let github = format!(r"{CWD}\.github");
        if path.starts_with(&format!(r"{github}\")) && protected.contains(&github) {
            continue; // covered by the whole-folder denial of a missing `.github`
        }
        assert!(
            protected.contains(&path),
            "integrity root {path} is neither read-only nor denied in the request: {protected:?}"
        );
    }

    // The reverse direction: every protected entry is either an integrity
    // root or on this named list of protected entries outside the roots
    // (installations, secret denials, and the git configuration files, which
    // have their own grant). A new grant or denial must be classified here or
    // in integrity_paths, never silently.
    let outside_the_roots: Vec<String> = [
        r"C:\Users\dev\.local\bin".into(),
        r"C:\Users\dev\.local\share\claude".into(),
        r"C:\Program Files\Git".into(),
        r"C:\lab\agentdesktop".into(),
        r"C:\Program Files\ClaudeCode".into(),
        r"C:\Users\dev\.gitconfig".into(),
        // XDG config is conditionally denied (only when parent exists); it's
        // not in the default snapshot's denied list.
        format!(r"{LAUNCH_DIR}\credential"),
        format!(r"{LAUNCH_DIR}\git_config"),
        r"C:\Users\dev\.claude\.credentials.json".into(),
        r"C:\Users\dev\.claude\history.jsonl".into(),
        r"C:\Users\dev\.claude\ide".into(),
        r"C:\Users\dev\.ssh".into(),
        r"C:\Users\dev\.aws".into(),
        r"C:\Users\dev\.azure".into(),
        r"C:\Users\dev\.kube".into(),
    ]
    .into_iter()
    .collect();
    for entry in &protected {
        assert!(
            root_paths.contains(entry) || outside_the_roots.contains(entry),
            "unclassified protected entry {entry}: add it to integrity_paths or name it here"
        );
    }
}

/// Path lists are sets to the sandbox; the snapshot compares them sorted.
fn sorted_paths(mut value: serde_json::Value) -> serde_json::Value {
    for key in ["readwritePaths", "readonlyPaths", "deniedPaths"] {
        if let Some(items) = value["filesystem"][key].as_array_mut() {
            items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    value
}

#[test]
fn the_request_matches_the_checked_in_snapshot() {
    let actual = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("testdata/request-claude.json")).unwrap();
    assert_eq!(sorted_paths(actual), sorted_paths(expected));
}

#[test]
fn the_request_has_the_fixed_shape_and_no_process_timeout() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    assert_eq!(request["version"], "1.0.0");
    assert_eq!(request["containment"], "processcontainer");
    assert_eq!(request["containerId"], format!("agentdesktop-{LAUNCH_ID}"));
    assert_eq!(
        request["ui"],
        serde_json::json!({"disable": false, "clipboard": "none", "injection": false})
    );
    assert_eq!(
        request["network"],
        serde_json::json!({
            "egress": {"default": "allow"},
            "ingress": {"default": "allow", "hostLoopback": "allow"},
        })
    );
    assert_eq!(request["process"]["inheritDefaultEnv"], false);
    assert!(request["process"].get("timeout").is_none());
}

// AC 1: egress deny test
#[test]
fn egress_deny_sets_the_network_default_to_deny() {
    use agentdesktop_core::config::ContainmentEgress;
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.egress = ContainmentEgress::Deny;
    });
    assert_eq!(
        request["network"],
        serde_json::json!({
            "egress": {"default": "deny"},
            "ingress": {"default": "allow", "hostLoopback": "allow"},
        }),
        "egress deny sets network.egress.default to deny and hostLoopback to allow"
    );
}

#[test]
fn the_command_line_starts_the_program_through_cmd_without_autorun_or_delayed_expansion() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    assert_eq!(
        request["process"]["commandLine"],
        concat!(
            r#"cmd.exe /d /v:off /c start "agentdesktop: claude" /WAIT /D "C:\lab\ws pr1" "#,
            r#""C:\Users\dev\.local\bin\claude.exe" "#,
            r#"--debug-file "C:\lab\ws pr1\pr1-l2.log" --allowedTools PowerShell"#
        )
    );
    let without_args = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.args.clear()
    });
    assert_eq!(
        without_args["process"]["commandLine"]
            .as_str()
            .unwrap()
            .trim_end(),
        r#"cmd.exe /d /v:off /c start "agentdesktop: claude" /WAIT /D "C:\lab\ws pr1" "C:\Users\dev\.local\bin\claude.exe""#
    );
}

#[test]
fn the_attach_command_line_runs_the_program_without_start() {
    // Same-terminal hosting: no `start`, so the boxed program inherits the
    // executor's console and cmd returns the program's own exit code.
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.console = crate::launch::plan::ConsolePlan::Attach { client_pid: 4242 };
    });
    assert_eq!(
        request["process"]["commandLine"],
        concat!(
            r#"cmd.exe /d /v:off /c ""C:\Users\dev\.local\bin\claude.exe" "#,
            r#"--debug-file "C:\lab\ws pr1\pr1-l2.log" --allowedTools PowerShell""#
        )
    );
    let without_args = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.console = crate::launch::plan::ConsolePlan::Attach { client_pid: 4242 };
        input.args.clear();
    });
    assert_eq!(
        without_args["process"]["commandLine"],
        r#"cmd.exe /d /v:off /c ""C:\Users\dev\.local\bin\claude.exe"""#
    );
}

#[test]
fn a_fast_exit_is_the_programs_own_result_only_after_the_attach_marker() {
    // `claude --version` exits within the early-exit watch; in same-terminal
    // mode that is the program's own result once the helper has spawned the
    // executor (its started marker is present), not a failed box start. Before
    // the marker, and for every windowed launch, the early-exit failure path
    // stays. The marker, not an exit-code value, is the discriminator, since a
    // program may legitimately exit with the helper's failure code.
    let dir = tempfile::tempdir().unwrap();
    let attach = crate::launch::plan::ConsolePlan::Attach { client_pid: 4242 };
    let window = crate::launch::plan::ConsolePlan::Window;
    assert!(!super::early_exit_is_program_result(&attach, dir.path()));
    std::fs::write(
        dir.path()
            .join(crate::launch::console::ATTACH_STARTED_MARKER),
        [],
    )
    .unwrap();
    assert!(super::early_exit_is_program_result(&attach, dir.path()));
    assert!(!super::early_exit_is_program_result(&window, dir.path()));
}

#[test]
fn an_attach_request_from_the_daemon_own_process_downgrades_to_window() {
    // The daemon must never attach to its own console; the same validation
    // refuses a pid it cannot own (on every platform this test host rejects
    // the request and launches windowed).
    assert_eq!(
        super::console_plan(Some(agentdesktop_core::model::ConsoleAttach {
            client_pid: std::process::id(),
        })),
        crate::launch::plan::ConsolePlan::Window
    );
    assert_eq!(
        super::console_plan(None),
        crate::launch::plan::ConsolePlan::Window
    );
}

#[test]
fn the_environment_is_closed_and_expands_the_path_from_system_root() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let env = env_map(&request);
    assert_eq!(
        env["PATH"],
        r"C:\Windows\System32;C:\Windows;C:\Windows\System32\WindowsPowerShell\v1.0;C:\Program Files\Git\cmd;C:\Users\dev\.local\bin;C:\lab\agentdesktop"
    );
    assert_eq!(env["TERM"], "xterm-256color");
    assert_eq!(env["TEMP"], format!(r"{LAUNCH_DIR}\tmp"));
    assert_eq!(env["TMP"], format!(r"{LAUNCH_DIR}\tmp"));
    assert_eq!(env["CLAUDE_CONFIG_DIR"], r"C:\Users\dev\.claude");
    assert_eq!(env["CLAUDE_CODE_USE_POWERSHELL_TOOL"], "1");
    assert_eq!(env["DISABLE_AUTOUPDATER"], "1");
    assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4000/sandbox");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "placeholder");
    assert_eq!(
        env["ANTHROPIC_CUSTOM_HEADERS"],
        "x-agentdesktop-pairing: test-pairing-value"
    );
    assert_eq!(env["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"], "1");
    for (key, value) in base_env() {
        assert_eq!(
            env[&key], value,
            "{key} comes from the daemon's environment"
        );
    }
}

#[test]
fn the_path_has_no_git_entry_without_git() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.git_dir = None
    });
    let env = env_map(&request);
    let path = env["PATH"].as_str();
    assert_eq!(
        path,
        r"C:\Windows\System32;C:\Windows;C:\Windows\System32\WindowsPowerShell\v1.0;C:\Users\dev\.local\bin;C:\lab\agentdesktop"
    );
    assert!(!readonly(&request).iter().any(|path| path.contains("Git")));
}

// PR 6 (AC-8b, AC-8d): plan_program's in-box PATH, pure.

/// AC-8b: wrap mode resolves `containment.programs.<p>.executable` instead
/// of `resolve_program_for`, but `plan_program` itself takes only the
/// resolved `executable` and knows nothing about which resolver produced it.
/// Holding the resolved directory fixed, the computed PATH must be identical
/// however the executable was resolved.
#[test]
fn the_in_box_path_is_the_same_for_a_wrap_resolved_and_a_native_resolved_executable_in_the_same_directory()
 {
    let exe_dir = r"C:\Program Files\Anthropic\ClaudeCode";
    let path_for = |executable: PathBuf| {
        let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
            input.executable = executable;
        });
        env_map(&request)["PATH"].clone()
    };
    // Native resolution (`resolve_program_for`) would have found this exact
    // file; wrap resolution (`resolve_managed_program`, D8) resolves the
    // admin-installed path directly. Both land in the same directory here.
    let native_like = path_for(PathBuf::from(format!(r"{exe_dir}\claude.exe")));
    let wrap_like = path_for(PathBuf::from(format!(r"{exe_dir}\claude.exe")));
    assert_eq!(native_like, wrap_like);
    assert!(native_like.contains(exe_dir), "{native_like}");
}

/// AC-8d (v3.1, N7): the launcher directory the MSI prepends to the machine
/// PATH is never added to the in-box PATH, under any input: a box must
/// resolve `claude` to the program directory, never the launcher.
#[test]
fn the_in_box_path_never_contains_the_launcher_directory() {
    // DV6-5: the packaged launcher directory lives under the product MSI's
    // own install directory (Tauri productName "Agent Desktop").
    const LAUNCHER_DIR: &str = r"C:\Program Files\Agent Desktop\launchers";
    for tweak in [None, Some(r"C:\lab\ws pr1" as &str)] {
        let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
            if let Some(cwd) = tweak {
                input.cwd = PathBuf::from(cwd);
            }
        });
        let path = env_map(&request)["PATH"].clone();
        assert!(
            !path.to_lowercase().contains(&LAUNCHER_DIR.to_lowercase()),
            "{path}"
        );
    }
}

#[test]
fn the_request_contains_no_percent_sign_and_no_forward_slash_in_any_path() {
    // Inputs with `/` in every position a path can come from.
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.cwd = PathBuf::from(r"C:\lab\ws pr1");
        input.client_exe_dir = PathBuf::from("C:/lab/agentdesktop");
    });
    assert!(!request.to_string().contains('%'));

    let mut paths = readwrite(&request);
    paths.extend(readonly(&request));
    paths.extend(denied(&request));
    paths.push(request["process"]["cwd"].as_str().unwrap().to_owned());
    for key in ["PATH", "TEMP", "TMP", "CLAUDE_CONFIG_DIR"] {
        paths.push(env_map(&request)[key].clone());
    }
    for path in paths {
        assert!(!path.contains('/'), "{path}");
    }
    assert!(readonly(&request).contains(&r"C:\lab\agentdesktop".to_owned()));
}

#[test]
fn a_url_in_the_request_keeps_its_slashes() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |input| {
        input.relay_base_url = "http://127.0.0.1:4000/sandbox".into();
    });
    assert_eq!(
        env_map(&request)["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:4000/sandbox"
    );
}

#[test]
fn read_write_paths_are_the_workspace_the_claude_directory_and_the_launch_tmp() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let mut actual = readwrite(&request);
    actual.sort();
    let mut expected = vec![
        CWD.to_owned(),
        r"C:\Users\dev\.claude".to_owned(),
        format!(r"{LAUNCH_DIR}\tmp"),
    ];
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn read_only_runtime_directories_are_listed_only_when_they_exist() {
    let all = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let candidates = [
        r"C:\Users\dev\.local\bin",
        r"C:\Users\dev\.local\share\claude",
        r"C:\Program Files\Git",
        r"C:\lab\agentdesktop",
        r"C:\Program Files\ClaudeCode",
    ];
    for candidate in candidates {
        assert!(
            readonly(&all).contains(&candidate.to_owned()),
            "{candidate}"
        );
        let mut existing = snapshot_existing();
        existing.remove(candidate);
        let directories = snapshot_directories();
        let request = request_json(&existing, &directories, |_| {});
        assert!(
            !readonly(&request).contains(&candidate.to_owned()),
            "{candidate} is absent from the host"
        );
        assert!(
            !denied(&request).contains(&candidate.to_owned()),
            "{candidate} is optional, not denied"
        );
    }
}

/// The executable-config rule: exists -> read-only, absent -> denied.
#[test]
fn executable_configuration_is_read_only_when_present_and_denied_when_absent() {
    let rule = [
        r"C:\Users\dev\.claude\settings.json",
        r"C:\Users\dev\.claude\hooks",
        r"C:\Users\dev\.claude\plugins",
        r"C:\lab\ws pr1\.claude\settings.json",
        r"C:\lab\ws pr1\.claude\settings.local.json",
        r"C:\lab\ws pr1\.claude\hooks",
        r"C:\lab\ws pr1\.mcp.json",
        r"C:\lab\ws pr1\.git\config",
        r"C:\lab\ws pr1\.git\hooks",
    ];
    for path in rule {
        let mut with = snapshot_existing();
        with.insert(path.to_owned());
        let directories = snapshot_directories();
        let present = request_json(&with, &directories, |_| {});
        assert!(
            readonly(&present).contains(&path.to_owned()),
            "{path} exists"
        );
        assert!(
            !denied(&present).contains(&path.to_owned()),
            "{path} exists"
        );
        assert!(!readwrite(&present).contains(&path.to_owned()));

        let mut without = snapshot_existing();
        without.remove(path);
        let directories = snapshot_directories();
        let absent = request_json(&without, &directories, |_| {});
        assert!(
            denied(&absent).contains(&path.to_owned()),
            "{path} is absent"
        );
        assert!(
            !readonly(&absent).contains(&path.to_owned()),
            "{path} is absent"
        );
    }
}

#[test]
fn a_workspace_without_a_git_directory_denies_it_so_the_box_cannot_init_one() {
    let mut existing = snapshot_existing();
    let mut directories = snapshot_directories();
    for gone in [
        r"C:\lab\ws pr1\.git",
        r"C:\lab\ws pr1\.git\HEAD",
        r"C:\lab\ws pr1\.git\config",
        r"C:\lab\ws pr1\.git\hooks",
    ] {
        existing.remove(gone);
        directories.remove(gone);
    }
    let request = request_json(&existing, &directories, |_| {});
    assert!(denied(&request).contains(&r"C:\lab\ws pr1\.git".to_owned()));
    assert!(!readonly(&request).contains(&r"C:\lab\ws pr1\.git".to_owned()));
    assert!(!readwrite(&request).contains(&r"C:\lab\ws pr1\.git".to_owned()));
}

#[test]
fn a_git_directory_is_left_writable_apart_from_its_config_and_hooks() {
    let request = request_json(&snapshot_existing(), &snapshot_directories(), |_| {});
    let git = r"C:\lab\ws pr1\.git".to_owned();
    assert!(!denied(&request).contains(&git));
    assert!(!readonly(&request).contains(&git));
    assert!(readonly(&request).contains(&r"C:\lab\ws pr1\.git\config".to_owned()));
    assert!(readonly(&request).contains(&r"C:\lab\ws pr1\.git\hooks".to_owned()));
}

#[test]
fn an_empty_git_directory_gets_config_and_hooks_guards() {
    // An empty `.git` directory: exists as a directory but has no children.
    let mut existing = snapshot_existing();
    for gone in [
        r"C:\lab\ws pr1\.git\HEAD",
        r"C:\lab\ws pr1\.git\config",
        r"C:\lab\ws pr1\.git\hooks",
    ] {
        existing.remove(gone);
    }
    let directories = snapshot_directories();
    let request = request_json(&existing, &directories, |_| {});
    // The guards are emitted even though the children are absent.
    assert!(denied(&request).contains(&r"C:\lab\ws pr1\.git\config".to_owned()));
    assert!(denied(&request).contains(&r"C:\lab\ws pr1\.git\hooks".to_owned()));
    assert!(denied(&request).contains(&r"C:\lab\ws pr1\.git\commondir".to_owned()));
}

#[test]
fn a_git_file_of_a_worktree_or_submodule_is_left_as_it_is() {
    // `.git` exists but is a file, not a directory: worktree or submodule.
    let mut existing = snapshot_existing();
    let mut directories = snapshot_directories();
    // Remove .git from directories to make it a file.
    directories.remove(r"C:\lab\ws pr1\.git");
    // A .git file has no children in the filesystem.
    for gone in [
        r"C:\lab\ws pr1\.git\HEAD",
        r"C:\lab\ws pr1\.git\config",
        r"C:\lab\ws pr1\.git\hooks",
    ] {
        existing.remove(gone);
    }
    let request = request_json(&existing, &directories, |_| {});
    // PR 4: the home git user-configuration files are always denied and the
    // generated git_config always granted, so the check is scoped to the
    // workspace `.git` the test is about.
    for path in readwrite(&request)
        .into_iter()
        .chain(readonly(&request))
        .chain(denied(&request))
    {
        // `.github` (the union pins) merely shares the prefix.
        assert!(
            path != r"C:\lab\ws pr1\.git" && !path.starts_with(r"C:\lab\ws pr1\.git\"),
            "{path}"
        );
    }
}

#[test]
fn credential_and_ssh_style_paths_are_denied_whether_or_not_they_exist() {
    let always = [
        r"C:\Users\dev\.claude\.credentials.json",
        r"C:\Users\dev\.claude\history.jsonl",
        r"C:\Users\dev\.ssh",
        r"C:\Users\dev\.aws",
        r"C:\Users\dev\.azure",
        r"C:\Users\dev\.kube",
    ];
    for (existing, directories) in [
        (BTreeSet::new(), BTreeSet::new()),
        (snapshot_existing(), snapshot_directories()),
    ] {
        let request = request_json(&existing, &directories, |_| {});
        for path in always {
            assert!(denied(&request).contains(&path.to_owned()), "{path}");
            assert!(!readonly(&request).contains(&path.to_owned()), "{path}");
        }
    }
}

#[test]
fn git_user_config_always_denied_and_xdg_config_denied_only_when_parent_exists() {
    // PR 4 AC 6: `~/.gitconfig` is always denied (parent `$HOME` always exists).
    // The XDG path `~/.config/git/config` is denied only when its parent
    // `~/.config/git` exists, to satisfy MXC's denied-path parent rule.
    let gitconfig = r"C:\Users\dev\.gitconfig";
    let xdg_config = r"C:\Users\dev\.config\git\config";
    let xdg_parent = r"C:\Users\dev\.config\git";

    // Case 1: XDG parent directory exists.
    let mut existing_with_parent = snapshot_existing();
    existing_with_parent.insert(xdg_parent.to_owned());
    let mut dirs_with_parent = snapshot_directories();
    dirs_with_parent.insert(xdg_parent.to_owned());
    let request_parent_exists = request_json(&existing_with_parent, &dirs_with_parent, |_| {});

    assert!(
        denied(&request_parent_exists).contains(&gitconfig.to_owned()),
        "~/.gitconfig is always denied"
    );
    assert!(
        denied(&request_parent_exists).contains(&xdg_config.to_owned()),
        "XDG config is denied when parent exists; got {:?}",
        denied(&request_parent_exists)
    );

    // Case 2: XDG parent directory does not exist.
    let existing_no_parent = snapshot_existing();
    let dirs_no_parent = snapshot_directories();
    let request_parent_absent = request_json(&existing_no_parent, &dirs_no_parent, |_| {});

    assert!(
        denied(&request_parent_absent).contains(&gitconfig.to_owned()),
        "~/.gitconfig is always denied even when XDG parent absent"
    );
    assert!(
        !denied(&request_parent_absent).contains(&xdg_config.to_owned()),
        "XDG config is not denied when parent does not exist (MXC parent rule)"
    );
}

#[test]
fn write_request_creates_the_private_launch_dir_and_writes_json_without_a_bom() {
    let temp = tempfile::tempdir().unwrap();
    let launch_dir = temp.path().join("launches").join(LAUNCH_ID);
    let existing = snapshot_existing();
    let directories = snapshot_directories();
    let exists = |path: &Path| existing.contains(path.to_str().unwrap());
    let is_dir = |path: &Path| directories.contains(path.to_str().unwrap());
    let request = build_request(&launch_input(&exists, &is_dir));

    let path = write_request(&launch_dir, &serde_json::to_vec_pretty(&request).unwrap()).unwrap();

    assert_eq!(path, launch_dir.join("request.json"));
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes.starts_with(&[0xEF, 0xBB, 0xBF]), "no UTF-8 BOM");
    assert!(!bytes.starts_with(&[0xFF, 0xFE]) && !bytes.starts_with(&[0xFE, 0xFF]));
    let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(written, serde_json::to_value(&request).unwrap());
    assert_eq!(written["containerId"], format!("agentdesktop-{LAUNCH_ID}"));
}

#[cfg(unix)]
#[test]
fn write_request_uses_a_private_directory_and_a_0600_file() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let launch_dir = temp.path().join("launches").join(LAUNCH_ID);
    let exists = |_: &Path| false;
    let is_dir = |_: &Path| false;
    let request = build_request(&launch_input(&exists, &is_dir));

    let path = write_request(&launch_dir, &serde_json::to_vec_pretty(&request).unwrap()).unwrap();

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&launch_dir), 0o700);
    assert_eq!(mode(&path), 0o600);
}

// =========================================================== LaunchRegistry (AC 9)

fn launch(id: &str, started_at: u64) -> Launch {
    Launch {
        launch_id: id.to_owned(),
        program: "claude-code".into(),
        cwd: p("work/app"),
        container_id: format!("agentdesktop-{id}"),
        status: LaunchStatus::Running,
        exit_code: None,
        started_at_unix_seconds: started_at,
        ended_at_unix_seconds: None,
        session_id: format!("session-{id}"),
        integrity_drift: false,
        managed_settings_source: None,
        managed_hooks_locked: false,
        managed_mcp_locked: false,
        console: agentdesktop_core::model::LaunchConsole::Window,
    }
}

fn ids(registry: &LaunchRegistry) -> Vec<String> {
    registry
        .list()
        .into_iter()
        .map(|launch| launch.launch_id)
        .collect()
}

#[test]
fn a_new_registry_is_empty() {
    assert!(LaunchRegistry::new().list().is_empty());
}

#[test]
fn the_registry_lists_launches_by_start_time_ascending() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("c", 30));
    registry.insert(launch("a", 10));
    registry.insert(launch("b", 20));
    assert_eq!(ids(&registry), ["a", "b", "c"]);
}

#[test]
fn clones_of_the_registry_share_their_launches() {
    let registry = LaunchRegistry::new();
    let clone = registry.clone();
    clone.insert(launch("a", 10));
    assert_eq!(ids(&registry), ["a"]);
}

#[test]
fn marking_a_launch_exited_records_the_code_and_the_time() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.insert(launch("b", 20));

    registry.mark_exited("a", Some(3), 99);

    let listed = registry.list();
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, Some(3));
    assert_eq!(listed[0].ended_at_unix_seconds, Some(99));
    assert_eq!(
        listed[1].status,
        LaunchStatus::Running,
        "the other launch is untouched"
    );
    assert_eq!(listed[1].exit_code, None);
    assert_eq!(listed[1].ended_at_unix_seconds, None);
}

#[test]
fn an_exit_without_a_code_is_recorded_as_exited() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.mark_exited("a", None, 50);
    let listed = registry.list();
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, None);
}

#[test]
fn the_registry_keeps_every_running_launch_and_the_last_32_exited_ones() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("running", 1));
    for index in 0..40u64 {
        let id = format!("exited-{index:02}");
        registry.insert(launch(&id, 100 + index));
        registry.mark_exited(&id, Some(0), 1_000 + index);
    }

    let listed = ids(&registry);

    assert_eq!(listed.len(), 33, "1 running + 32 exited");
    assert_eq!(listed[0], "running");
    assert!(
        !listed.contains(&"exited-00".to_owned()),
        "the oldest exits are dropped"
    );
    assert!(!listed.contains(&"exited-07".to_owned()));
    assert!(listed.contains(&"exited-08".to_owned()));
    assert!(listed.contains(&"exited-39".to_owned()));
}

#[test]
fn many_running_launches_are_never_dropped() {
    let registry = LaunchRegistry::new();
    for index in 0..50u64 {
        registry.insert(launch(&format!("run-{index:02}"), index));
    }
    assert_eq!(registry.list().len(), 50);
}

// AC 3: integrity_drift registry tests

#[test]
fn integrity_drift_can_be_set_on_a_running_launch() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.set_integrity_drift("a");
    assert!(registry.list()[0].integrity_drift);
}

#[test]
fn integrity_drift_stays_true_on_second_set() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.set_integrity_drift("a");
    assert!(registry.list()[0].integrity_drift);
    registry.set_integrity_drift("a");
    assert!(registry.list()[0].integrity_drift);
}

#[test]
fn setting_integrity_drift_on_unknown_id_is_a_noop() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.set_integrity_drift("nope");
    assert!(!registry.list()[0].integrity_drift);
}

// ================================================================== sweep (AC 9)

fn make_launch_dir(state: &Path, id: &str, pid_file: Option<&str>) -> PathBuf {
    let dir = state.join("launches").join(id);
    std::fs::create_dir_all(dir.join("credential")).unwrap();
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    std::fs::write(dir.join("credential").join("token"), "secret-gateway-token").unwrap();
    std::fs::write(dir.join("request.json"), "{}").unwrap();
    std::fs::write(dir.join("wxc-exec.log"), "log").unwrap();
    if let Some(pid_file) = pid_file {
        std::fs::write(dir.join("pid"), pid_file).unwrap();
    }
    dir
}

#[test]
fn sweep_of_a_state_directory_without_launches_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    sweep(temp.path(), &|_, _| panic!("no launches, nothing to ask")).unwrap();
}

#[test]
fn sweep_removes_the_whole_directory_of_a_launch_whose_process_is_gone() {
    let temp = tempfile::tempdir().unwrap();
    let dir = make_launch_dir(temp.path(), "dead", Some("4242 133700000000000000"));
    let asked = RefCell::new(Vec::new());

    sweep(temp.path(), &|pid, start| {
        asked.borrow_mut().push((pid, start));
        false
    })
    .unwrap();

    assert!(!dir.exists());
    assert_eq!(&*asked.borrow(), &[(4242, 133_700_000_000_000_000)]);
}

#[test]
fn sweep_removes_only_the_secrets_of_a_launch_that_is_still_running() {
    let temp = tempfile::tempdir().unwrap();
    let dir = make_launch_dir(temp.path(), "alive", Some("4242 133700000000000000"));

    sweep(temp.path(), &|pid, start| {
        pid == 4242 && start == 133_700_000_000_000_000
    })
    .unwrap();

    assert!(
        !dir.join("credential").exists(),
        "the credential is no longer refreshed"
    );
    assert!(!dir.join("request.json").exists());
    assert!(dir.is_dir(), "the box still runs in it");
    assert!(dir.join("pid").is_file());
    assert!(dir.join("tmp").is_dir());
}

#[test]
fn sweep_handles_live_and_dead_launches_side_by_side() {
    let temp = tempfile::tempdir().unwrap();
    let alive = make_launch_dir(temp.path(), "alive", Some("100 1"));
    let dead = make_launch_dir(temp.path(), "dead", Some("200 2"));

    sweep(temp.path(), &|pid, _| pid == 100).unwrap();

    assert!(alive.is_dir() && !alive.join("credential").exists());
    assert!(!dead.exists());
}

#[test]
fn sweep_does_not_treat_a_reused_pid_as_the_launch() {
    // The start time is part of the identity: same pid, different creation time.
    let temp = tempfile::tempdir().unwrap();
    let dir = make_launch_dir(temp.path(), "reused", Some("4242 1"));

    sweep(temp.path(), &|pid, start| pid == 4242 && start == 2).unwrap();

    assert!(!dir.exists());
}

// ======================================================= routes (AC 3, 4, 8, 9)
//
// A real HTTP round trip against `launch::router`, with a fake `LaunchHost`
// (temp-dir paths on both platforms), a stub executor and a stub credential
// source. Time is paused: the 3 s early-exit watch, the 1 s poller and the
// refresh timer run on the virtual clock, and the fake host's `now()` follows
// it.

const BASE_TIME: u64 = 1_700_000_000;
const CREDENTIAL_LIFETIME: u64 = 3_600;

const GOOD_CONFIG: &str = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode: {}
"#;

#[derive(Clone)]
struct Clock {
    start: tokio::time::Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
        }
    }

    fn now(&self) -> u64 {
        BASE_TIME + self.start.elapsed().as_secs()
    }
}

/// Successful `read_bytes` calls recorded by `RecordingIntegrityFs`.
type IntegrityReads = Arc<Mutex<Vec<(PathBuf, Vec<u8>)>>>;

/// Delegates to the real integrity filesystem and records every successful
/// `read_bytes`, so a test can see which bytes a verify pass read.
struct RecordingIntegrityFs {
    reads: IntegrityReads,
}

impl integrity::IntegrityFs for RecordingIntegrityFs {
    fn metadata(&self, p: &Path) -> io::Result<integrity::EntryKind> {
        integrity::REAL_FS.metadata(p)
    }

    fn read_bytes(&self, p: &Path) -> io::Result<Vec<u8>> {
        let bytes = integrity::REAL_FS.read_bytes(p)?;
        self.reads
            .lock()
            .unwrap()
            .push((p.to_path_buf(), bytes.clone()));
        Ok(bytes)
    }

    fn read_dir_names(&self, p: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        integrity::REAL_FS.read_dir_names(p)
    }

    fn file_identity(&self, p: &Path) -> io::Result<integrity::FileIdentity> {
        integrity::REAL_FS.file_identity(p)
    }

    fn read_link(&self, p: &Path) -> io::Result<std::ffi::OsString> {
        integrity::REAL_FS.read_link(p)
    }
}

struct FakeHost {
    home: PathBuf,
    state_dir: PathBuf,
    client_exe_dir: PathBuf,
    client_exe_name: String,
    clock: Clock,
    integrity_reads: IntegrityReads,
    managed_file: Option<&'static str>,
    managed_file_error: Option<&'static str>,
    /// `(host, line)` pairs `known_hosts_seed` answers from.
    known_hosts: Vec<(&'static str, &'static str)>,
    /// Real filesystem plus wrap-configured Windows-absolute executables.
    wrap_fs: WrapAwareProgramFs,
    /// The daemon PATH directories native resolution searches (empty unless
    /// the Setup asks for one; the wrap sentinel test plants a native
    /// claude.exe there).
    path_dirs: Vec<PathBuf>,
}

impl LaunchHost for FakeHost {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn git_identity(&self) -> Vec<(String, Vec<String>)> {
        // Deterministic: never read the test machine's git configuration.
        Vec::new()
    }

    fn known_hosts_seed(&self, hosts: &[String]) -> Vec<String> {
        // Deterministic: never read the test machine's known_hosts.
        self.known_hosts
            .iter()
            .filter(|(host, _)| hosts.iter().any(|wanted| wanted == host))
            .map(|(_, line)| (*line).to_owned())
            .collect()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        self.path_dirs.clone()
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        base_env()
    }

    fn cwd_context(&self) -> CwdContext {
        CwdContext {
            home: self.home.clone(),
            state_dir: self.state_dir.clone(),
            system_dirs: vec![self.home.parent().unwrap().join("windows")],
            denied: vec![self.home.join(".ssh")],
            app_data: vec![],
        }
    }

    fn cwd_fs(&self) -> &dyn CwdFs {
        &RealFs
    }

    fn program_fs(&self) -> &dyn ProgramFs {
        &self.wrap_fs
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn git_dir(&self) -> Option<PathBuf> {
        None
    }

    fn program_files_claude(&self) -> Option<PathBuf> {
        None
    }

    fn client_exe_dir(&self) -> PathBuf {
        self.client_exe_dir.clone()
    }

    fn client_exe_name(&self) -> String {
        self.client_exe_name.clone()
    }

    fn now(&self) -> u64 {
        self.clock.now()
    }

    fn integrity_fs(&self) -> Arc<dyn integrity::IntegrityFs> {
        Arc::new(RecordingIntegrityFs {
            reads: self.integrity_reads.clone(),
        })
    }

    fn managed_sources(&self) -> crate::provider::claude_code::managed::ManagedSources {
        use crate::provider::claude_code::managed::ManagedSources;
        ManagedSources {
            hklm: None,
            file: self
                .managed_file_error
                .map(|msg| Err(io::Error::other(msg)))
                .or_else(|| self.managed_file.map(|s| Ok(s.to_owned()))),
            drop_ins: Vec::new(),
        }
    }
}

struct StubCredentials {
    clock: Clock,
    calls: Mutex<Vec<(String, bool)>>,
    failure: Mutex<Option<(StatusCode, String)>>,
    panic_on_call: bool,
    issued: AtomicU32,
}

impl StubCredentials {
    fn calls(&self) -> Vec<(String, bool)> {
        self.calls.lock().unwrap().clone()
    }

    fn fail_with(&self, failure: Option<(StatusCode, &str)>) {
        *self.failure.lock().unwrap() = failure.map(|(status, body)| (status, body.to_owned()));
    }
}

#[async_trait]
impl CredentialSource for StubCredentials {
    async fn credential(
        &self,
        client_id: &str,
        interactive: bool,
    ) -> Result<LlmGatewayCredential, (StatusCode, String)> {
        self.calls
            .lock()
            .unwrap()
            .push((client_id.to_owned(), interactive));
        assert!(
            !self.panic_on_call,
            "a precondition failure must not reach the credential source"
        );
        if let Some(failure) = self.failure.lock().unwrap().clone() {
            return Err(failure);
        }
        let issued = self.issued.fetch_add(1, Ordering::SeqCst);
        Ok(LlmGatewayCredential {
            credential: format!("gateway-token-{issued}"),
            expires_at_unix_seconds: self.clock.now() + CREDENTIAL_LIFETIME,
        })
    }
}

#[derive(Clone, Debug)]
struct SpawnCall {
    launch_id: String,
    executor: PathBuf,
    request: PathBuf,
    log: PathBuf,
    /// `request.json` as it was when the executor was started.
    request_json: Option<String>,
    /// `credential\token` as it was when the executor was started.
    token: Option<String>,
}

struct StubExecutor {
    spawns: Mutex<Vec<SpawnCall>>,
    error: Mutex<Option<io::Error>>,
    /// Written to the log file by `spawn`.
    log_text: Mutex<String>,
    /// The children's exit status; `None` while running.
    exit: Arc<Mutex<Option<i32>>>,
    /// Error kind returned by `try_wait` while set.
    try_wait_error: Arc<Mutex<Option<io::ErrorKind>>>,
    /// Error kind returned by `kill` while set.
    kill_error: Arc<Mutex<Option<io::ErrorKind>>>,
    /// Number of times `kill` was called.
    kills: Arc<AtomicU32>,
    /// When true, `spawn` creates the pid file as a directory.
    pid_as_dir: AtomicBool,
    /// Records whether the launch directory existed when `kill` was called.
    dir_existed_at_kill: Arc<Mutex<Option<bool>>>,
    /// Called at the start of `spawn` if set.
    on_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// When true, `spawn` really executes the request's resolved program
    /// where it exists on this host (the AC-4/AC-7 sentinel test), so a
    /// resolver regression produces a running process, not just a string.
    #[cfg(unix)]
    run_resolved_program: AtomicBool,
}

/// The program of an MXC `commandLine` (the third quoted token: window
/// title, cwd, program), with `win()`'s backslashes mapped back to this
/// host's separators. Executes it when it exists here (the configured
/// `C:\...` wrap path never does) and panics if that execution cannot start
/// or fails: the sentinel assertions rely on execution really happening.
#[cfg(unix)]
fn run_command_line_program(command_line: &str) -> PathBuf {
    let quoted: Vec<&str> = command_line.split('"').skip(1).step_by(2).collect();
    assert!(quoted.len() >= 3, "unexpected commandLine: {command_line}");
    let program = PathBuf::from(quoted[2].replace('\\', "/"));
    if program.exists() {
        let status = std::process::Command::new(&program)
            .status()
            .unwrap_or_else(|error| panic!("execute {}: {error}", program.display()));
        assert!(status.success(), "{} failed", program.display());
    }
    program
}

impl StubExecutor {
    fn spawns(&self) -> Vec<SpawnCall> {
        self.spawns.lock().unwrap().clone()
    }

    fn exit_with(&self, code: i32) {
        *self.exit.lock().unwrap() = Some(code);
    }

    fn kills(&self) -> u32 {
        self.kills.load(Ordering::SeqCst)
    }
}

struct StubChild {
    exit: Arc<Mutex<Option<i32>>>,
    try_wait_error: Arc<Mutex<Option<io::ErrorKind>>>,
    kill_error: Arc<Mutex<Option<io::ErrorKind>>>,
    kills: Arc<AtomicU32>,
    launch_dir: PathBuf,
    dir_existed_at_kill: Arc<Mutex<Option<bool>>>,
}

impl LaunchChild for StubChild {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if let Some(kind) = *self.try_wait_error.lock().unwrap() {
            return Err(io::Error::from(kind));
        }
        Ok(*self.exit.lock().unwrap())
    }

    fn pid(&self) -> u32 {
        4242
    }

    fn start_time(&self) -> u64 {
        987_654_321
    }

    fn kill(&mut self) -> io::Result<()> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        *self.dir_existed_at_kill.lock().unwrap() = Some(self.launch_dir.exists());
        if let Some(kind) = *self.kill_error.lock().unwrap() {
            return Err(io::Error::from(kind));
        }
        Ok(())
    }
}

impl Executor for StubExecutor {
    fn spawn(
        &self,
        launch_id: &str,
        executor: &Path,
        request: &Path,
        log: &Path,
        _console: super::plan::ConsolePlan,
    ) -> io::Result<Box<dyn LaunchChild>> {
        if let Some(callback) = self.on_spawn.lock().unwrap().take() {
            callback();
        }
        if let Some(error) = self.error.lock().unwrap().take() {
            return Err(error);
        }
        let launch_dir = request
            .parent()
            .expect("request.json is inside the launch directory");
        let request_json = std::fs::read_to_string(request).ok();
        #[cfg(unix)]
        if self.run_resolved_program.load(Ordering::SeqCst) {
            let value: serde_json::Value =
                serde_json::from_str(request_json.as_deref().expect("request.json readable"))
                    .expect("request.json parses");
            run_command_line_program(value["process"]["commandLine"].as_str().unwrap());
        }
        self.spawns.lock().unwrap().push(SpawnCall {
            launch_id: launch_id.to_owned(),
            executor: executor.to_owned(),
            request: request.to_owned(),
            log: log.to_owned(),
            request_json,
            token: std::fs::read_to_string(launch_dir.join("credential").join("token")).ok(),
        });
        let text = self.log_text.lock().unwrap().clone();
        if !text.is_empty() {
            std::fs::write(log, text)?;
        }
        if self.pid_as_dir.load(Ordering::SeqCst) {
            std::fs::create_dir(launch_dir.join("pid"))?;
        }
        Ok(Box::new(StubChild {
            exit: self.exit.clone(),
            try_wait_error: self.try_wait_error.clone(),
            kill_error: self.kill_error.clone(),
            kills: self.kills.clone(),
            launch_dir: launch_dir.to_owned(),
            dir_existed_at_kill: self.dir_existed_at_kill.clone(),
        }))
    }
}

enum Availability {
    Available,
    Unavailable(&'static str),
    Missing,
}

struct Setup {
    config: &'static str,
    remote_config: Option<&'static str>,
    system_mode: bool,
    availability: Availability,
    daemon_sid: Option<&'static str>,
    peer: Option<PipePeer>,
    claude_installed: bool,
    credentials_must_not_be_reached: bool,
    managed_file: Option<&'static str>,
    proxy_address: Option<std::net::SocketAddr>,
    session_policy: Arc<dyn crate::llm_proxy::sessions::SessionPolicy>,
    sessions: crate::llm_proxy::sessions::SessionRegistry,
    egress: agentdesktop_core::config::ContainmentEgress,
    managed_file_error: Option<&'static str>,
    git: Option<agentdesktop_core::config::GitConfig>,
    known_hosts: Vec<(&'static str, &'static str)>,
    client_exe_name: Option<&'static str>,
    /// PR 6a, G1: `daemon.claudeCode.config` set at startup.
    claude_settings_overridden: bool,
    /// Creates one real directory under the harness root and serves it as a
    /// daemon PATH directory (`FakeHost::path_dirs`); `Harness::path_dir`
    /// then names it.
    extra_path_dir: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            config: GOOD_CONFIG,
            remote_config: None,
            system_mode: false,
            availability: Availability::Available,
            daemon_sid: None,
            peer: None,
            claude_installed: true,
            credentials_must_not_be_reached: false,
            managed_file: None,
            proxy_address: Some(std::net::SocketAddr::from(([127, 0, 0, 1], 4000))),
            session_policy: Arc::new(crate::llm_proxy::sessions::ConfigPolicy {
                config: serde_yaml::from_str(GOOD_CONFIG).unwrap(),
                state_dir: std::env::temp_dir(),
            }),
            sessions: crate::llm_proxy::sessions::SessionRegistry::default(),
            egress: agentdesktop_core::config::ContainmentEgress::default(),
            managed_file_error: None,
            git: None,
            known_hosts: Vec::new(),
            client_exe_name: None,
            claude_settings_overridden: false,
            extra_path_dir: false,
        }
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    state_dir: PathBuf,
    base: String,
    client: reqwest::Client,
    executor: Arc<StubExecutor>,
    credentials: Arc<StubCredentials>,
    integrity_reads: IntegrityReads,
    #[allow(dead_code)]
    sessions: crate::llm_proxy::sessions::SessionRegistry,
    /// The directory `Setup::extra_path_dir` created, `None` otherwise.
    #[allow(dead_code)]
    path_dir: Option<PathBuf>,
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

/// Keeps two process-lifetime dispatchers registered so tracing-core never
/// enters its single-dispatcher mode, for log-capture tests that run beside
/// launches on other threads.
///
/// With at most one registered dispatcher, tracing-core 0.1 computes a
/// callsite's first-hit interest from the CALLING thread's default
/// subscriber, without the dispatcher lock, and caches it globally. A
/// concurrent launch test with no subscriber could then cache
/// `Interest::never` for a callsite another test is capturing (the
/// `launch_logs_posture_error_as_warning` flake). With two or more
/// dispatchers always registered, callsite registration takes the read lock
/// and sees every live dispatcher, and `set_default` (write lock) re-computes
/// every callsite: the two are serialized, so a capture installed before an
/// event always sees it. Called from `Harness::start`, so every launch
/// happens after the pins exist.
pub(super) fn pin_tracing_dispatchers() {
    static PINS: std::sync::OnceLock<[tracing::Dispatch; 2]> = std::sync::OnceLock::new();
    PINS.get_or_init(|| {
        [
            tracing::Dispatch::new(tracing_subscriber::registry()),
            tracing::Dispatch::new(tracing_subscriber::registry()),
        ]
    });
}

impl Harness {
    async fn start(setup: Setup) -> Self {
        pin_tracing_dispatchers();
        let dir = tempfile::tempdir().unwrap();
        let root = strip_verbatim(std::fs::canonicalize(dir.path()).unwrap());
        let home = root.join("home").join("dev");
        let cwd = home.join("work").join("app");
        let state_dir = root.join("state");
        let client_exe_dir = root.join("bin");
        for directory in [&cwd, &state_dir, &client_exe_dir] {
            std::fs::create_dir_all(directory).unwrap();
        }
        if setup.claude_installed {
            let bin = home.join(".local").join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("claude.exe"), b"MZ").unwrap();
        }
        let path_dir = setup.extra_path_dir.then(|| {
            let tools = root.join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            tools
        });
        if let Some(remote) = setup.remote_config {
            std::fs::write(state_dir.join("remote-config.yaml"), remote).unwrap();
        }

        let clock = Clock::new();
        let credentials = Arc::new(StubCredentials {
            clock: clock.clone(),
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
            panic_on_call: setup.credentials_must_not_be_reached,
            issued: AtomicU32::new(0),
        });
        let executor = Arc::new(StubExecutor {
            spawns: Mutex::new(Vec::new()),
            error: Mutex::new(None),
            log_text: Mutex::new(String::new()),
            exit: Arc::new(Mutex::new(None)),
            try_wait_error: Arc::new(Mutex::new(None)),
            kill_error: Arc::new(Mutex::new(None)),
            kills: Arc::new(AtomicU32::new(0)),
            pid_as_dir: AtomicBool::new(false),
            dir_existed_at_kill: Arc::new(Mutex::new(None)),
            on_spawn: Mutex::new(None),
            #[cfg(unix)]
            run_resolved_program: AtomicBool::new(false),
        });
        let executor_path = client_exe_dir.join("wxc-exec.exe");
        let availability = match setup.availability {
            Availability::Available => Some(ContainmentInfo {
                executor: executor_path.display().to_string(),
                engine: "mxc".to_owned(),
                available: true,
                error: None,
                managed_settings_source: None,
                managed_hooks_locked: false,
                managed_mcp_locked: false,
                managed_settings_error: None,
                programs: BTreeMap::new(),
            }),
            Availability::Unavailable(error) => Some(ContainmentInfo {
                executor: executor_path.display().to_string(),
                engine: "mxc".to_owned(),
                available: false,
                error: Some(error.to_owned()),
                managed_settings_source: None,
                managed_hooks_locked: false,
                managed_mcp_locked: false,
                managed_settings_error: None,
                programs: BTreeMap::new(),
            }),
            Availability::Missing => None,
        };

        let daemon_info: agentdesktop_core::model::DaemonInfo =
            serde_json::from_value(serde_json::json!({
                "version": "test",
                "scope": if setup.system_mode { "system" } else { "user" },
                "configPath": "",
                "stateDirectory": "",
                "inventoryInterval": "60s",
                "controller": null,
            }))
            .unwrap();
        let (_discovery_sender, discovery) =
            tokio::sync::watch::channel(Arc::new(agentdesktop_core::model::Discovery {
                agents: vec![],
                model_runtimes: vec![],
            }));
        let app = api::AppState {
            config: agentdesktop_core::config::parse_daemon(setup.config)
                .expect("test configuration"),
            daemon_info,
            discovery,
            enrollment: crate::enrollment::EnrollmentState::new(false),
            controller_status: None,
            state_dir: state_dir.clone(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            violation_ledger: None,
        };
        let integrity_reads: IntegrityReads = Arc::new(Mutex::new(Vec::new()));
        let state = LaunchState {
            app,
            executor: executor.clone(),
            credentials: credentials.clone(),
            host: Arc::new(FakeHost {
                home: home.clone(),
                state_dir: state_dir.clone(),
                client_exe_dir,
                client_exe_name: setup
                    .client_exe_name
                    .map(str::to_owned)
                    .unwrap_or_else(|| "agentdesktop".to_owned()),
                clock: clock.clone(),
                integrity_reads: integrity_reads.clone(),
                managed_file: setup.managed_file,
                managed_file_error: setup.managed_file_error,
                known_hosts: setup.known_hosts,
                wrap_fs: WrapAwareProgramFs { root: root.clone() },
                path_dirs: path_dir.iter().cloned().collect(),
            }),
            registry: LaunchRegistry::new(),
            availability,
            daemon_sid: setup.daemon_sid.map(str::to_owned),
            sessions: setup.sessions.clone(),
            proxy_address: setup.proxy_address,
            session_policy: setup.session_policy.clone(),
            egress: setup.egress,
            git: setup.git,
            engine: Arc::new(engine::mxc::MxcEngine),
            claude_settings_overridden: setup.claude_settings_overridden,
        };

        let mut router = router(state);
        if let Some(peer) = setup.peer {
            router = router.layer(axum::Extension(peer));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        Self {
            _dir: dir,
            cwd,
            home,
            state_dir,
            base: format!("http://{address}"),
            client: reqwest::Client::new(),
            executor,
            credentials,
            integrity_reads,
            sessions: setup.sessions,
            path_dir,
        }
    }

    fn body(&self) -> serde_json::Value {
        serde_json::to_value(LaunchRequest {
            program: "claude-code".into(),
            cwd: self.cwd.clone(),
            args: vec![],
            console: None,
        })
        .unwrap()
    }

    fn body_with(&self, change: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
        let mut body = self.body();
        change(&mut body);
        body
    }

    async fn launch(&self, body: &serde_json::Value) -> (u16, String) {
        let response = self
            .client
            .post(format!("{}/v1/launch", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn launches(&self) -> (u16, String) {
        let response = self
            .client
            .get(format!("{}/v1/launches", self.base))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn listed(&self) -> Vec<Launch> {
        let (status, body) = self.launches().await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn launch_ok(&self) -> Launch {
        let (status, body) = self.launch(&self.body()).await;
        assert_eq!(status, 201, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    fn launch_dir(&self, launch_id: &str) -> PathBuf {
        self.state_dir.join("launches").join(launch_id)
    }

    fn token(&self, launch_id: &str) -> Option<String> {
        std::fs::read_to_string(self.launch_dir(launch_id).join("credential").join("token")).ok()
    }

    /// Nothing was written or started for a refused request.
    fn assert_untouched(&self) {
        assert!(self.executor.spawns().is_empty(), "nothing is spawned");
        let launches = self.state_dir.join("launches");
        assert!(
            !launches.exists() || std::fs::read_dir(&launches).unwrap().next().is_none(),
            "no launch directory is created"
        );
    }
}

fn is_launch_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// ----------------------------------------------------- check order and statuses

#[tokio::test(start_paused = true)]
async fn system_mode_answers_409_on_both_routes_before_anything_else() {
    let h = Harness::start(Setup {
        system_mode: true,
        availability: Availability::Missing,
        daemon_sid: Some("S-1-5-21-1"),
        peer: Some(peer(Some("S-1-5-21-2"), true)),
        ..Setup::default()
    })
    .await;

    let (status, body) = h
        .launch(&h.body_with(|body| body["program"] = "codex".into()))
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("containment needs the user-mode daemon"),
        "{body}"
    );
    let (status, body) = h.launches().await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("containment needs the user-mode daemon"),
        "{body}"
    );
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn a_peer_of_another_user_is_refused_with_403_on_both_routes() {
    let h = Harness::start(Setup {
        daemon_sid: Some("S-1-5-21-1"),
        peer: Some(peer(Some("S-1-5-21-2"), false)),
        ..Setup::default()
    })
    .await;

    let (status, body) = h.launch(&h.body()).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body, "launch is available to the daemon's own user only");
    let (status, body) = h.launches().await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body, "launch is available to the daemon's own user only");
    h.assert_untouched();
    assert!(h.credentials.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_app_container_peer_of_the_same_user_is_refused_with_403() {
    let h = Harness::start(Setup {
        daemon_sid: Some("S-1-5-21-1"),
        peer: Some(peer(Some("S-1-5-21-1"), true)),
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launch(&h.body()).await.0, 403);
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn a_request_without_a_peer_extension_is_refused_when_the_daemon_has_a_sid() {
    let h = Harness::start(Setup {
        daemon_sid: Some("S-1-5-21-1"),
        peer: None,
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launches().await.0, 403);
    assert_eq!(h.launch(&h.body()).await.0, 403);
}

#[tokio::test(start_paused = true)]
async fn the_daemons_own_user_outside_an_app_container_is_served() {
    let h = Harness::start(Setup {
        daemon_sid: Some("S-1-5-21-1"),
        peer: Some(peer(Some("S-1-5-21-1"), false)),
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launches().await, (200, "[]".to_owned()));
    assert_eq!(h.launch(&h.body()).await.0, 201);
}

#[tokio::test(start_paused = true)]
async fn the_peer_check_comes_before_the_availability_check() {
    let h = Harness::start(Setup {
        availability: Availability::Missing,
        daemon_sid: Some("S-1-5-21-1"),
        peer: Some(peer(Some("S-1-5-21-2"), false)),
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launch(&h.body()).await.0, 403);
}

#[tokio::test(start_paused = true)]
async fn a_daemon_without_a_sid_does_not_check_the_peer() {
    // Unix user mode: the socket is 0600 and the accept loop attaches no peer.
    let h = Harness::start(Setup::default()).await;
    assert_eq!(h.launches().await.0, 200);
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_containment_answers_409_with_its_error() {
    let h = Harness::start(Setup {
        availability: Availability::Unavailable("executor not found: C:\\missing\\wxc-exec.exe"),
        ..Setup::default()
    })
    .await;
    let (status, body) = h
        .launch(&h.body_with(|body| body["program"] = "codex".into()))
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("executor not found: C:\\missing\\wxc-exec.exe"),
        "{body}"
    );
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn an_unconfigured_containment_answers_409() {
    let h = Harness::start(Setup {
        availability: Availability::Missing,
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launch(&h.body()).await.0, 409);
    h.assert_untouched();
}

// PR 6a (G6, contract): the unknown-program text lists the three programs.
#[tokio::test(start_paused = true)]
async fn only_the_three_profiled_programs_can_be_launched() {
    let h = Harness::start(Setup::default()).await;
    for program in ["codex", "claude", ""] {
        let (status, body) = h
            .launch(&h.body_with(|body| body["program"] = program.into()))
            .await;
        assert_eq!(status, 400, "{program}: {body}");
        assert_eq!(
            body, "program must be one of claude-code, opencode, copilot",
            "{program}"
        );
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn the_program_is_checked_before_the_workspace() {
    let h = Harness::start(Setup::default()).await;
    let body = h.body_with(|body| {
        body["program"] = "codex".into();
        body["cwd"] = h.home.display().to_string().into();
    });
    assert_eq!(
        h.launch(&body).await,
        (
            400,
            "program must be one of claude-code, opencode, copilot".to_owned()
        )
    );
}

#[tokio::test(start_paused = true)]
async fn a_workspace_that_breaks_the_rules_answers_400() {
    let h = Harness::start(Setup::default()).await;
    let with_percent = h.cwd.parent().unwrap().join("50%off");
    std::fs::create_dir_all(&with_percent).unwrap();
    std::fs::create_dir_all(h.home.join(".claude")).unwrap();
    for cwd in [
        h.home.clone(),
        h.home.join(".claude"),
        h.home.join("missing-directory"),
        with_percent,
        h.state_dir.clone(),
    ] {
        let (status, body) = h
            .launch(&h.body_with(|body| body["cwd"] = cwd.display().to_string().into()))
            .await;
        assert_eq!(status, 400, "{}: {body}", cwd.display());
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn the_workspace_is_checked_before_the_arguments() {
    let h = Harness::start(Setup::default()).await;
    let body = h.body_with(|body| {
        body["cwd"] = h.home.display().to_string().into();
        body["args"] = serde_json::json!(["a&b"]);
    });
    let (status, text) = h.launch(&body).await;
    assert_eq!(status, 400);
    assert_ne!(text, "argument contains a character cmd.exe interprets");
}

#[tokio::test(start_paused = true)]
async fn an_argument_cmd_exe_would_interpret_answers_400() {
    let h = Harness::start(Setup::default()).await;
    for argument in ["a&b", "50%", "x|y", "say \"hi\"", "!x", "a\nb"] {
        let (status, body) = h
            .launch(&h.body_with(|body| body["args"] = serde_json::json!([argument])))
            .await;
        assert_eq!(status, 400, "{argument:?}: {body}");
        assert_eq!(
            body, "argument contains a character cmd.exe interprets",
            "{argument:?}"
        );
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn the_arguments_are_checked_before_the_program_is_resolved() {
    let h = Harness::start(Setup {
        claude_installed: false,
        ..Setup::default()
    })
    .await;
    let (status, body) = h
        .launch(&h.body_with(|body| body["args"] = serde_json::json!(["a&b"])))
        .await;
    assert_eq!(
        (status, body.as_str()),
        (400, "argument contains a character cmd.exe interprets")
    );
}

#[tokio::test(start_paused = true)]
async fn a_missing_native_install_answers_400() {
    let h = Harness::start(Setup {
        claude_installed: false,
        ..Setup::default()
    })
    .await;
    let (status, body) = h.launch(&h.body()).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("Claude Code native install (claude.exe) not found"),
        "{body}"
    );
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_applied_configuration_answers_500() {
    let h = Harness::start(Setup {
        config: "controller:\n  address: https://controller.example.com\n",
        remote_config: Some("llmGateway: [this is not a gateway"),
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;
    let (status, body) = h.launch(&h.body()).await;
    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("read applied configuration: "), "{body}");
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn the_configuration_is_read_before_the_program_is_resolved() {
    // Wrap mode resolves the configured executable from the applied
    // configuration, so the handler reads the configuration first and a
    // config read failure (500) precedes a native not-found (400).
    let h = Harness::start(Setup {
        config: "controller:\n  address: https://controller.example.com\n",
        remote_config: Some("llmGateway: [this is not a gateway"),
        claude_installed: false,
        ..Setup::default()
    })
    .await;
    let (status, body) = h.launch(&h.body()).await;
    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("read applied configuration: "), "{body}");
}

// ------------------------------------------------------------ 409 preconditions

async fn precondition(config: &'static str) -> (u16, String) {
    let h = Harness::start(Setup {
        config,
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;
    let answer = h.launch(&h.body()).await;
    assert!(
        h.credentials.calls().is_empty(),
        "the credential source is not reached"
    );
    h.assert_untouched();
    answer
}

#[tokio::test(start_paused = true)]
async fn a_gateway_without_authentication_answers_409() {
    let (status, body) = precondition(
        "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  claudeCode: {}\n",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("llmGateway") && body.contains("authentication"),
        "{body}"
    );
}

#[tokio::test(start_paused = true)]
async fn no_gateway_at_all_answers_409() {
    let (status, body) = precondition("programs:\n  claudeCode: {}\n").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("llmGateway"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn a_configuration_without_claude_code_answers_409() {
    let (status, body) = precondition(
        "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\n",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("programs.claudeCode"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn claude_code_that_does_not_use_the_gateway_answers_409() {
    let (status, body) = precondition(
        "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  claudeCode:\n    useLlmGateway: false\n",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("useLlmGateway"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn subscription_sign_in_answers_409() {
    let (status, body) = precondition(
        "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  claudeCode:\n    auth: subscription\n",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("subscription sign-in is not available inside the box in PR 1"),
        "{body}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_top_level_sandbox_answers_409() {
    let (status, body) = precondition(
        "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  claudeCode: {}\nsandbox: {}\n",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("Claude Code's own sandbox is not available inside the box"),
        "{body}"
    );
}

// ------------------------------------------------------------------- credential

#[tokio::test(start_paused = true)]
async fn a_credential_error_passes_through_and_nothing_is_started_or_written() {
    for (status, text) in [
        (StatusCode::UNAUTHORIZED, "LLM gateway sign-in required"),
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no LLM gateway configured",
        ),
        (StatusCode::BAD_GATEWAY, "upstream refused"),
    ] {
        let h = Harness::start(Setup::default()).await;
        h.credentials.fail_with(Some((status, text)));

        let (answered, body) = h.launch(&h.body()).await;

        assert_eq!(answered, status.as_u16(), "{body}");
        assert_eq!(body, text);
        h.assert_untouched();
        assert!(h.listed().await.is_empty());
    }
}

// ---------------------------------------------------------------- 201 and files

#[tokio::test(start_paused = true)]
async fn a_launch_answers_201_with_the_launch_and_lists_it() {
    let h = Harness::start(Setup::default()).await;

    let launched = h.launch_ok().await;

    assert!(is_launch_id(&launched.launch_id), "{}", launched.launch_id);
    assert_eq!(launched.program, "claude-code");
    assert_eq!(launched.cwd, h.cwd);
    assert_eq!(
        launched.container_id,
        format!("agentdesktop-{}", launched.launch_id)
    );
    assert_eq!(launched.status, LaunchStatus::Running);
    assert_eq!(launched.exit_code, None);
    assert_eq!(launched.ended_at_unix_seconds, None);
    assert!((BASE_TIME..BASE_TIME + 10).contains(&launched.started_at_unix_seconds));
    assert!(!launched.session_id.is_empty(), "session_id present");
    assert_eq!(h.listed().await, vec![launched]);
}

#[tokio::test(start_paused = true)]
async fn the_handler_asks_for_an_interactive_credential_for_claude_code() {
    let h = Harness::start(Setup::default()).await;
    h.launch_ok().await;
    assert_eq!(
        h.credentials.calls(),
        vec![("claude-code".to_owned(), true)]
    );
}

// PR 6 (AC-8a, D1, D8, N13): with a wrap entry, the LaunchRequest written
// for the box carries the configured executable, not the native resolution.
const WRAP_CLAUDE_CONFIG: &str = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode: {}
containment:
  programs:
    claudeCode:
      executable: 'C:\Program Files\Anthropic\ClaudeCode\claude.exe'
"#;

#[tokio::test(start_paused = true)]
async fn a_wrap_entry_launch_uses_the_configured_executable_not_the_native_resolution() {
    let h = Harness::start(Setup {
        config: WRAP_CLAUDE_CONFIG,
        ..Setup::default()
    })
    .await;

    let launched = h.launch_ok().await;

    let spawns = h.executor.spawns();
    let request: serde_json::Value =
        serde_json::from_str(spawns[0].request_json.as_deref().unwrap()).unwrap();
    let command_line = request["process"]["commandLine"].as_str().unwrap();
    assert!(
        command_line.contains(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe"),
        "wrap mode must launch the configured executable, not ~\\.local\\bin\\claude.exe: {command_line}"
    );
    assert!(!launched.launch_id.is_empty());
}

/// Proves the sentinel harness below can fail: `run_command_line_program`
/// really executes a program that exists on this host, so a resolver
/// regression that puts the sentinel into the command line leaves a marker.
#[cfg(unix)]
#[test]
fn the_sentinel_harness_executor_really_executes_an_existing_program() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let exe = dir.path().join("claude.exe");
    std::fs::write(&exe, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command_line = format!(
        "cmd.exe /d /v:off /c start \"agentdesktop: claude\" /WAIT /D \"{}\" \"{}\"",
        dir.path().display(),
        exe.display().to_string().replace('/', "\\")
    );
    let program = run_command_line_program(&command_line);
    assert_eq!(program, exe);
    assert!(marker.exists(), "the program was not executed");
}

/// AC-4/AC-7 sentinel, falsifiable (fix round 2; adversarial m9 /
/// conformance m4 said the previous sentinel could not fail): a native
/// `claude.exe` sits exactly where a regressed wrap resolver would find it,
/// in the fake home's `.local/bin` AND in a daemon PATH directory, and the
/// harness executor really executes whatever program lands in the spawned
/// request. In wrap mode only the configured `C:\...` executable (which
/// does not exist on this host) may appear there, so the sentinel never
/// runs and its marker never appears. Verified falsifiable by temporarily
/// forcing the handler's native-resolution arm: the sentinel then resolves,
/// runs, and this test fails first on the command-line assertion below (the
/// marker assertion fails on its own once the earlier asserts are removed;
/// that change is not committed).
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_wrap_launch_never_resolves_or_runs_the_sentinel_native_claude() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::start(Setup {
        config: WRAP_CLAUDE_CONFIG,
        claude_installed: false,
        extra_path_dir: true,
        ..Setup::default()
    })
    .await;
    h.executor
        .run_resolved_program
        .store(true, Ordering::SeqCst);

    let marker = h.home.join("sentinel-ran");
    let bin = h.home.join(".local").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for dir in [&bin, h.path_dir.as_ref().unwrap()] {
        let sentinel = dir.join("claude.exe");
        std::fs::write(
            &sentinel,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.launch_ok().await;

    let spawns = h.executor.spawns();
    assert_eq!(spawns.len(), 1);
    let request: serde_json::Value =
        serde_json::from_str(spawns[0].request_json.as_deref().unwrap()).unwrap();
    let command_line = request["process"]["commandLine"].as_str().unwrap();
    assert!(
        command_line.contains(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe"),
        "{command_line}"
    );
    assert!(
        !command_line.contains(".local") && !command_line.contains("tools"),
        "the sentinel path leaked into the command line: {command_line}"
    );
    assert!(
        !marker.exists(),
        "the sentinel native claude.exe was resolved and executed"
    );
}

#[tokio::test(start_paused = true)]
async fn the_credential_and_request_files_exist_before_the_executor_starts() {
    let h = Harness::start(Setup::default()).await;

    let launched = h.launch_ok().await;

    let spawns = h.executor.spawns();
    assert_eq!(spawns.len(), 1);
    let spawn = &spawns[0];
    let dir = h.launch_dir(&launched.launch_id);
    assert_eq!(spawn.launch_id, launched.launch_id);
    assert_eq!(spawn.request, dir.join("request.json"));
    assert_eq!(spawn.log, dir.join("wxc-exec.log"));
    assert_eq!(spawn.token, None, "nothing is stored for the box");
    let request: serde_json::Value =
        serde_json::from_str(spawn.request_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        request["containerId"],
        format!("agentdesktop-{}", launched.launch_id)
    );
    let env = env_map(&request);
    assert!(
        env["ANTHROPIC_BASE_URL"].ends_with("/sandbox"),
        "the box talks to the relay: {env:?}"
    );
    assert!(
        env["ANTHROPIC_CUSTOM_HEADERS"].starts_with("x-agentdesktop-pairing: "),
        "{env:?}"
    );
    assert!(!env.contains_key("AGENTDESKTOP_CREDENTIAL_FILE"), "{env:?}");
    assert!(
        !request.to_string().contains("gateway-token"),
        "the request holds no secret"
    );
    assert_eq!(
        spawn.executor.display().to_string(),
        h.executor_path(),
        "the configured executor is started"
    );
}

impl Harness {
    fn executor_path(&self) -> String {
        self.state_dir
            .parent()
            .unwrap()
            .join("bin")
            .join("wxc-exec.exe")
            .display()
            .to_string()
    }
}

#[tokio::test(start_paused = true)]
async fn the_pid_file_names_the_process_and_its_start_time() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;
    assert_eq!(
        std::fs::read_to_string(h.launch_dir(&launched.launch_id).join("pid")).unwrap(),
        "4242 987654321"
    );
    assert_eq!(h.token(&launched.launch_id), None, "nothing is stored");
    assert_eq!(
        h.credentials.calls(),
        vec![("claude-code".to_owned(), true)],
        "the sign-in pre-flight ran"
    );
}

#[tokio::test(start_paused = true)]
async fn launches_are_listed_by_start_time_ascending() {
    let h = Harness::start(Setup::default()).await;
    let first = h.launch_ok().await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    let second = h.launch_ok().await;
    assert_ne!(first.launch_id, second.launch_id);
    assert!(second.started_at_unix_seconds > first.started_at_unix_seconds);
    assert_eq!(h.listed().await, vec![first, second]);
}

#[tokio::test(start_paused = true)]
async fn the_list_is_empty_before_any_launch() {
    let h = Harness::start(Setup::default()).await;
    assert_eq!(h.launches().await, (200, "[]".to_owned()));
}

// ----------------------------------------------------------------- spawn errors

#[tokio::test(start_paused = true)]
async fn a_spawn_failure_answers_500_with_the_os_error() {
    let h = Harness::start(Setup::default()).await;
    *h.executor.error.lock().unwrap() =
        Some(io::Error::new(io::ErrorKind::NotFound, "no such file"));

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("start wxc-exec: "), "{body}");
    assert!(body.contains("no such file"), "{body}");
    assert!(
        h.listed().await.is_empty(),
        "a launch that never started is not listed"
    );
}

#[tokio::test(start_paused = true)]
async fn an_exit_within_three_seconds_answers_500_with_the_last_4_kib_of_the_log() {
    let h = Harness::start(Setup::default()).await;
    *h.executor.log_text.lock().unwrap() = format!("HEAD-MARKER{}TAIL-MARKER", "x".repeat(8_000));
    h.executor.exit_with(2);

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 500, "{body}");
    assert!(body.contains("TAIL-MARKER"), "{body}");
    assert!(
        !body.contains("HEAD-MARKER"),
        "only the last 4 KiB are returned"
    );
    assert!(body.len() < 5_000, "{}", body.len());
}

// ---------------------------------------------------------- lifecycle and refresh

#[tokio::test(start_paused = true)]
async fn the_poller_marks_an_exit_stops_the_refresh_and_removes_the_launch_directory() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;
    assert!(h.launch_dir(&launched.launch_id).is_dir());

    h.executor.exit_with(7);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let listed = h.listed().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, Some(7));
    let ended = listed[0].ended_at_unix_seconds.expect("an end time");
    assert!(ended >= listed[0].started_at_unix_seconds);
    assert!(!h.launch_dir(&launched.launch_id).exists());
    let now = h.credentials.clock.now();
    assert_eq!(h.sessions.len(now), 0, "session revoked on exit");

    tokio::time::sleep(Duration::from_secs(2 * CREDENTIAL_LIFETIME)).await;
    assert_eq!(
        h.credentials.calls().len(),
        1,
        "only the sign-in pre-flight reached the credentials"
    );
}

// -------------------------------------------------------------------- body errors

#[tokio::test(start_paused = true)]
async fn body_errors_are_the_ones_axum_produces() {
    let h = Harness::start(Setup::default()).await;
    let url = format!("{}/v1/launch", h.base);

    let not_json = h
        .client
        .post(&url)
        .header("content-type", "text/plain")
        .body("program=claude-code")
        .send()
        .await
        .unwrap();
    assert_eq!(not_json.status().as_u16(), 415);

    let malformed = h
        .client
        .post(&url)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status().as_u16(), 400);

    let missing_cwd = h
        .client
        .post(&url)
        .json(&serde_json::json!({"program": "claude-code", "args": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_cwd.status().as_u16(), 422);
    h.assert_untouched();
}

// -------------------------------------------------------- stopping the executor

#[tokio::test(start_paused = true)]
async fn the_executor_is_stopped_when_the_pid_file_cannot_be_written() {
    let h = Harness::start(Setup::default()).await;
    h.executor.pid_as_dir.store(true, Ordering::SeqCst);

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("record the launch pid: "), "{body}");
    assert_eq!(h.executor.kills(), 1);
    assert!(
        h.listed().await.is_empty(),
        "a launch that never started is not listed"
    );
    let launches = h.state_dir.join("launches");
    assert!(
        !launches.exists() || std::fs::read_dir(&launches).unwrap().next().is_none(),
        "no launch directory remains"
    );
}

#[tokio::test(start_paused = true)]
async fn the_executor_is_stopped_when_the_startup_watch_fails() {
    let h = Harness::start(Setup::default()).await;
    *h.executor.try_wait_error.lock().unwrap() = Some(io::ErrorKind::Other);

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("watch wxc-exec: "), "{body}");
    assert_eq!(h.executor.kills(), 1);
    assert!(
        h.listed().await.is_empty(),
        "a launch that never started is not listed"
    );
    let launches = h.state_dir.join("launches");
    assert!(
        !launches.exists() || std::fs::read_dir(&launches).unwrap().next().is_none(),
        "no launch directory remains"
    );
}

#[tokio::test(start_paused = true)]
async fn a_kill_error_does_not_replace_the_original_error() {
    let h = Harness::start(Setup::default()).await;
    *h.executor.try_wait_error.lock().unwrap() = Some(io::ErrorKind::Other);
    *h.executor.kill_error.lock().unwrap() = Some(io::ErrorKind::PermissionDenied);

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 500, "{body}");
    assert!(body.starts_with("watch wxc-exec: "), "{body}");
    assert!(body.contains("other error"), "{body}");
    assert!(!body.to_lowercase().contains("permission"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn the_executor_is_stopped_when_the_poller_loses_track_of_it() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;

    *h.executor.try_wait_error.lock().unwrap() = Some(io::ErrorKind::Other);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let listed = h.listed().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, None);
    assert_eq!(h.executor.kills(), 1);
    assert!(
        h.executor.dir_existed_at_kill.lock().unwrap().unwrap(),
        "directory must exist when kill is called"
    );
    assert!(!h.launch_dir(&launched.launch_id).exists());
}

#[tokio::test(start_paused = true)]
async fn a_kill_error_does_not_prevent_marking_the_launch_exited() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;

    *h.executor.try_wait_error.lock().unwrap() = Some(io::ErrorKind::Other);
    *h.executor.kill_error.lock().unwrap() = Some(io::ErrorKind::PermissionDenied);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let listed = h.listed().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, None);
    assert!(
        h.executor.dir_existed_at_kill.lock().unwrap().unwrap(),
        "directory must exist when kill is called"
    );
    assert!(!h.launch_dir(&launched.launch_id).exists());
}

#[tokio::test(start_paused = true)]
async fn the_executor_is_not_stopped_after_spawn_failure_early_exit_or_normal_exit() {
    let h = Harness::start(Setup::default()).await;

    // Spawn failure
    *h.executor.error.lock().unwrap() =
        Some(io::Error::new(io::ErrorKind::NotFound, "no such file"));
    h.launch(&h.body()).await;
    assert_eq!(h.executor.kills(), 0, "no kill after spawn failure");

    // Early exit
    *h.executor.error.lock().unwrap() = None;
    h.executor.exit_with(2);
    let (status, _) = h.launch(&h.body()).await;
    assert_eq!(status, 500, "early exit returns 500");
    assert_eq!(h.executor.kills(), 0, "no kill after early exit");

    // Normal exit
    *h.executor.exit.lock().unwrap() = None;
    let launched = h.launch_ok().await;
    h.executor.exit_with(7);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let listed = h.listed().await;
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert_eq!(listed[0].exit_code, Some(7));
    assert!(!h.launch_dir(&launched.launch_id).exists());
    assert_eq!(h.executor.kills(), 0, "no kill after normal exit");
}

// AC 1: integrity check integration tests

#[tokio::test(start_paused = true)]
async fn the_periodic_check_sets_the_drift_flag_while_the_box_runs() {
    let h = Harness::start(Setup::default()).await;
    std::fs::write(h.cwd.join(".mcp.json"), b"{}").unwrap();
    let launched = h.launch_ok().await;

    // No drift at launch.
    assert!(!h.listed().await[0].integrity_drift);

    std::fs::write(h.cwd.join(".mcp.json"), b"{\"changed\":true}").unwrap();

    // Past the first 60 s tick; the periodic check must flag the running launch.
    tokio::time::sleep(Duration::from_secs(70)).await;
    let mut flagged = false;
    for _ in 0..1000 {
        let listed = h.listed().await;
        if listed[0].integrity_drift {
            flagged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(flagged, "the periodic check flags a running launch");
    assert_eq!(h.listed().await[0].status, LaunchStatus::Running);

    h.executor.exit_with(0);
    let _ = launched;
}

#[tokio::test(start_paused = true)]
async fn the_periodic_check_stops_when_the_box_exits() {
    let h = Harness::start(Setup::default()).await;
    std::fs::write(h.cwd.join(".mcp.json"), b"{}").unwrap();
    let _launched = h.launch_ok().await;
    h.executor.exit_with(0);

    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if h.listed().await[0].status == LaunchStatus::Exited {
            break;
        }
    }
    assert_eq!(h.listed().await[0].status, LaunchStatus::Exited);
    assert!(!h.listed().await[0].integrity_drift, "clean exit, no drift");

    // A change after the exit must not be flagged: the periodic task is gone.
    std::fs::write(h.cwd.join(".mcp.json"), b"{\"changed\":true}").unwrap();
    tokio::time::sleep(Duration::from_secs(130)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        !h.listed().await[0].integrity_drift,
        "no drift after the launch exited"
    );
}

#[tokio::test(start_paused = true)]
async fn integrity_drift_detected_when_file_modified_during_launch() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;

    // Modify a protected file
    let mcp_path = launched.cwd.join(".mcp.json");
    std::fs::write(&mcp_path, b"{\"modified\":true}").unwrap();

    // Wait for periodic check (60s + margin)
    tokio::time::sleep(Duration::from_secs(80)).await;

    // Exit the launch
    h.executor.exit_with(0);

    // Wait for launch to be marked exited
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let listed = h.listed().await;
        if listed[0].status == LaunchStatus::Exited {
            break;
        }
    }

    let listed = h.listed().await;
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert!(
        listed[0].integrity_drift,
        "integrity drift should be detected"
    );
}

#[tokio::test(start_paused = true)]
async fn integrity_drift_detected_when_file_replaced_before_spawn() {
    let h = Harness::start(Setup::default()).await;
    std::fs::write(h.cwd.join(".mcp.json"), b"{}").unwrap();

    // The snapshot is taken before `spawn`; replacing the file inside `spawn`
    // must therefore show up as drift (this fails if the snapshot moved after).
    let mcp = h.cwd.join(".mcp.json");
    *h.executor.on_spawn.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(&mcp, b"{\"replaced\":true}").unwrap();
    }));

    let _launched = h.launch_ok().await;
    h.executor.exit_with(0);

    // Wait for launch to be marked exited
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let listed = h.listed().await;
        if listed[0].status == LaunchStatus::Exited {
            break;
        }
    }

    let listed = h.listed().await;
    assert_eq!(listed[0].status, LaunchStatus::Exited);
    assert!(
        listed[0].integrity_drift,
        "drift should be detected even if changed during spawn"
    );
}

#[tokio::test(start_paused = true)]
async fn integrity_drift_on_early_exit_verify() {
    let h = Harness::start(Setup::default()).await;
    std::fs::write(h.cwd.join(".mcp.json"), b"{}").unwrap();

    let mcp = h.cwd.join(".mcp.json");
    *h.executor.on_spawn.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(&mcp, b"{\"replaced\":true}").unwrap();
    }));
    h.executor.exit_with(1);

    let (status, _) = h.launch(&h.body()).await;
    assert_eq!(status, 500, "early exit returns 500");

    // An early exit leaves no launch record, so the verify pass is observed
    // through the recording integrity filesystem: the pre-spawn snapshot read
    // the original bytes, and the early-exit verify re-read the replaced ones.
    let reads = h.integrity_reads.lock().unwrap();
    let mcp_reads: Vec<&[u8]> = reads
        .iter()
        .filter(|(path, _)| path.ends_with(".mcp.json"))
        .map(|(_, bytes)| bytes.as_slice())
        .collect();
    assert!(
        mcp_reads.contains(&b"{}".as_slice()),
        "the snapshot before spawn read the original file: {mcp_reads:?}"
    );
    assert!(
        mcp_reads.contains(&b"{\"replaced\":true}".as_slice()),
        "the early-exit verify re-read the replaced file: {mcp_reads:?}"
    );
}

// Per-launch managed-settings snapshot tests

#[tokio::test(start_paused = true)]
async fn launch_records_managed_settings_source_and_locks_when_present() {
    let h = Harness::start(Setup {
        managed_file: Some(r#"{"allowManagedHooksOnly":true}"#),
        ..Setup::default()
    })
    .await;

    let launched = h.launch_ok().await;
    assert_eq!(
        launched.managed_settings_source,
        Some(agentdesktop_core::model::ManagedSettingsSource::File)
    );
    assert!(launched.managed_hooks_locked);
    assert!(!launched.managed_mcp_locked);

    let listed = h.listed().await;
    assert_eq!(
        listed[0].managed_settings_source,
        Some(agentdesktop_core::model::ManagedSettingsSource::File)
    );
    assert!(listed[0].managed_hooks_locked);
    assert!(!listed[0].managed_mcp_locked);
}

#[tokio::test(start_paused = true)]
async fn launch_records_null_source_and_false_locks_when_managed_file_absent() {
    let h = Harness::start(Setup {
        managed_file: None,
        ..Setup::default()
    })
    .await;

    let launched = h.launch_ok().await;
    assert_eq!(launched.managed_settings_source, None);
    assert!(!launched.managed_hooks_locked);
    assert!(!launched.managed_mcp_locked);

    let listed = h.listed().await;
    assert_eq!(listed[0].managed_settings_source, None);
    assert!(!listed[0].managed_hooks_locked);
    assert!(!listed[0].managed_mcp_locked);
}

#[tokio::test(start_paused = true)]
async fn launch_records_null_source_and_false_locks_when_posture_has_error() {
    let h = Harness::start(Setup {
        managed_file_error: Some("access denied"),
        ..Setup::default()
    })
    .await;

    let launched = h.launch_ok().await;
    assert_eq!(launched.managed_settings_source, None);
    assert!(!launched.managed_hooks_locked);
    assert!(!launched.managed_mcp_locked);

    let listed = h.listed().await;
    assert_eq!(listed[0].managed_settings_source, None);
    assert!(!listed[0].managed_hooks_locked);
    assert!(!listed[0].managed_mcp_locked);
}

#[tokio::test(start_paused = true)]
async fn launch_logs_posture_error_as_warning() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;

    // Capture logs in a shared buffer. The dispatcher pins go first, so no
    // concurrent launch on another thread can cache the posture callsite as
    // disabled (see `pin_tracing_dispatchers`).
    pin_tracing_dispatchers();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let logs_writer = logs.clone();

    let subscriber = tracing_subscriber::registry().with(
        fmt::layer()
            .with_writer(move || PostureLogWriter(logs_writer.clone()))
            .with_ansi(false),
    );

    // Set as global default for this test's duration
    let _guard = tracing::subscriber::set_default(subscriber);

    let h = Harness::start(Setup {
        managed_file_error: Some("access denied"),
        ..Setup::default()
    })
    .await;

    let _launched = h.launch_ok().await;

    // Verify the posture-error warn was emitted
    let log_bytes = logs.lock().unwrap();
    let log_output = String::from_utf8_lossy(&log_bytes);
    assert!(
        log_output.contains("WARN"),
        "Expected WARN level in logs, got:\n{}",
        log_output
    );
    assert!(
        log_output.contains("managed-settings posture could not be determined at launch"),
        "Expected posture error message in logs"
    );
    assert!(
        log_output.contains("error=file:"),
        "Expected error field to name the failing source"
    );
    // The error message itself (like "cannot read: access denied") is fine;
    // we're protecting against document-content leakage (e.g., raw JSON).
    // This test uses a read-error scenario, so no document exists to leak.
}

/// Writer adapter for capturing tracing output
struct PostureLogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for PostureLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

// ----------------------------------------------------------------- relay sessions

/// Stub policy for testing: returns a fixed result.
struct RefusingPolicy {
    status: StatusCode,
    message: String,
}

#[async_trait::async_trait]
impl crate::llm_proxy::sessions::SessionPolicy for RefusingPolicy {
    async fn check(&self, _client_id: &str) -> Result<(), (StatusCode, String)> {
        Err((self.status, self.message.clone()))
    }
}

#[tokio::test(start_paused = true)]
async fn a_launch_without_a_proxy_address_answers_409() {
    let h = Harness::start(Setup {
        proxy_address: None,
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("the local LLM proxy is not running (daemon.llmProxy.listen)"),
        "{body}"
    );
    h.assert_untouched();
    assert!(h.credentials.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_proxy_check_comes_after_an_invalid_cwd() {
    let h = Harness::start(Setup {
        proxy_address: None,
        ..Setup::default()
    })
    .await;

    let (status, body) = h
        .launch(&h.body_with(|body| {
            body["cwd"] = h.home.display().to_string().into();
        }))
        .await;

    assert_eq!(status, 400, "{body}");
    assert_ne!(
        body, "the local LLM proxy is not running (daemon.llmProxy.listen)",
        "cwd check runs before proxy check"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refusing_session_policy_passes_its_status_and_text_through() {
    let h = Harness::start(Setup {
        session_policy: Arc::new(RefusingPolicy {
            status: StatusCode::CONFLICT,
            message: "policy says no".to_owned(),
        }),
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 409, "{body}");
    assert_eq!(body, "policy says no");
    h.assert_untouched();
    assert!(h.executor.spawns().is_empty());
}

#[tokio::test(start_paused = true)]
async fn controller_jwt_without_claude_code_in_allowed_client_ids_is_refused() {
    let config: agentdesktop_core::config::DaemonConfig =
        serde_yaml::from_str(
            "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [copilot-cli]\nprograms:\n  claudeCode: {}\n"
        )
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let h = Harness::start(Setup {
        session_policy: Arc::new(crate::llm_proxy::sessions::ConfigPolicy {
            config,
            state_dir: temp.path().to_owned(),
        }),
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("claude-code") && body.contains("allowedClientIds"),
        "{body}"
    );
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn a_successful_launch_registers_a_session_with_the_pairing_in_env() {
    let h = Harness::start(Setup::default()).await;

    let launched = h.launch_ok().await;

    let now = h.credentials.clock.now();
    let sessions = h.sessions.list(now);
    assert_eq!(sessions.len(), 1, "exactly one session registered");
    let session = &sessions[0];
    assert_eq!(session.id, launched.session_id);
    assert_eq!(session.client_id, "claude-code");
    assert!(
        session.label.starts_with("run "),
        "label is 'run <launch_id>': {}",
        session.label
    );

    let spawn = &h.executor.spawns()[0];
    let request: serde_json::Value =
        serde_json::from_str(spawn.request_json.as_deref().unwrap()).unwrap();
    let env = env_map(&request);
    let pairing_header = env
        .get("ANTHROPIC_CUSTOM_HEADERS")
        .expect("ANTHROPIC_CUSTOM_HEADERS in env");
    assert!(
        pairing_header.starts_with("x-agentdesktop-pairing: "),
        "{pairing_header}"
    );
    let pairing = pairing_header
        .strip_prefix("x-agentdesktop-pairing: ")
        .unwrap();

    let body = serde_json::to_string(&launched).unwrap();
    assert!(
        !body.contains(pairing),
        "pairing is not in the 201 body: {body}"
    );
    assert!(
        !body.contains("credentialExpiresAtUnixSeconds"),
        "no credential expiry in body: {body}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_early_exit_revokes_the_session_before_answering() {
    let h = Harness::start(Setup::default()).await;
    h.executor.exit_with(1);

    let (status, _) = h.launch(&h.body()).await;

    assert_eq!(status, 500);
    let now = h.credentials.clock.now();
    assert_eq!(h.sessions.len(now), 0, "session revoked on early exit");
}

#[tokio::test(start_paused = true)]
async fn a_spawn_error_revokes_the_session() {
    let h = Harness::start(Setup::default()).await;
    *h.executor.error.lock().unwrap() =
        Some(io::Error::new(io::ErrorKind::NotFound, "no such file"));

    let (status, _) = h.launch(&h.body()).await;

    assert_eq!(status, 500);
    let now = h.credentials.clock.now();
    assert_eq!(h.sessions.len(now), 0, "session revoked on spawn error");
}

#[tokio::test(start_paused = true)]
async fn session_capacity_exhaustion_answers_429_before_the_credential_check() {
    let h = Harness::start(Setup {
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;
    let now = h.credentials.clock.now();
    for n in 0..64 {
        h.sessions
            .register(
                "claude-code",
                &format!("s{n}"),
                Duration::from_secs(3600),
                now,
            )
            .unwrap();
    }

    let (status, body) = h.launch(&h.body()).await;

    assert_eq!(status, 429, "{body}");
    assert!(
        body.contains("too many sessions (64)") && body.contains("revoke unused ones"),
        "{body}"
    );
    h.assert_untouched();
    assert_eq!(h.sessions.len(now), 64, "registry count unchanged");
    assert!(h.credentials.calls().is_empty());
}

#[tokio::test]
async fn the_launch_log_names_the_session_once_and_never_its_pairing() {
    let (logs, _guard) = capture_logs();
    let h = Harness::start(Setup::default()).await;

    let launched = h.launch_ok().await;

    let spawn = &h.executor.spawns()[0];
    let request: serde_json::Value =
        serde_json::from_str(spawn.request_json.as_deref().unwrap()).unwrap();
    let env = env_map(&request);
    let pairing_header = env
        .get("ANTHROPIC_CUSTOM_HEADERS")
        .expect("ANTHROPIC_CUSTOM_HEADERS in env");
    let pairing = pairing_header
        .strip_prefix("x-agentdesktop-pairing: ")
        .expect("header starts with x-agentdesktop-pairing: ");

    let log_text = logs.wait_for_text(&launched.session_id).await;
    let session_count = log_text.matches(&launched.session_id).count();
    assert_eq!(
        session_count, 1,
        "session_id appears exactly once in the log: {}",
        launched.session_id
    );
    assert!(
        !log_text.contains(pairing),
        "pairing value never appears in the log"
    );
}

// ----------------------------------------------------------------- Windows only

#[cfg(windows)]
#[test]
fn the_current_process_has_a_user_sid_outside_an_app_container() {
    let peer = peer_identity(std::process::id()).expect("read the identity of this process");
    let sid = peer.user_sid.expect("a user SID");
    assert!(sid.starts_with("S-1-5-"), "{sid}");
    assert!(!peer.app_container);
}

#[cfg(windows)]
#[test]
fn the_user_mode_pipe_security_descriptor_for_the_daemon_sid_is_valid() {
    let sid = peer_identity(std::process::id()).unwrap().user_sid.unwrap();
    crate::windows_security::SecurityDescriptor::from_sddl(&user_mode_pipe_sddl(&sid))
        .expect("the SDDL parses");
}

#[test]
fn copy_log_lines_uses_raw_byte_offset_not_lossy_string_offset() {
    use std::io::Write;

    // A non-UTF-8 byte before a newline: the file is 5 bytes, but the lossy
    // string is 7 bytes (U+FFFD is 3 bytes in UTF-8). The returned offset
    // must be the raw byte position (5) so the next read starts at the
    // correct position.
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("test.log");
    let mut file = std::fs::File::create(&log).unwrap();
    file.write_all(b"caf\xe9\n").unwrap();
    drop(file);

    let new_offset = super::copy_log_lines("test-launch", &log, 0);
    assert_eq!(new_offset, 5, "offset should be raw byte count");

    // Append another line and verify the second read starts at the correct position.
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    file.write_all(b"second\n").unwrap();
    drop(file);

    let final_offset = super::copy_log_lines("test-launch", &log, new_offset);
    assert_eq!(final_offset, 12, "second read from correct offset");
}

// --- PR 2d spec-derived tests (AC 7, engine interface)

#[test]
fn the_engine_reproduces_the_baseline_request_bytes() {
    use engine::Engine;
    use engine::mxc::MxcEngine;
    use profile::CLAUDE_CODE;

    const LAUNCH_ID: &str = "0123456789abcdef";

    // Oracle (a): launch_input + snapshot_existing extended with both git
    // user-configuration files, egress: Allow. Re-captured for PR 4 with the
    // same fixture (ledger ## v5 writer): the git user-configuration files
    // moved from readonlyPaths to deniedPaths, the generated git_config is
    // granted read-only, and the box env gains GIT_CONFIG_GLOBAL and
    // GIT_TERMINAL_PROMPT=0 (AC 6).
    const ORACLE_A: &str = r#"{
  "version": "1.0.0",
  "containment": "processcontainer",
  "containerId": "agentdesktop-0123456789abcdef",
  "ui": {
    "disable": false,
    "clipboard": "none",
    "injection": false
  },
  "process": {
    "commandLine": "cmd.exe /d /v:off /c start \"agentdesktop: claude\" /WAIT /D \"C:\\lab\\ws pr1\" \"C:\\Users\\dev\\.local\\bin\\claude.exe\" --debug-file \"C:\\lab\\ws pr1\\pr1-l2.log\" --allowedTools PowerShell",
    "cwd": "C:\\lab\\ws pr1",
    "inheritDefaultEnv": false,
    "env": [
      "ANTHROPIC_AUTH_TOKEN=placeholder",
      "ANTHROPIC_BASE_URL=http://127.0.0.1:4000/sandbox",
      "ANTHROPIC_CUSTOM_HEADERS=x-agentdesktop-pairing: test-pairing-value",
      "APPDATA=C:\\Users\\dev\\AppData\\Roaming",
      "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
      "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1",
      "CLAUDE_CODE_USE_POWERSHELL_TOOL=1",
      "CLAUDE_CONFIG_DIR=C:\\Users\\dev\\.claude",
      "ComSpec=C:\\Windows\\system32\\cmd.exe",
      "DISABLE_AUTOUPDATER=1",
      "GIT_CONFIG_GLOBAL=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config",
      "GIT_TERMINAL_PROMPT=0",
      "HOME=C:\\Users\\dev",
      "HOMEDRIVE=C:",
      "HOMEPATH=\\Users\\dev",
      "LOCALAPPDATA=C:\\Users\\dev\\AppData\\Local",
      "PATH=C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\WindowsPowerShell\\v1.0;C:\\Program Files\\Git\\cmd;C:\\Users\\dev\\.local\\bin;C:\\lab\\agentdesktop",
      "PATHEXT=.COM;.EXE;.BAT;.CMD",
      "ProgramData=C:\\ProgramData",
      "ProgramFiles=C:\\Program Files",
      "SystemDrive=C:",
      "SystemRoot=C:\\Windows",
      "TEMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp",
      "TERM=xterm-256color",
      "TMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp",
      "USERNAME=dev",
      "USERPROFILE=C:\\Users\\dev",
      "windir=C:\\Windows"
    ]
  },
  "filesystem": {
    "readwritePaths": [
      "C:\\lab\\ws pr1",
      "C:\\Users\\dev\\.claude",
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp"
    ],
    "readonlyPaths": [
      "C:\\Users\\dev\\.local\\bin",
      "C:\\Users\\dev\\.local\\share\\claude",
      "C:\\Program Files\\Git",
      "C:\\lab\\agentdesktop",
      "C:\\Program Files\\ClaudeCode",
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config",
      "C:\\Users\\dev\\.claude\\settings.json",
      "C:\\Users\\dev\\.claude\\plugins",
      "C:\\lab\\ws pr1\\.mcp.json",
      "C:\\lab\\ws pr1\\.claude\\settings.local.json",
      "C:\\lab\\ws pr1\\.git\\config",
      "C:\\lab\\ws pr1\\.git\\hooks"
    ],
    "deniedPaths": [
      "C:\\lab\\ws pr1\\.github",
      "C:\\Users\\dev\\.claude\\hooks",
      "C:\\lab\\ws pr1\\HEAD",
      "C:\\lab\\ws pr1\\.claude\\settings.json",
      "C:\\lab\\ws pr1\\.claude\\hooks",
      "C:\\lab\\ws pr1\\.git\\config.worktree",
      "C:\\lab\\ws pr1\\.git\\commondir",
      "C:\\lab\\ws pr1\\opencode.json",
      "C:\\lab\\ws pr1\\opencode.jsonc",
      "C:\\lab\\ws pr1\\.opencode",
      "C:\\Users\\dev\\.claude\\.credentials.json",
      "C:\\Users\\dev\\.claude\\history.jsonl",
      "C:\\Users\\dev\\.claude\\ide",
      "C:\\Users\\dev\\.ssh",
      "C:\\Users\\dev\\.aws",
      "C:\\Users\\dev\\.azure",
      "C:\\Users\\dev\\.kube",
      "C:\\Users\\dev\\.gitconfig",
      "C:\\Users\\dev\\.config\\git\\config"
    ]
  },
  "network": {
    "egress": {
      "default": "allow"
    },
    "ingress": {
      "default": "allow",
      "hostLoopback": "allow"
    }
  }
}"#;

    // Oracle (b): same with egress: Deny
    const ORACLE_B: &str = r#"{
  "version": "1.0.0",
  "containment": "processcontainer",
  "containerId": "agentdesktop-0123456789abcdef",
  "ui": {
    "disable": false,
    "clipboard": "none",
    "injection": false
  },
  "process": {
    "commandLine": "cmd.exe /d /v:off /c start \"agentdesktop: claude\" /WAIT /D \"C:\\lab\\ws pr1\" \"C:\\Users\\dev\\.local\\bin\\claude.exe\" --debug-file \"C:\\lab\\ws pr1\\pr1-l2.log\" --allowedTools PowerShell",
    "cwd": "C:\\lab\\ws pr1",
    "inheritDefaultEnv": false,
    "env": [
      "ANTHROPIC_AUTH_TOKEN=placeholder",
      "ANTHROPIC_BASE_URL=http://127.0.0.1:4000/sandbox",
      "ANTHROPIC_CUSTOM_HEADERS=x-agentdesktop-pairing: test-pairing-value",
      "APPDATA=C:\\Users\\dev\\AppData\\Roaming",
      "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
      "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1",
      "CLAUDE_CODE_USE_POWERSHELL_TOOL=1",
      "CLAUDE_CONFIG_DIR=C:\\Users\\dev\\.claude",
      "ComSpec=C:\\Windows\\system32\\cmd.exe",
      "DISABLE_AUTOUPDATER=1",
      "GIT_CONFIG_GLOBAL=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config",
      "GIT_TERMINAL_PROMPT=0",
      "HOME=C:\\Users\\dev",
      "HOMEDRIVE=C:",
      "HOMEPATH=\\Users\\dev",
      "LOCALAPPDATA=C:\\Users\\dev\\AppData\\Local",
      "PATH=C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\WindowsPowerShell\\v1.0;C:\\Program Files\\Git\\cmd;C:\\Users\\dev\\.local\\bin;C:\\lab\\agentdesktop",
      "PATHEXT=.COM;.EXE;.BAT;.CMD",
      "ProgramData=C:\\ProgramData",
      "ProgramFiles=C:\\Program Files",
      "SystemDrive=C:",
      "SystemRoot=C:\\Windows",
      "TEMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp",
      "TERM=xterm-256color",
      "TMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp",
      "USERNAME=dev",
      "USERPROFILE=C:\\Users\\dev",
      "windir=C:\\Windows"
    ]
  },
  "filesystem": {
    "readwritePaths": [
      "C:\\lab\\ws pr1",
      "C:\\Users\\dev\\.claude",
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp"
    ],
    "readonlyPaths": [
      "C:\\Users\\dev\\.local\\bin",
      "C:\\Users\\dev\\.local\\share\\claude",
      "C:\\Program Files\\Git",
      "C:\\lab\\agentdesktop",
      "C:\\Program Files\\ClaudeCode",
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config",
      "C:\\Users\\dev\\.claude\\settings.json",
      "C:\\Users\\dev\\.claude\\plugins",
      "C:\\lab\\ws pr1\\.mcp.json",
      "C:\\lab\\ws pr1\\.claude\\settings.local.json",
      "C:\\lab\\ws pr1\\.git\\config",
      "C:\\lab\\ws pr1\\.git\\hooks"
    ],
    "deniedPaths": [
      "C:\\lab\\ws pr1\\.github",
      "C:\\Users\\dev\\.claude\\hooks",
      "C:\\lab\\ws pr1\\HEAD",
      "C:\\lab\\ws pr1\\.claude\\settings.json",
      "C:\\lab\\ws pr1\\.claude\\hooks",
      "C:\\lab\\ws pr1\\.git\\config.worktree",
      "C:\\lab\\ws pr1\\.git\\commondir",
      "C:\\lab\\ws pr1\\opencode.json",
      "C:\\lab\\ws pr1\\opencode.jsonc",
      "C:\\lab\\ws pr1\\.opencode",
      "C:\\Users\\dev\\.claude\\.credentials.json",
      "C:\\Users\\dev\\.claude\\history.jsonl",
      "C:\\Users\\dev\\.claude\\ide",
      "C:\\Users\\dev\\.ssh",
      "C:\\Users\\dev\\.aws",
      "C:\\Users\\dev\\.azure",
      "C:\\Users\\dev\\.kube",
      "C:\\Users\\dev\\.gitconfig"
    ]
  },
  "network": {
    "egress": {
      "default": "deny"
    },
    "ingress": {
      "default": "allow",
      "hostLoopback": "allow"
    }
  }
}"#;

    // Oracle (c): minimal input
    const ORACLE_C: &str = r#"{
  "version": "1.0.0",
  "containment": "processcontainer",
  "containerId": "agentdesktop-0123456789abcdef",
  "ui": {
    "disable": false,
    "clipboard": "none",
    "injection": false
  },
  "process": {
    "commandLine": "cmd.exe /d /v:off /c start \"agentdesktop: claude\" /WAIT /D \"C:\\ws\" \"C:\\Users\\dev\\.local\\bin\\claude.exe\"",
    "cwd": "C:\\ws",
    "inheritDefaultEnv": false,
    "env": [
      "ANTHROPIC_AUTH_TOKEN=placeholder",
      "ANTHROPIC_BASE_URL=http://127.0.0.1:4000/sandbox",
      "ANTHROPIC_CUSTOM_HEADERS=x-agentdesktop-pairing: test-pairing-value",
      "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
      "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1",
      "CLAUDE_CODE_USE_POWERSHELL_TOOL=1",
      "CLAUDE_CONFIG_DIR=C:\\Users\\dev\\.claude",
      "DISABLE_AUTOUPDATER=1",
      "GIT_CONFIG_GLOBAL=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config",
      "GIT_TERMINAL_PROMPT=0",
      "HOME=C:\\Users\\dev",
      "PATH=C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\WindowsPowerShell\\v1.0;C:\\Users\\dev\\.local\\bin;C:\\lab\\agentdesktop",
      "TEMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp",
      "TERM=xterm-256color",
      "TMP=C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp"
    ]
  },
  "filesystem": {
    "readwritePaths": [
      "C:\\ws",
      "C:\\Users\\dev\\.claude",
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\tmp"
    ],
    "readonlyPaths": [
      "C:\\Users\\dev\\.local\\state\\agentdesktop\\launches\\0123456789abcdef\\git_config"
    ],
    "deniedPaths": [
      "C:\\ws\\.claude",
      "C:\\ws\\.git",
      "C:\\ws\\.github",
      "C:\\Users\\dev\\.claude\\settings.json",
      "C:\\Users\\dev\\.claude\\hooks",
      "C:\\Users\\dev\\.claude\\plugins",
      "C:\\ws\\.mcp.json",
      "C:\\ws\\HEAD",
      "C:\\ws\\opencode.json",
      "C:\\ws\\opencode.jsonc",
      "C:\\ws\\.opencode",
      "C:\\Users\\dev\\.claude\\.credentials.json",
      "C:\\Users\\dev\\.claude\\history.jsonl",
      "C:\\Users\\dev\\.claude\\ide",
      "C:\\Users\\dev\\.ssh",
      "C:\\Users\\dev\\.aws",
      "C:\\Users\\dev\\.azure",
      "C:\\Users\\dev\\.kube",
      "C:\\Users\\dev\\.gitconfig"
    ]
  },
  "network": {
    "egress": {
      "default": "allow"
    },
    "ingress": {
      "default": "allow",
      "hostLoopback": "allow"
    }
  }
}"#;

    // Fixture (a): both git user-configuration files exist, so the oracle
    // covers their positions among the read-only grants. The XDG config's
    // parent directory also exists (required for MXC denied-path rule).
    let existing_a = {
        let mut existing = snapshot_existing();
        existing.insert(r"C:\Users\dev\.gitconfig".to_owned());
        existing.insert(r"C:\Users\dev\.config\git".to_owned());
        existing.insert(r"C:\Users\dev\.config\git\config".to_owned());
        existing
    };
    let directories_a = {
        let mut dirs = snapshot_directories();
        dirs.insert(r"C:\Users\dev\.config\git".to_owned());
        dirs
    };
    let exists_a = |path: &Path| existing_a.contains(path.to_str().expect("UTF-8 path"));
    let is_dir_a = |path: &Path| directories_a.contains(path.to_str().expect("UTF-8 path"));
    let input_a = launch_input(&exists_a, &is_dir_a);
    let plan_a = plan(&input_a, &CLAUDE_CODE);
    let bytes_a = MxcEngine.request(&plan_a, LAUNCH_ID).unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes_a), ORACLE_A);

    // Fixture (b)
    let exists_b = |path: &Path| snapshot_existing().contains(path.to_str().expect("UTF-8 path"));
    let is_dir_b =
        |path: &Path| snapshot_directories().contains(path.to_str().expect("UTF-8 path"));
    let mut input_b = launch_input(&exists_b, &is_dir_b);
    input_b.egress = agentdesktop_core::config::ContainmentEgress::Deny;
    let plan_b = plan(&input_b, &CLAUDE_CODE);
    let bytes_b = MxcEngine.request(&plan_b, LAUNCH_ID).unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes_b), ORACLE_B);

    // Fixture (c)
    let exists_c = |_: &Path| false;
    let is_dir_c = |_: &Path| false;
    let input_c = LaunchInput {
        launch_id: LAUNCH_ID.into(),
        executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
        client_exe_dir: PathBuf::from(r"C:\lab\agentdesktop"),
        cwd: PathBuf::from(r"C:\ws"),
        args: vec![],
        home: PathBuf::from(r"C:\Users\dev"),
        state_dir: PathBuf::from(r"C:\Users\dev\.local/state\agentdesktop"),
        launch_dir: PathBuf::from(
            r"C:\Users\dev\.local/state\agentdesktop\launches\0123456789abcdef",
        ),
        git_dir: None,
        program_files_claude: None,
        egress: agentdesktop_core::config::ContainmentEgress::Allow,
        relay_base_url: "http://127.0.0.1:4000/sandbox".into(),
        pairing: "test-pairing-value".into(),
        base_env: BTreeMap::new(),
        ssh_enabled: false,
        exists: &exists_c,
        is_dir: &is_dir_c,
        console: crate::launch::plan::ConsolePlan::Window,
    };
    let plan_c = plan(&input_c, &CLAUDE_CODE);
    let bytes_c = MxcEngine.request(&plan_c, LAUNCH_ID).unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes_c), ORACLE_C);
}

#[test]
fn engine_selection_absent_key_defaults_to_mxc() {
    use agentdesktop_core::config::ContainmentEngine;
    let json = r#"{"executor": "C:\\path\\to\\wxc-exec.exe"}"#;
    let cfg: agentdesktop_core::config::ContainmentStartupConfig =
        serde_json::from_str(json).unwrap();
    assert_eq!(cfg.engine, ContainmentEngine::Mxc);
}

#[test]
fn engine_selection_unknown_value_fails() {
    let json = r#"{"executor": "C:\\path\\to\\wxc-exec.exe", "engine": "unknown"}"#;
    let result: Result<agentdesktop_core::config::ContainmentStartupConfig, _> =
        serde_json::from_str(json);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("mxc"),
        "error should name accepted values: {}",
        err
    );
}

#[test]
fn posture_carries_engine_name() {
    use engine::Engine;
    use engine::mxc::MxcEngine;

    let info = containment_info(
        Some(&agentdesktop_core::config::ContainmentStartupConfig {
            executor: p("path/to/wxc-exec.exe"),
            egress: agentdesktop_core::config::ContainmentEgress::Allow,
            engine: agentdesktop_core::config::ContainmentEngine::Mxc,
            egress_upstream: None,
            git: None,
        }),
        true,
        true,
        false,
        &|_| true,
        MxcEngine.name(),
    );
    assert_eq!(info.as_ref().unwrap().engine, "mxc");
}

#[test]
fn posture_carries_engine_when_containment_unavailable() {
    use engine::Engine;
    use engine::mxc::MxcEngine;

    let info = containment_info(
        Some(&agentdesktop_core::config::ContainmentStartupConfig {
            executor: p("path/to/missing.exe"),
            egress: agentdesktop_core::config::ContainmentEgress::Allow,
            engine: agentdesktop_core::config::ContainmentEngine::Mxc,
            egress_upstream: None,
            git: None,
        }),
        true,
        true,
        false,
        &|_| false,
        MxcEngine.name(),
    );
    let info = info.unwrap();
    assert!(!info.available);
    assert_eq!(info.engine, "mxc");
}

#[test]
fn mxc_engine_name_is_mxc() {
    use engine::Engine;
    use engine::mxc::MxcEngine;
    assert_eq!(MxcEngine.name(), "mxc");
}

#[test]
fn mxc_engine_container_id() {
    use engine::Engine;
    use engine::mxc::MxcEngine;
    assert_eq!(
        MxcEngine.container_id("0123456789abcdef"),
        "agentdesktop-0123456789abcdef"
    );
}

#[test]
fn plan_yields_expected_env_keys() {
    use profile::CLAUDE_CODE;

    let exists = |path: &Path| snapshot_existing().contains(path.to_str().expect("UTF-8 path"));
    let is_dir = |path: &Path| snapshot_directories().contains(path.to_str().expect("UTF-8 path"));
    let input = launch_input(&exists, &is_dir);
    let plan_result = plan(&input, &CLAUDE_CODE);

    // Check that plan has the relay URL and pairing in env
    assert!(plan_result.env.contains_key("ANTHROPIC_BASE_URL"));
    assert!(plan_result.env.contains_key("ANTHROPIC_CUSTOM_HEADERS"));
    assert_eq!(
        plan_result.env.get("ANTHROPIC_BASE_URL").unwrap(),
        "http://127.0.0.1:4000/sandbox"
    );
}

// --- Profile equivalence (spec "Tests first"): every `ClaudeCodeProfile`
// field equals the hard-coded list it replaced, written out literally from
// the baseline commit 8910230 and compared entry by entry in order, so a
// future profile edit shows up as a reviewable test diff here.

#[test]
fn the_profile_base_env_keys_equal_the_replaced_base_env() {
    // Baseline 8910230 crates/agent/src/launch/host.rs:24-38 (`BASE_ENV`).
    let expected: &[&str] = &[
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramFiles",
        "ProgramData",
        "USERNAME",
    ];
    assert_eq!(profile::SHARED.base_env_keys, expected);
}

#[test]
fn the_profile_fixed_env_equals_the_replaced_env_inserts() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:463-480: the six fixed
    // `env.insert` literals, in insertion order (the dynamic inserts between
    // them stay in `plan`; the map is a `BTreeMap`, so order is not
    // observable, but the list is pinned verbatim anyway).
    let expected: &[(&str, &str)] = &[
        ("TERM", "xterm-256color"),
        ("CLAUDE_CODE_USE_POWERSHELL_TOOL", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_AUTH_TOKEN", "placeholder"),
        ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"),
    ];
    assert_eq!(profile::CLAUDE_CODE.fixed_env, expected);
}

#[test]
fn the_profile_runtime_entries_equal_the_replaced_runtime_list() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:495-506 (the `runtime`
    // array): the three home-relative entries and their order among the
    // discovered directories; the byte oracle (fixture (a)) pins the
    // positions in the request.
    assert_eq!(profile::CLAUDE_HOME.runtime_share, r".local\share\claude");
    assert_eq!(profile::SHARED.git_user_config, ".gitconfig");
    assert_eq!(profile::SHARED.git_xdg_config, r".config\git\config");
}

#[test]
fn the_profile_home_executable_config_equals_the_replaced_list() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:514-516 (the
    // `claude_dir` joins of `executable_config`).
    let expected: &[&str] = &["settings.json", "hooks", "plugins"];
    assert_eq!(profile::CLAUDE_HOME.home_executable_config, expected);
}

#[test]
fn the_profile_workspace_entries_equal_the_replaced_lists() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:517-520 (the two `cwd`
    // joins of `executable_config`) and :526-530
    // (`workspace_claude_entries`).
    assert_eq!(profile::SHARED.mcp_json, ".mcp.json");
    assert_eq!(profile::SHARED.workspace_head, "HEAD");
    let expected: &[&str] = &["settings.json", "settings.local.json", "hooks"];
    assert_eq!(profile::SHARED.workspace_claude_entries, expected);
}

#[test]
fn the_profile_git_executable_config_equals_the_replaced_list() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:548-550 (the `.git`
    // pushes onto `executable_config`), plus `config.worktree`: with
    // `extensions.worktreeConfig` a box could otherwise write
    // `core.sshCommand`/`credential.helper` there for a later unboxed git.
    let expected: &[&str] = &["config", "config.worktree", "hooks", "commondir"];
    assert_eq!(profile::SHARED.git_executable_config, expected);
}

#[test]
fn the_profile_denied_secrets_equal_the_replaced_secret_list() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:560-569: the first
    // three were joined onto `claude_dir = join(home, ".claude")`, the rest
    // onto `home`; a `.claude\<entry>` segment joined onto `home` is
    // string-identical through `join`.
    let expected: &[&str] = &[
        r".claude\.credentials.json",
        r".claude\history.jsonl",
        r".claude\ide",
        ".ssh",
        ".aws",
        ".azure",
        ".kube",
    ];
    assert_eq!(profile::SHARED.denied_secrets, expected);
    // The single-segment entries double as the workspace-validation denied
    // home directories (baseline host.rs:50, `DENIED_HOME_DIRS`).
    let denied_home_dirs: Vec<&str> = profile::SHARED
        .denied_secrets
        .iter()
        .copied()
        .filter(|entry| !entry.contains('\\'))
        .collect();
    assert_eq!(denied_home_dirs, [".ssh", ".aws", ".azure", ".kube"]);
}

#[test]
fn the_profile_window_title_equals_the_replaced_command_line_title() {
    // Baseline 8910230 crates/agent/src/launch/mod.rs:483-485 (the `start`
    // window title in the command line).
    assert_eq!(profile::CLAUDE_CODE.window_title, "agentdesktop: claude");
}

// ------------------------------------- PR 5: ssh generation and wiring
//
// Spec-derived tests for PR 5 (`specs/PR-5-ssh-agent.md`, Tests first:
// Generation, the env guard, and the launch side of Leg pump). The broker
// suites live in `launch/ssh/mod.rs`. These drive the launch handler with
// `git.ssh` set and read what the daemon produced at the moment the box
// starts (the stub executor's `on_spawn`): with ssh hosts configured the
// post-spawn pipe setup cannot succeed against the stub child (pid 4242 has
// no AppContainer box behind it; off Windows there is no pipe at all), so the
// launch fails closed right after (AC 3) and its directory is removed.

const SSH_HOST_A: &str = "git.example.com";
const SSH_HOST_B: &str = "code.example.org";
const HTTPS_ONLY_HOST: &str = "other.example.net";
const SEED_A: &str = "git.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
const SEED_B: &str = "code.example.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIC";
const SEED_OTHER: &str = "other.example.net ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMD";

fn ssh_git_config(ssh: Option<(&[&str], &[&str])>) -> agentdesktop_core::config::GitConfig {
    agentdesktop_core::config::GitConfig {
        allow: vec![
            format!("https://{SSH_HOST_A}/org/"),
            format!("https://{HTTPS_ONLY_HOST}/team/"),
        ],
        push_allow: vec![],
        credentials: vec![],
        ssh: ssh.map(|(hosts, keys)| agentdesktop_core::config::GitSshConfig {
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }),
    }
}

fn ssh_setup(ssh: Option<(&[&str], &[&str])>) -> Setup {
    Setup {
        git: Some(ssh_git_config(ssh)),
        known_hosts: vec![
            (SSH_HOST_A, SEED_A),
            (SSH_HOST_B, SEED_B),
            (HTTPS_ONLY_HOST, SEED_OTHER),
        ],
        client_exe_name: Some("agentdesktop-windows-x64"),
        ..Setup::default()
    }
}

/// What the launch directory held when the box was started.
#[derive(Clone, Debug, Default)]
struct SpawnTimeFiles {
    launch_dir: PathBuf,
    ssh_config: Option<String>,
    known_hosts: Option<String>,
    git_config: Option<String>,
    git_ssh: Option<String>,
    /// Unix permission bits of (ssh_config, known_hosts); `None` elsewhere
    /// or when absent.
    modes: (Option<u32>, Option<u32>),
    /// Each `Port` of the ssh_config, and whether 127.0.0.1 had a listener on
    /// it at spawn.
    ports_in_use: Vec<(u16, bool)>,
}

/// Whether something listens on 127.0.0.1:`port` (probed by binding, so no
/// connection reaches a leg and nothing is dialed).
fn port_in_use(port: u16) -> bool {
    matches!(
        std::net::TcpListener::bind(("127.0.0.1", port)),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse
    )
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

fn capture_at_spawn(h: &Harness) -> Arc<Mutex<Option<SpawnTimeFiles>>> {
    let captured = Arc::new(Mutex::new(None));
    let slot = captured.clone();
    let launches = h.state_dir.join("launches");
    *h.executor.on_spawn.lock().unwrap() = Some(Box::new(move || {
        let Some(Ok(entry)) = std::fs::read_dir(&launches)
            .ok()
            .and_then(|mut dir| dir.next())
        else {
            return;
        };
        let launch_dir = entry.path();
        let read = |name: &str| std::fs::read_to_string(launch_dir.join(name)).ok();
        let ssh_config = read(SSH_CONFIG_FILE);
        let ports_in_use = ssh_config
            .as_deref()
            .map(ssh_config_ports)
            .unwrap_or_default()
            .into_iter()
            .map(|port| (port, port_in_use(port)))
            .collect();
        *slot.lock().unwrap() = Some(SpawnTimeFiles {
            ssh_config,
            known_hosts: read(KNOWN_HOSTS_FILE),
            git_config: read(GIT_CONFIG_FILE),
            git_ssh: read(GIT_SSH_FILE),
            modes: (
                file_mode(&launch_dir.join(SSH_CONFIG_FILE)),
                file_mode(&launch_dir.join(KNOWN_HOSTS_FILE)),
            ),
            ports_in_use,
            launch_dir,
        });
    }));
    captured
}

fn spawn_time_files(captured: &Arc<Mutex<Option<SpawnTimeFiles>>>) -> SpawnTimeFiles {
    captured
        .lock()
        .unwrap()
        .clone()
        .expect("the box was started and the launch directory read")
}

/// The `Host` blocks of an ssh_config: host to its directives
/// `(keyword, value)`, surrounding quotes removed. Directives before the first
/// `Host` land under `""`.
fn ssh_config_blocks(text: &str) -> BTreeMap<String, Vec<(String, String)>> {
    let mut blocks: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut current = String::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (keyword, value) = line
            .split_once(char::is_whitespace)
            .map(|(keyword, value)| (keyword, value.trim()))
            .unwrap_or((line, ""));
        let value = value.trim_matches('"').to_owned();
        if keyword == "Host" {
            current = value.clone();
            blocks.entry(value).or_default();
            continue;
        }
        blocks
            .entry(current.clone())
            .or_default()
            .push((keyword.to_owned(), value));
    }
    blocks
}

fn ssh_config_ports(text: &str) -> Vec<u16> {
    ssh_config_blocks(text)
        .values()
        .flat_map(|directives| {
            directives
                .iter()
                .filter(|(keyword, _)| keyword == "Port")
                .map(|(_, value)| value.parse().unwrap_or(0))
                .collect::<Vec<u16>>()
        })
        .collect()
}

fn spawned_request(h: &Harness) -> (String, serde_json::Value) {
    let spawn = h.executor.spawns().into_iter().next().expect("one spawn");
    let request = serde_json::from_str(spawn.request_json.as_deref().unwrap()).unwrap();
    (spawn.launch_id, request)
}

#[tokio::test(start_paused = true)]
async fn ssh_config_and_known_hosts_are_generated_for_a_two_host_fixture() {
    let h = Harness::start(ssh_setup(Some((&[SSH_HOST_A, SSH_HOST_B], &["*"])))).await;
    let captured = capture_at_spawn(&h);

    let _ = h.launch(&h.body()).await;

    let files = spawn_time_files(&captured);
    let ssh_config = files
        .ssh_config
        .as_deref()
        .expect("ssh_config is in the launch directory before the box starts");
    assert!(
        !ssh_config.contains("IdentityAgent"),
        "the pipe travels in SSH_AUTH_SOCK, not IdentityAgent: {ssh_config}"
    );
    let blocks = ssh_config_blocks(ssh_config);
    assert_eq!(
        blocks.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([SSH_HOST_A, SSH_HOST_B]),
        "one Host block per ssh host and nothing else: {ssh_config}"
    );
    let known_hosts_path = format!("{}\\{KNOWN_HOSTS_FILE}", win(&files.launch_dir));
    let mut ports = Vec::new();
    for host in [SSH_HOST_A, SSH_HOST_B] {
        let port: u16 = blocks[host]
            .iter()
            .find(|(keyword, _)| keyword == "Port")
            .map(|(_, value)| value.parse().expect("a numeric Port"))
            .unwrap_or_else(|| panic!("a Port for {host}: {ssh_config}"));
        assert_ne!(port, 0, "{ssh_config}");
        let mut actual: Vec<(String, String)> = blocks[host]
            .iter()
            .map(|(keyword, value)| {
                if keyword == "UserKnownHostsFile" {
                    (keyword.clone(), value.replace('/', "\\"))
                } else {
                    (keyword.clone(), value.clone())
                }
            })
            .collect();
        actual.sort();
        let mut expected: Vec<(String, String)> = [
            ("HostName", "127.0.0.1".to_owned()),
            ("Port", port.to_string()),
            ("HostKeyAlias", host.to_owned()),
            ("UserKnownHostsFile", known_hosts_path.clone()),
            ("StrictHostKeyChecking", "accept-new".to_owned()),
            ("IdentitiesOnly", "no".to_owned()),
        ]
        .into_iter()
        .map(|(keyword, value)| (keyword.to_owned(), value))
        .collect();
        expected.sort();
        assert_eq!(actual, expected, "the {host} block: {ssh_config}");
        ports.push(port);
    }
    assert_ne!(ports[0], ports[1], "one loopback leg per host");
    assert_eq!(files.ports_in_use.len(), 2, "{:?}", files.ports_in_use);
    for (port, in_use) in &files.ports_in_use {
        assert!(in_use, "leg port {port} is listening before the box starts");
    }

    let known_hosts = files
        .known_hosts
        .as_deref()
        .expect("known_hosts is in the launch directory before the box starts");
    let lines: BTreeSet<&str> = known_hosts
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines,
        BTreeSet::from([SEED_A, SEED_B]),
        "exactly the seeded entries of the ssh hosts: {known_hosts}"
    );
    if cfg!(unix) {
        assert_eq!(
            files.modes,
            (Some(0o600), Some(0o600)),
            "both files are 0600"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_gitconfig_drops_the_ssh_refusal_mapping_only_for_ssh_hosts() {
    let h = Harness::start(ssh_setup(Some((&[SSH_HOST_A], &["*"])))).await;
    let captured = capture_at_spawn(&h);

    let _ = h.launch(&h.body()).await;

    let files = spawn_time_files(&captured);
    let git_config = files.git_config.as_deref().expect("git_config at spawn");
    // The ssh host: git uses the real remote and GIT_SSH.
    for absent in [
        format!("git-ssh/{SSH_HOST_A}/"),
        format!("insteadOf = ssh://{SSH_HOST_A}/"),
        format!("insteadOf = git@{SSH_HOST_A}:"),
    ] {
        assert!(
            !git_config.contains(&absent),
            "{absent} must be gone: {git_config}"
        );
    }
    // Its https mapping is untouched.
    assert!(
        git_config.contains(&format!("git/{SSH_HOST_A}/org/")),
        "{git_config}"
    );
    // A git.allow host that is not an ssh host keeps the refusal route.
    for present in [
        format!("git-ssh/{HTTPS_ONLY_HOST}/"),
        format!("insteadOf = ssh://{HTTPS_ONLY_HOST}/"),
        format!("insteadOf = git@{HTTPS_ONLY_HOST}:"),
    ] {
        assert!(
            git_config.contains(&present),
            "{present} must stay: {git_config}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn ssh_env_names_the_launch_pipe_and_the_generated_ssh_config() {
    let h = Harness::start(ssh_setup(Some((&[SSH_HOST_A, SSH_HOST_B], &["*"])))).await;

    let _ = h.launch(&h.body()).await;

    let (launch_id, request) = spawned_request(&h);
    let env = env_map(&request);
    let dir = win(&h.launch_dir(&launch_id));
    assert_eq!(
        env.get("SSH_AUTH_SOCK"),
        Some(&format!("{}{launch_id}", ssh::pipe::AGENT_PIPE_PREFIX)),
        "{env:?}"
    );
    // Git reaches ssh through the launcher script beside the ssh_config,
    // which git runs without a shell (fix round v3, lab F-ssh-1).
    let launcher = format!("{dir}\\{GIT_SSH_FILE}");
    assert_eq!(env.get("GIT_SSH"), Some(&launcher), "{env:?}");
    assert_eq!(
        env.get("GIT_SSH_VARIANT").map(String::as_str),
        Some("ssh"),
        "{env:?}"
    );
    assert!(
        env["PATH"]
            .split(';')
            .any(|entry| entry.eq_ignore_ascii_case(r"C:\Windows\System32\OpenSSH")),
        "Windows OpenSSH is on the box PATH: {}",
        env["PATH"]
    );
    let ssh_config = format!("{dir}\\{SSH_CONFIG_FILE}");
    let known_hosts = format!("{dir}\\{KNOWN_HOSTS_FILE}");
    assert!(
        readonly(&request).contains(&ssh_config),
        "ssh_config is granted read-only, exactly the file: {:?}",
        readonly(&request)
    );
    assert!(
        readonly(&request).contains(&launcher) && !readwrite(&request).contains(&launcher),
        "the launcher script is granted read-only, exactly the file: {:?}",
        readonly(&request)
    );
    assert!(
        readwrite(&request).contains(&known_hosts),
        "known_hosts is granted read-write, exactly the file: {:?}",
        readwrite(&request)
    );
    assert!(
        !readwrite(&request).contains(&ssh_config),
        "ssh_config is not writable"
    );
}

#[tokio::test(start_paused = true)]
async fn git_over_ssh_sets_no_shell_run_ssh_command_and_the_launcher_names_the_client_executable() {
    // Git runs GIT_SSH_COMMAND and core.sshCommand through `sh`, which cannot
    // start in the box (lab F-ssh-1): neither may be set, and GIT_SSH must
    // name the launcher script, whose interpreter is the agentdesktop
    // executable, never a shell.
    let h = Harness::start(ssh_setup(Some((&[SSH_HOST_A], &["*"])))).await;
    let captured = capture_at_spawn(&h);

    let _ = h.launch(&h.body()).await;

    let (launch_id, request) = spawned_request(&h);
    let env = env_map(&request);
    assert!(
        !env.keys()
            .any(|key| key.eq_ignore_ascii_case("GIT_SSH_COMMAND")),
        "{env:?}"
    );
    // Nor through the environment-config route (GIT_CONFIG_KEY_<n>).
    assert!(
        !env.iter()
            .any(|(key, value)| key.starts_with("GIT_CONFIG_KEY_")
                && value.eq_ignore_ascii_case("core.sshCommand")),
        "{env:?}"
    );
    let files = spawn_time_files(&captured);
    let git_config = files.git_config.as_deref().expect("git_config at spawn");
    assert!(
        !git_config.to_ascii_lowercase().contains("sshcommand"),
        "the generated gitconfig sets no core.sshCommand: {git_config}"
    );
    assert_eq!(
        env["GIT_SSH"],
        format!("{}\\{GIT_SSH_FILE}", win(&h.launch_dir(&launch_id))),
        "GIT_SSH is the bare launcher path, no arguments"
    );
    assert_eq!(
        files.git_ssh.as_deref(),
        Some("#!/agentdesktop-windows-x64\n"),
        "the launcher is one #! line naming the client executable"
    );
}

#[test]
fn ssh_files_refuse_a_client_executable_name_git_would_cut_and_write_the_launcher_0600() {
    let temp = tempfile::tempdir().unwrap();
    let legs = [(SSH_HOST_A.to_owned(), 2222u16)];
    let long_name = "a".repeat(100);
    for name in ["agent desktop", "", "agent\tdesktop", &long_name] {
        let error = generate_ssh_files(temp.path(), &legs, &[], name)
            .expect_err("a name with whitespace, none, or too long is refused");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("without spaces") || msg.contains("fits the #! line"),
            "{error:#}"
        );
        assert!(
            std::fs::read_dir(temp.path()).unwrap().next().is_none(),
            "nothing is written for {name:?}"
        );
    }

    generate_ssh_files(temp.path(), &legs, &[], "agentdesktop-windows-x64").unwrap();

    assert_eq!(
        std::fs::read_to_string(temp.path().join(GIT_SSH_FILE)).unwrap(),
        "#!/agentdesktop-windows-x64\n"
    );
    if cfg!(unix) {
        assert_eq!(file_mode(&temp.path().join(GIT_SSH_FILE)), Some(0o600));
    }
}

/// A request with the per-harness parts (temp root, launch id, pairing)
/// replaced by placeholders, so two launches compare byte for byte.
fn normalized_request(h: &Harness) -> String {
    let (launch_id, request) = spawned_request(h);
    let env = env_map(&request);
    let pairing = env["ANTHROPIC_CUSTOM_HEADERS"]
        .strip_prefix("x-agentdesktop-pairing: ")
        .unwrap()
        .to_owned();
    let root = h.state_dir.parent().unwrap().to_path_buf();
    let text = serde_json::to_string_pretty(&request).unwrap();
    let windows_root = serde_json::to_string(&win(&root)).unwrap();
    let native_root = serde_json::to_string(&root.display().to_string()).unwrap();
    text.replace(windows_root.trim_matches('"'), "<root>")
        .replace(native_root.trim_matches('"'), "<root>")
        .replace(&launch_id, "<launch_id>")
        .replace(&pairing, "<pairing>")
}

#[tokio::test(start_paused = true)]
async fn without_ssh_hosts_nothing_changes_no_env_no_files_and_identical_request_bytes() {
    // The PR 4 shape: no git block at all, and a git block without ssh.
    let baseline = Harness::start(Setup::default()).await;
    baseline.launch_ok().await;
    let baseline_request = normalized_request(&baseline);

    for ssh in [None, Some((&[][..], &["*"][..]))] {
        let h = Harness::start(ssh_setup(ssh)).await;
        let captured = capture_at_spawn(&h);
        // No ssh surface, so nothing post-spawn can fail: the launch starts.
        h.launch_ok().await;
        let files = spawn_time_files(&captured);
        assert!(files.ssh_config.is_none(), "no ssh_config for {ssh:?}");
        assert!(files.known_hosts.is_none(), "no known_hosts for {ssh:?}");
        let (_, request) = spawned_request(&h);
        let env = env_map(&request);
        assert!(!env.contains_key("SSH_AUTH_SOCK"), "{env:?}");
        assert!(!env.contains_key("GIT_SSH_COMMAND"), "{env:?}");
        assert_eq!(
            normalized_request(&h),
            baseline_request,
            "the request bytes are the PR 4 ones for {ssh:?}"
        );
    }
}

#[cfg(not(windows))]
#[tokio::test(start_paused = true)]
async fn an_ssh_setup_failure_fails_the_launch_stops_the_box_and_closes_the_legs() {
    let h = Harness::start(ssh_setup(Some((&[SSH_HOST_A], &["*"])))).await;
    let captured = capture_at_spawn(&h);

    let (status, body) = h.launch(&h.body()).await;

    // Off Windows there is no pipe transport: the launch must not degrade to
    // a box without ssh (its env already names the pipe).
    assert_eq!(status, 500, "{body}");
    assert_eq!(h.executor.kills(), 1, "the started box is stopped");
    assert!(h.listed().await.is_empty(), "{body}");
    let now = h.credentials.clock.now();
    assert_eq!(h.sessions.len(now), 0, "the session is revoked");
    let launches = h.state_dir.join("launches");
    assert!(
        !launches.exists() || std::fs::read_dir(&launches).unwrap().next().is_none(),
        "no launch directory remains"
    );
    // The legs bound before spawn close with the launch.
    let files = spawn_time_files(&captured);
    assert!(
        !files.ports_in_use.is_empty(),
        "a leg was bound before spawn"
    );
    for (port, _) in &files.ports_in_use {
        let mut closed = false;
        for _ in 0..100 {
            if !port_in_use(*port) {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(closed, "leg port {port} is closed after the failed launch");
    }
}

// =====================================================================
// PR 6a: OpenCode and Copilot CLI launch profiles
// (`specs/PR-6a-harness-profiles.md`, frozen v3). Spec-derived, written
// before the implementation; ledger `specs/PR-6a-test-changes.md`. Every
// test here runs on Linux: the plan works on `\`-joined strings, the probe
// on a temp directory.

use super::profile::{InjectionStrategy, ProgramProfile, program_profile};

const SANDBOX_REFUSAL: &str =
    "argument --sandbox is not available inside the box (the box is the sandbox)";
const OPENCODE_MODEL_REFUSAL: &str =
    "programs.openCode.model must be set (and present in programs.openCode.models) for run";
const COPILOT_MODELS_REFUSAL: &str =
    "programs.copilot.models needs an entry with provider agentdesktop-anthropic for run";
const COPILOT_POLICY_REFUSAL: &str = "Copilot's managed settings require its own sandbox (sandbox.enabled), which cannot run inside the box; launch Copilot unboxed or lift the requirement for boxed devices";
const EGRESS_ALLOW_REFUSAL: &str = "egress allow is supported for claude-code only";

/// G9: the listener refusal for `token` (`serve`, `web`, `--port`, ...).
fn listener_refusal(token: &str) -> String {
    format!(
        "argument {token} is not available inside the box (an in-box listener would expose the session pairing)"
    )
}

/// D4: the missing/mismatched package-cache refusal.
fn pkg_cache_refusal(version: &str, cache_home: &Path) -> String {
    format!(
        "Copilot package cache not found for {version}: run copilot --version once outside the box with COPILOT_PKG_CACHE_HOME={}",
        cache_home.display()
    )
}

/// `<home>\.agentdesktop\harness` of the build_request fixture.
const HARNESS: &str = r"C:\Users\dev\.agentdesktop\harness";
const OPENCODE_MODEL: &str = "claude-sonnet";
const COPILOT_WIRE_MODEL: &str = "claude-sonnet-4-5";

const PROGRAMS_6A: [&str; 3] = ["claude-code", "opencode", "copilot"];

fn profile_of(program: &str) -> &'static ProgramProfile {
    program_profile(program).unwrap_or_else(|| panic!("a profile for {program}"))
}

fn exe_of(program: &str) -> &'static str {
    match program {
        "claude-code" => "claude.exe",
        "opencode" => "opencode.exe",
        "copilot" => "copilot.exe",
        other => panic!("no exe for {other}"),
    }
}

/// D5 (round-2 B1, test-author source re-check at v1.19.3): OpenCode's
/// executable configuration under `config\opencode`. Post-freeze DV-1 adds
/// `tui.jsonc` and the legacy TOML `config` file (imported, merged and
/// rewritten as `config.json` by OpenCode).
const OPENCODE_HOME_PINS: [&str; 20] = [
    "opencode.json",
    "opencode.jsonc",
    "config.json",
    "config",
    "tui.json",
    "tui.jsonc",
    "AGENTS.md",
    "agents",
    "agent",
    "commands",
    "command",
    "modes",
    "mode",
    "plugins",
    "plugin",
    "skills",
    "skill",
    "tools",
    "tool",
    "themes",
];

/// D5: Copilot's executable configuration under `COPILOT_HOME` (config-dir
/// reference); `config.json` is written by Copilot itself and not a pin.
const COPILOT_HOME_PINS: [&str; 11] = [
    "settings.json",
    "mcp-config.json",
    "lsp-config.json",
    "providers.json",
    "copilot-instructions.md",
    "instructions",
    "agents",
    "skills",
    "hooks",
    "extensions",
    "installed-plugins",
];

/// D5 pins of `program` as full `\`-joined paths of the fixture.
fn home_pins(program: &str) -> Vec<String> {
    match program {
        "claude-code" => ["settings.json", "hooks", "plugins"]
            .iter()
            .map(|entry| format!(r"{HOME}\.claude\{entry}"))
            .collect(),
        "opencode" => OPENCODE_HOME_PINS
            .iter()
            .map(|entry| format!(r"{HARNESS}\opencode\config\opencode\{entry}"))
            .collect(),
        "copilot" => COPILOT_HOME_PINS
            .iter()
            .map(|entry| format!(r"{HARNESS}\copilot\{entry}"))
            .collect(),
        other => panic!("{other}"),
    }
}

/// DV-10: the harness-home pins the daemon pre-creates as `{}` (absolute,
/// in the request's form). OpenCode's global trio and Copilot CLI's BYOK
/// provider registry.
fn pre_created_pins(program: &str) -> Vec<String> {
    match program {
        "opencode" => ["config.json", "opencode.json", "opencode.jsonc"]
            .iter()
            .map(|entry| format!(r"{HARNESS}\opencode\config\opencode\{entry}"))
            .collect(),
        "copilot" => vec![format!(r"{HARNESS}\copilot\providers.json")],
        "claude-code" => Vec::new(),
        other => panic!("{other}"),
    }
}

/// D9: the union workspace pins at the fixture workspace (`.github`
/// entries included; the plan denies `.github` whole when it is absent).
fn union_pins() -> Vec<String> {
    [
        r".claude\settings.json",
        r".claude\settings.local.json",
        r".claude\hooks",
        ".mcp.json",
        "HEAD",
        r".git\config",
        r".git\hooks",
        r".git\commondir",
        "opencode.json",
        "opencode.jsonc",
        ".opencode",
        r".github\copilot",
        r".github\mcp.json",
        r".github\hooks",
        r".github\agents",
    ]
    .iter()
    .map(|entry| format!(r"{CWD}\{entry}"))
    .collect()
}

/// The harness directories the 6a scenarios have on disk.
fn harness_dirs(program: &str) -> Vec<String> {
    let mut dirs = vec![r"C:\Users\dev\.agentdesktop".to_owned(), HARNESS.to_owned()];
    match program {
        "opencode" => {
            for sub in [
                "",
                r"\data",
                r"\config",
                r"\state",
                r"\cache",
                r"\config\opencode",
                r"\cache\opencode",
                r"\cache\opencode\bin",
            ] {
                dirs.push(format!(r"{HARNESS}\opencode{sub}"));
            }
        }
        "copilot" => {
            dirs.push(format!(r"{HARNESS}\copilot"));
            dirs.push(format!(r"{HARNESS}\copilot-pkg"));
        }
        _ => dirs.push(format!(r"{HOME}\.claude")),
    }
    dirs
}

fn existing_6a(program: &str) -> BTreeSet<String> {
    let mut existing = snapshot_existing();
    existing.extend(harness_dirs(program));
    existing
}

fn directories_6a(program: &str) -> BTreeSet<String> {
    let mut directories = snapshot_directories();
    directories.extend(harness_dirs(program));
    directories
}

fn model_6a(program: &str) -> Option<&'static str> {
    match program {
        "opencode" => Some(OPENCODE_MODEL),
        "copilot" => Some(COPILOT_WIRE_MODEL),
        _ => None,
    }
}

/// `plan_program` for `program` on the build_request fixture, with egress
/// deny (D1b) and the program's exe in `<home>\.local\bin`.
fn plan_6a(
    program: &str,
    existing: &BTreeSet<String>,
    directories: &BTreeSet<String>,
    tweak: impl FnOnce(&mut LaunchInput),
) -> plan::LaunchPlan {
    let exists = |path: &Path| existing.contains(path.to_str().expect("UTF-8 path"));
    let is_dir = |path: &Path| directories.contains(path.to_str().expect("UTF-8 path"));
    let mut input = launch_input(&exists, &is_dir);
    input.executable = PathBuf::from(format!(r"{HOME}\.local\bin\{}", exe_of(program)));
    input.egress = agentdesktop_core::config::ContainmentEgress::Deny;
    tweak(&mut input);
    plan_program(&input, profile_of(program), model_6a(program))
}

/// The fixture's arguments are Claude Code flags; the other programs get
/// none.
fn default_plan_6a(program: &str) -> plan::LaunchPlan {
    plan_6a(
        program,
        &existing_6a(program),
        &directories_6a(program),
        |input| {
            if program != "claude-code" {
                input.args.clear();
            }
        },
    )
}

fn strings(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|path| win(path)).collect()
}

fn is_under_str(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}\\"))
}

/// Read-only or denied: the executable-configuration rule.
fn protected_6a(plan: &plan::LaunchPlan) -> BTreeSet<String> {
    let mut protected: BTreeSet<String> = strings(&plan.readonly).into_iter().collect();
    protected.extend(strings(&plan.denied));
    protected
}

fn writable_6a(plan: &plan::LaunchPlan, path: &str) -> bool {
    strings(&plan.readwrite)
        .iter()
        .any(|root| is_under_str(path, root))
}

fn yaml_config(text: &str) -> agentdesktop_core::config::DaemonConfig {
    serde_yaml::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

const GATEWAY_6A: &str = "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\n";

fn with_gateway(programs: &str) -> agentdesktop_core::config::DaemonConfig {
    yaml_config(&format!("{GATEWAY_6A}{programs}"))
}

const DENY: agentdesktop_core::config::ContainmentEgress =
    agentdesktop_core::config::ContainmentEgress::Deny;
const ALLOW: agentdesktop_core::config::ContainmentEgress =
    agentdesktop_core::config::ContainmentEgress::Allow;

// ------------------------------------------------- ProgramProfile table (D1)

#[test]
fn the_profile_table_has_exactly_the_three_programs() {
    for program in PROGRAMS_6A {
        assert_eq!(profile_of(program).program, program);
    }
    for other in ["codex", "claude", "copilot-cli", "OpenCode", ""] {
        assert!(program_profile(other).is_none(), "{other:?}");
    }
}

#[test]
fn the_claude_code_profile_row_keeps_claude_s_executable_injection_and_title() {
    let claude = profile_of("claude-code");
    assert_eq!(claude.exe_name, "claude.exe");
    assert_eq!(claude.home_candidates, &[r".local\bin"]);
    assert_eq!(
        claude.injection,
        InjectionStrategy::Env {
            base_url: "ANTHROPIC_BASE_URL",
            headers: "ANTHROPIC_CUSTOM_HEADERS",
        }
    );
    for pair in super::profile::CLAUDE_CODE.fixed_env {
        assert!(claude.fixed_env.contains(pair), "{pair:?} moved away");
    }
    assert!(claude.harness_home.is_none());
    assert!(claude.denied_args.is_empty(), "{:?}", claude.denied_args);
    assert!(
        claude.denied_tokens.is_empty(),
        "{:?}",
        claude.denied_tokens
    );
    assert_eq!(claude.window_title, "agentdesktop: claude");
}

#[test]
fn the_opencode_profile_row_names_its_exe_inline_config_harness_dirs_and_title() {
    let opencode = profile_of("opencode");
    assert_eq!(opencode.exe_name, "opencode.exe");
    assert_eq!(opencode.home_candidates, &[r".local\bin"]);
    assert_eq!(opencode.injection, InjectionStrategy::OpenCodeInlineConfig);
    for pair in [
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
    ] {
        assert!(opencode.fixed_env.contains(&pair), "{pair:?} (D8)");
    }
    let home = opencode.harness_home.as_ref().expect("a harness home (D5)");
    assert_eq!(home.dir, "opencode");
    let env_dirs: BTreeSet<(&str, &str)> = home.env_dirs.iter().copied().collect();
    assert_eq!(
        env_dirs,
        BTreeSet::from([
            ("XDG_DATA_HOME", r"opencode\data"),
            ("XDG_CONFIG_HOME", r"opencode\config"),
            ("XDG_STATE_HOME", r"opencode\state"),
            ("XDG_CACHE_HOME", r"opencode\cache"),
        ])
    );
    for flag in ["--port", "--hostname", "--mdns"] {
        assert!(opencode.denied_args.contains(&flag), "{flag} (G9)");
    }
    assert_eq!(opencode.denied_tokens, &["serve", "web"]);
    assert_eq!(opencode.window_title, "agentdesktop: opencode");
}

#[test]
fn the_copilot_profile_row_names_its_exe_byok_env_harness_dirs_and_title() {
    let copilot = profile_of("copilot");
    assert_eq!(copilot.exe_name, "copilot.exe");
    assert_eq!(copilot.home_candidates, &[r".local\bin"]);
    assert_eq!(
        copilot.injection,
        InjectionStrategy::Env {
            base_url: "COPILOT_PROVIDER_BASE_URL",
            headers: "COPILOT_PROVIDER_HEADERS",
        }
    );
    for pair in [
        ("COPILOT_PROVIDER_TYPE", "anthropic"),
        ("COPILOT_PROVIDER_API_KEY", "placeholder"),
        ("COPILOT_AUTO_UPDATE", "false"),
    ] {
        assert!(copilot.fixed_env.contains(&pair), "{pair:?} (D2b, D8)");
    }
    assert!(
        !copilot
            .fixed_env
            .iter()
            .any(|(key, _)| *key == "COPILOT_OFFLINE"),
        "COPILOT_OFFLINE stays unset (D8)"
    );
    let home = copilot.harness_home.as_ref().expect("a harness home (D5)");
    assert_eq!(home.dir, "copilot");
    let env_dirs: BTreeSet<(&str, &str)> = home.env_dirs.iter().copied().collect();
    assert_eq!(
        env_dirs,
        BTreeSet::from([
            ("COPILOT_HOME", "copilot"),
            ("COPILOT_PKG_CACHE_HOME", "copilot-pkg"),
        ])
    );
    assert_eq!(copilot.denied_args, &["--sandbox"]);
    assert!(
        copilot.denied_tokens.is_empty(),
        "{:?}",
        copilot.denied_tokens
    );
    assert_eq!(copilot.window_title, "agentdesktop: copilot");
}

#[test]
fn no_non_claude_profile_carries_claude_or_anthropic_fixed_env() {
    for program in ["opencode", "copilot"] {
        for (key, _) in profile_of(program).fixed_env {
            assert!(
                !key.starts_with("CLAUDE_") && !key.starts_with("ANTHROPIC_"),
                "{program}: {key} (AC6)"
            );
        }
    }
}

#[test]
fn the_opencode_harness_home_pins_cover_the_global_config_files_and_executable_directories() {
    let home = profile_of("opencode").harness_home.as_ref().unwrap();
    for entry in OPENCODE_HOME_PINS {
        let pin = format!(r"opencode\config\opencode\{entry}");
        assert!(
            home.executable_config.contains(&pin.as_str()),
            "{pin} (D5, B1)"
        );
    }
}

/// DV-1: a planted `tui.jsonc` or legacy `config` file is executable
/// config like `opencode.json`; both are denied in a fresh harness home.
/// DV-4: there is no singular `theme` directory to pin.
#[test]
fn the_opencode_box_denies_a_planted_tui_jsonc_and_legacy_config_file() {
    let plan = default_plan_6a("opencode");
    let denied = strings(&plan.denied);
    for entry in ["tui.jsonc", "config"] {
        let pin = format!(r"{HARNESS}\opencode\config\opencode\{entry}");
        assert!(denied.contains(&pin), "{pin} (DV-1)");
        assert!(recursive_roots("opencode").contains(&pin), "{pin} (D12)");
    }
    let home = profile_of("opencode").harness_home.as_ref().unwrap();
    assert!(
        !home
            .executable_config
            .contains(&r"opencode\config\opencode\theme"),
        "DV-4"
    );
}

#[test]
fn the_copilot_harness_home_pins_cover_the_config_dir_reference_entries() {
    let home = profile_of("copilot").harness_home.as_ref().unwrap();
    for entry in COPILOT_HOME_PINS {
        let pin = format!(r"copilot\{entry}");
        assert!(home.executable_config.contains(&pin.as_str()), "{pin} (D5)");
    }
    assert!(
        !home.executable_config.contains(&r"copilot\config.json"),
        "config.json is denied, not pinned read-only (D5)"
    );
}

// ------------------------------------------------- resolve_program (D3)

#[test]
fn resolve_program_for_claude_code_matches_resolve_program_on_every_existing_case() {
    let home = p("home/dev");
    let npm = p("npm");
    let own = home.join(".local").join("bin").join("claude.exe");
    let (first, second, third) = (p("a"), p("b"), p("c"));
    let mut outside = program_fs(std::slice::from_ref(&own));
    outside
        .canonical
        .insert(own.clone(), p("home/other/claude.exe"));
    let cases: Vec<(FakeProgramFs, Vec<PathBuf>)> = vec![
        (
            program_fs(&[
                npm.join("claude.cmd"),
                npm.join("claude"),
                home.join(".local/bin/claude.cmd"),
            ]),
            vec![npm.clone()],
        ),
        (
            program_fs(&[p("home/other/.local/bin/claude.exe")]),
            vec![p("windows/system32")],
        ),
        (outside, vec![]),
        (
            program_fs(&[npm.join("claude.cmd"), npm.join("claude"), own.clone()]),
            vec![npm.clone()],
        ),
        (
            program_fs(&[p("tools").join("claude.exe"), own.clone()]),
            vec![p("tools")],
        ),
        (
            program_fs(&[second.join("claude.exe"), third.join("claude.exe")]),
            vec![first, second, third],
        ),
        (program_fs(&[]), vec![p("tools")]),
    ];
    for (fs, path_dirs) in cases {
        assert_eq!(
            resolve_program_for(profile_of("claude-code"), &home, &path_dirs, &fs),
            resolve_program(&home, &path_dirs, &fs),
            "{path_dirs:?}"
        );
    }
}

#[test]
fn each_program_resolves_its_native_exe_from_home_before_path() {
    let home = p("home/dev");
    let tools = p("tools");
    for program in PROGRAMS_6A {
        let exe = exe_of(program);
        let own = home.join(".local").join("bin").join(exe);
        let fs = program_fs(&[tools.join(exe), own.clone()]);
        assert_eq!(
            resolve_program_for(
                profile_of(program),
                &home,
                std::slice::from_ref(&tools),
                &fs
            ),
            Ok(own),
            "{program}"
        );
    }
}

#[test]
fn each_program_falls_back_to_the_first_path_directory_with_its_exe() {
    let home = p("home/dev");
    let (first, second, third) = (p("a"), p("b"), p("c"));
    for program in PROGRAMS_6A {
        let exe = exe_of(program);
        let fs = program_fs(&[second.join(exe), third.join(exe)]);
        assert_eq!(
            resolve_program_for(
                profile_of(program),
                &home,
                &[first.clone(), second.clone(), third.clone()],
                &fs
            ),
            Ok(second.join(exe)),
            "{program}"
        );
    }
}

#[test]
fn npm_shims_of_opencode_and_copilot_are_not_found_and_the_error_names_the_exe() {
    let home = p("home/dev");
    let npm = p("npm");
    for program in ["opencode", "copilot"] {
        let stem = exe_of(program).trim_end_matches(".exe");
        let fs = program_fs(&[
            npm.join(format!("{stem}.cmd")),
            npm.join(format!("{stem}.ps1")),
            npm.join(stem),
            home.join(".local/bin").join(format!("{stem}.cmd")),
        ]);
        let error =
            resolve_program_for(profile_of(program), &home, std::slice::from_ref(&npm), &fs)
                .expect_err(program);
        assert!(error.contains(exe_of(program)), "{program}: {error}");
    }
}

#[test]
fn one_programs_exe_never_resolves_another_program() {
    let home = p("home/dev");
    let bin = home.join(".local").join("bin");
    for program in PROGRAMS_6A {
        let others: Vec<PathBuf> = PROGRAMS_6A
            .iter()
            .filter(|other| **other != program)
            .map(|other| bin.join(exe_of(other)))
            .collect();
        let fs = program_fs(&others);
        assert!(
            resolve_program_for(profile_of(program), &home, &[], &fs).is_err(),
            "{program}"
        );
    }
}

#[test]
fn a_candidate_that_canonicalises_outside_home_and_path_is_not_used_for_any_program() {
    let home = p("home/dev");
    for program in PROGRAMS_6A {
        let candidate = home.join(".local").join("bin").join(exe_of(program));
        let mut fs = program_fs(std::slice::from_ref(&candidate));
        fs.canonical
            .insert(candidate, p("home/other").join(exe_of(program)));
        assert!(
            resolve_program_for(profile_of(program), &home, &[], &fs).is_err(),
            "{program}"
        );
    }
}

// ------------------------------------------------- denied args (D6, G9)

#[test]
fn copilot_refuses_any_argument_equal_to_or_prefixed_by_sandbox() {
    for list in [
        vec!["--sandbox"],
        vec!["--sandbox=true"],
        vec!["--sandbox-mode", "workspace"],
        vec!["-p", "hello", "--sandbox"],
    ] {
        assert_eq!(
            check_denied_args(profile_of("copilot"), &args(&list)),
            Err(SANDBOX_REFUSAL.to_owned()),
            "{list:?}"
        );
    }
}

#[test]
fn opencode_refuses_serve_and_web_wherever_they_appear_as_bare_tokens() {
    for (list, token) in [
        (vec!["serve"], "serve"),
        (vec!["web"], "web"),
        (vec!["--print-logs", "serve"], "serve"),
        (vec!["run", "--continue", "web"], "web"),
    ] {
        assert_eq!(
            check_denied_args(profile_of("opencode"), &args(&list)),
            Err(listener_refusal(token)),
            "{list:?}"
        );
    }
}

#[test]
fn opencode_refuses_port_hostname_and_mdns_by_prefix() {
    for (list, flag) in [
        (vec!["--port", "4096"], "--port"),
        (vec!["--port=4096"], "--port"),
        (vec!["--hostname", "0.0.0.0"], "--hostname"),
        (vec!["--hostname=127.0.0.1"], "--hostname"),
        (vec!["--mdns"], "--mdns"),
        (vec!["--mdns-domain", "x.local"], "--mdns"),
    ] {
        assert_eq!(
            check_denied_args(profile_of("opencode"), &args(&list)),
            Err(listener_refusal(flag)),
            "{list:?}"
        );
    }
}

/// G9 fail-closed rule: no flag-arity parsing, so a space-separated flag
/// value equal to `serve` is refused; a value glued with `=`, or one that
/// merely starts with `serve`, is not a bare token.
#[test]
fn opencode_refuses_a_space_separated_flag_value_equal_to_serve_but_not_a_glued_one() {
    assert_eq!(
        check_denied_args(profile_of("opencode"), &args(&["--model", "serve"])),
        Err(listener_refusal("serve"))
    );
    for list in [
        vec!["--model=serve"],
        vec!["--model", "serve/x"],
        vec!["--model=web"],
    ] {
        assert_eq!(
            check_denied_args(profile_of("opencode"), &args(&list)),
            Ok(()),
            "{list:?}"
        );
    }
}

#[test]
fn the_listener_and_sandbox_denials_are_per_profile() {
    assert_eq!(
        check_denied_args(profile_of("opencode"), &args(&["--sandbox"])),
        Ok(())
    );
    for list in [vec!["serve"], vec!["web"], vec!["--port", "1"]] {
        assert_eq!(
            check_denied_args(profile_of("copilot"), &args(&list)),
            Ok(()),
            "{list:?}"
        );
    }
    for list in [vec!["serve"], vec!["--sandbox"], vec!["--port", "1"]] {
        assert_eq!(
            check_denied_args(profile_of("claude-code"), &args(&list)),
            Ok(()),
            "Claude has no denied args (D6): {list:?}"
        );
    }
}

#[test]
fn ordinary_arguments_pass_every_profile() {
    for program in PROGRAMS_6A {
        for list in [
            vec![],
            vec!["--model", "anthropic/claude-sonnet"],
            vec!["-p", "fix the tests"],
            vec!["--print-logs"],
        ] {
            assert_eq!(
                check_denied_args(profile_of(program), &args(&list)),
                Ok(()),
                "{program}: {list:?}"
            );
        }
    }
}

// ------------------------------------------------- preconditions (AC4, D1b, D11)

#[test]
fn claude_code_preconditions_keep_their_existing_texts() {
    let cases = [
        (
            yaml_config("programs:\n  claudeCode: {}\n"),
            "llmGateway (with authentication) must be configured for run",
        ),
        (
            yaml_config(
                "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  claudeCode: {}\n",
            ),
            "llmGateway.authentication must be configured for run",
        ),
        (
            with_gateway(""),
            "programs.claudeCode must be configured for run",
        ),
        (
            with_gateway("programs:\n  claudeCode:\n    useLlmGateway: false\n"),
            "programs.claudeCode.useLlmGateway must be true for run",
        ),
        (
            with_gateway("programs:\n  claudeCode:\n    auth: subscription\n"),
            "programs.claudeCode.auth: subscription sign-in is not available inside the box in PR 1",
        ),
        (
            with_gateway("programs:\n  claudeCode: {}\nsandbox: {}\n"),
            "Claude Code's own sandbox is not available inside the box; remove `sandbox` or launch without `run`",
        ),
    ];
    for (config, text) in cases {
        assert_eq!(
            program_preconditions(&config, "claude-code", DENY),
            Err(text.to_owned())
        );
        assert_eq!(
            preconditions(&config),
            Err(text.to_owned()),
            "the old rule agrees"
        );
    }
    assert_eq!(
        program_preconditions(
            &with_gateway("programs:\n  claudeCode: {}\n"),
            "claude-code",
            DENY
        ),
        Ok(())
    );
}

const VALID_6A_PROGRAMS: &str = "programs:\n  openCode:\n    model: claude-sonnet\n    models:\n      claude-sonnet: {}\n  copilot:\n    models:\n      sonnet:\n        provider: agentdesktop-anthropic\n";

#[test]
fn valid_opencode_and_copilot_configurations_pass_under_deny() {
    let config = with_gateway(VALID_6A_PROGRAMS);
    for program in ["opencode", "copilot"] {
        assert_eq!(
            program_preconditions(&config, program, DENY),
            Ok(()),
            "{program}"
        );
    }
}

#[test]
fn claude_code_is_the_only_program_allowed_under_egress_allow() {
    let config = with_gateway(&format!("{VALID_6A_PROGRAMS}  claudeCode: {{}}\n"));
    assert_eq!(program_preconditions(&config, "claude-code", ALLOW), Ok(()));
    for program in ["opencode", "copilot"] {
        assert_eq!(
            program_preconditions(&config, program, ALLOW),
            Err(EGRESS_ALLOW_REFUSAL.to_owned()),
            "{program} (D1b)"
        );
    }
}

#[test]
fn a_missing_program_section_names_it_for_opencode_and_copilot() {
    let config = with_gateway("programs:\n  claudeCode: {}\n");
    for (program, field) in [
        ("opencode", "programs.openCode"),
        ("copilot", "programs.copilot"),
    ] {
        let error = program_preconditions(&config, program, DENY).expect_err(program);
        assert!(error.contains(field), "{program}: {error}");
    }
}

#[test]
fn use_llm_gateway_false_names_the_field_for_opencode_and_copilot() {
    let config = with_gateway(
        "programs:\n  openCode:\n    useLlmGateway: false\n    model: claude-sonnet\n    models:\n      claude-sonnet: {}\n  copilot:\n    useLlmGateway: false\n    models:\n      sonnet:\n        provider: agentdesktop-anthropic\n",
    );
    for (program, field) in [
        ("opencode", "programs.openCode.useLlmGateway"),
        ("copilot", "programs.copilot.useLlmGateway"),
    ] {
        let error = program_preconditions(&config, program, DENY).expect_err(program);
        assert!(error.contains(field), "{program}: {error}");
    }
}

#[test]
fn opencode_without_a_usable_model_answers_the_model_text() {
    for programs in [
        "programs:\n  openCode:\n    models:\n      claude-sonnet: {}\n",
        "programs:\n  openCode:\n    model: claude-opus\n    models:\n      claude-sonnet: {}\n",
        "programs:\n  openCode:\n    model: claude-sonnet\n",
    ] {
        assert_eq!(
            program_preconditions(&with_gateway(programs), "opencode", DENY),
            Err(OPENCODE_MODEL_REFUSAL.to_owned()),
            "{programs}"
        );
    }
}

/// Fix round 1 (DV-8): an empty `programs.openCode.model` is refused like an
/// unset one, even when `models` has an empty key; no plan is built with
/// `"model":"anthropic/"`.
#[test]
fn an_empty_opencode_model_answers_the_model_text_like_an_unset_one() {
    for programs in [
        "programs:\n  openCode:\n    model: \"\"\n    models:\n      claude-sonnet: {}\n",
        "programs:\n  openCode:\n    model: \"\"\n    models:\n      \"\": {}\n",
    ] {
        assert_eq!(
            program_preconditions(&with_gateway(programs), "opencode", DENY),
            Err(OPENCODE_MODEL_REFUSAL.to_owned()),
            "{programs}"
        );
    }
}

#[test]
fn copilot_without_an_anthropic_model_entry_answers_the_models_text() {
    for programs in [
        "programs:\n  copilot: {}\n",
        "programs:\n  copilot:\n    models:\n      gpt:\n        provider: agentdesktop\n",
    ] {
        assert_eq!(
            program_preconditions(&with_gateway(programs), "copilot", DENY),
            Err(COPILOT_MODELS_REFUSAL.to_owned()),
            "{programs}"
        );
    }
}

#[test]
fn copilot_with_a_non_string_wire_model_answers_the_wire_model_text() {
    let config = with_gateway(
        "programs:\n  copilot:\n    models:\n      sonnet:\n        provider: agentdesktop-anthropic\n        wireModel: 5\n",
    );
    assert_eq!(
        program_preconditions(&config, "copilot", DENY),
        Err("programs.copilot.models entry sonnet: wireModel must be a string".to_owned())
    );
}

#[test]
fn opencode_and_copilot_need_the_gateway_with_authentication() {
    for config in [
        yaml_config(VALID_6A_PROGRAMS),
        yaml_config(&format!(
            "llmGateway:\n  url: https://gateway.example.com\n{VALID_6A_PROGRAMS}"
        )),
    ] {
        for program in ["opencode", "copilot"] {
            let error = program_preconditions(&config, program, DENY).expect_err(program);
            assert!(error.contains("llmGateway"), "{program}: {error}");
        }
    }
}

/// D6, D11: the subscription refusal and the `sandbox` precondition are
/// Claude's; `SandboxConfig` is not consumed by the other two.
#[test]
fn subscription_and_the_top_level_sandbox_refusals_stay_claude_only() {
    let config = with_gateway(&format!(
        "{VALID_6A_PROGRAMS}  claudeCode:\n    auth: subscription\nsandbox: {{}}\n"
    ));
    for program in ["opencode", "copilot"] {
        assert_eq!(
            program_preconditions(&config, program, DENY),
            Ok(()),
            "{program}"
        );
    }
}

#[test]
fn an_unknown_program_has_no_preconditions_path() {
    let config = with_gateway(VALID_6A_PROGRAMS);
    assert!(program_preconditions(&config, "codex", DENY).is_err());
}

// ------------------------------------------------- Copilot model (D2b, R5)

fn copilot_models(models: &str) -> agentdesktop_core::config::DaemonConfig {
    with_gateway(&format!("programs:\n  copilot:\n    models:\n{models}"))
}

#[test]
fn copilot_wire_model_takes_the_first_anthropic_entry_in_key_order() {
    let config = copilot_models(
        "      a-gpt:\n        provider: agentdesktop\n        modelId: gpt-5\n      b-sonnet:\n        provider: agentdesktop-anthropic\n        modelId: m-b\n      c-opus:\n        provider: agentdesktop-anthropic\n        modelId: m-c\n",
    );
    assert_eq!(copilot_wire_model(&config), Ok("m-b".to_owned()));
}

#[test]
fn copilot_wire_model_prefers_a_string_wire_model_then_model_id_then_the_key() {
    let wire = copilot_models(
        "      sonnet:\n        provider: agentdesktop-anthropic\n        modelId: m\n        wireModel: w\n",
    );
    assert_eq!(copilot_wire_model(&wire), Ok("w".to_owned()));
    let model_id = copilot_models(
        "      sonnet:\n        provider: agentdesktop-anthropic\n        modelId: m\n",
    );
    assert_eq!(copilot_wire_model(&model_id), Ok("m".to_owned()));
    let key = copilot_models("      sonnet:\n        provider: agentdesktop-anthropic\n");
    assert_eq!(copilot_wire_model(&key), Ok("sonnet".to_owned()));
}

#[test]
fn copilot_wire_model_refuses_a_non_string_wire_model() {
    for value in ["5", "true", "[a]", "{x: 1}"] {
        let config = copilot_models(&format!(
            "      sonnet:\n        provider: agentdesktop-anthropic\n        wireModel: {value}\n"
        ));
        assert_eq!(
            copilot_wire_model(&config),
            Err("programs.copilot.models entry sonnet: wireModel must be a string".to_owned()),
            "{value}"
        );
    }
}

#[test]
fn copilot_wire_model_without_an_anthropic_entry_names_the_models_field() {
    let config = copilot_models("      gpt:\n        provider: agentdesktop\n");
    assert_eq!(
        copilot_wire_model(&config),
        Err(COPILOT_MODELS_REFUSAL.to_owned())
    );
    assert_eq!(
        copilot_wire_model(&with_gateway("programs:\n  claudeCode: {}\n")),
        Err(COPILOT_MODELS_REFUSAL.to_owned())
    );
}

// ------------------------------------------------- OPENCODE_CONFIG_CONTENT (D2a, N6, AC2)

/// The inline config snapshot, with the build_request fixture's relay URL
/// and placeholder pairing.
fn opencode_inline_snapshot() -> serde_json::Value {
    serde_json::json!({
        "provider": {
            "anthropic": {
                "options": {
                    "baseURL": "http://127.0.0.1:4000/sandbox/v1",
                    "apiKey": "placeholder",
                    "headers": {"x-agentdesktop-pairing": "test-pairing-value"},
                },
                "models": {"claude-sonnet": {}},
            },
        },
        "model": "anthropic/claude-sonnet",
        "enabled_providers": ["anthropic"],
        "autoupdate": false,
        "share": "disabled",
    })
}

#[test]
fn the_opencode_inline_config_matches_its_snapshot() {
    assert_eq!(
        opencode_config_content(
            "http://127.0.0.1:4000/sandbox",
            "test-pairing-value",
            OPENCODE_MODEL
        ),
        opencode_inline_snapshot()
    );
}

#[test]
fn the_opencode_box_carries_the_inline_config_in_opencode_config_content() {
    let plan = default_plan_6a("opencode");
    let content = plan
        .env
        .get("OPENCODE_CONFIG_CONTENT")
        .expect("OPENCODE_CONFIG_CONTENT (D2a)");
    let parsed: serde_json::Value = serde_json::from_str(content).expect("JSON");
    assert_eq!(parsed, opencode_inline_snapshot());
    assert!(
        !plan.env.contains_key("OPENCODE_CONFIG"),
        "inline is primary; no config file (D2a)"
    );
}

// ------------------------------------------------- Copilot pre-flight (D7, AC3)

#[test]
fn a_local_copilot_policy_requiring_the_sandbox_is_refused_with_the_two_modes() {
    for policy in [
        serde_json::json!({"sandbox": {"enabled": true}}),
        serde_json::json!({"sandbox": {"enabled": true, "failIfUnavailable": true}}),
        serde_json::json!({"sandbox": {"enabled": true, "allowBypass": false}}),
    ] {
        assert_eq!(
            copilot_sandbox_preflight(&[serde_json::json!({}), policy.clone()]),
            Err(COPILOT_POLICY_REFUSAL.to_owned()),
            "{policy}"
        );
    }
}

#[test]
fn copilot_policies_without_sandbox_enabled_pass() {
    assert_eq!(copilot_sandbox_preflight(&[]), Ok(()));
    for policy in [
        serde_json::json!({}),
        serde_json::json!({"sandbox": {"enabled": false}}),
        serde_json::json!({"sandbox": {}}),
        serde_json::json!({"model": "x"}),
    ] {
        assert_eq!(
            copilot_sandbox_preflight(std::slice::from_ref(&policy)),
            Ok(()),
            "{policy}"
        );
    }
}

/// Fix round 1 (m5): registry value names are case-insensitive, so a
/// policy written `Sandbox.Enabled`, or `Enabled` under a `Sandbox` key, is
/// the same policy.
#[test]
fn registry_policy_names_are_matched_case_insensitively() {
    for values in [
        vec![(None, "Sandbox.Enabled".to_owned(), serde_json::json!(1))],
        vec![(Some("sandbox"), "Enabled".to_owned(), serde_json::json!(1))],
        vec![(
            None,
            "SANDBOX.enabled".to_owned(),
            serde_json::json!("true"),
        )],
        vec![(None, "sandbox.enabled".to_owned(), serde_json::json!(1))],
    ] {
        let document =
            serde_json::Value::Object(super::host::registry_policy_document(values.clone()));
        assert_eq!(
            copilot_sandbox_preflight(&[document]),
            Err(COPILOT_POLICY_REFUSAL.to_owned()),
            "{values:?}"
        );
    }
    let off = serde_json::Value::Object(super::host::registry_policy_document([(
        Some("sandbox"),
        "Enabled".to_owned(),
        serde_json::json!(0),
    )]));
    assert_eq!(copilot_sandbox_preflight(&[off]), Ok(()));
}

// ------------------------------------------------- pkg-cache probe (D4, G8, AC5)

/// Creates `<cache>\pkg\win32-x64\<version>` with one file in it (or empty).
fn prime_pkg_cache(cache_home: &Path, version: &str, with_file: bool) -> PathBuf {
    let dir = cache_home.join("pkg").join("win32-x64").join(version);
    std::fs::create_dir_all(&dir).unwrap();
    if with_file {
        std::fs::write(dir.join("index.js"), b"// extracted").unwrap();
    }
    dir
}

#[test]
fn the_pkg_cache_probe_accepts_the_directory_of_the_exe_version() {
    let temp = tempfile::tempdir().unwrap();
    let dir = prime_pkg_cache(temp.path(), "1.0.101", true);
    prime_pkg_cache(temp.path(), "1.0.99", true);
    assert_eq!(
        probe_copilot_pkg_cache(temp.path(), Some("1.0.101")),
        Ok(dir)
    );
}

/// G8 (round 3 F5): the first three dot-separated parts of the PE
/// ProductVersion are compared with the directory name; a fourth is ignored.
#[test]
fn the_pkg_cache_probe_compares_the_first_three_version_parts() {
    let temp = tempfile::tempdir().unwrap();
    let dir = prime_pkg_cache(temp.path(), "1.0.101", true);
    assert_eq!(
        probe_copilot_pkg_cache(temp.path(), Some("1.0.101.0")),
        Ok(dir)
    );
}

#[test]
fn a_missing_pkg_cache_names_the_priming_command() {
    let temp = tempfile::tempdir().unwrap();
    let cache_home = temp.path().join("copilot-pkg");
    assert_eq!(
        probe_copilot_pkg_cache(&cache_home, Some("1.0.101")),
        Err(pkg_cache_refusal("1.0.101", &cache_home))
    );
}

#[test]
fn a_mismatched_pkg_cache_names_the_priming_command() {
    let temp = tempfile::tempdir().unwrap();
    prime_pkg_cache(temp.path(), "1.0.100", true);
    assert_eq!(
        probe_copilot_pkg_cache(temp.path(), Some("1.0.101.0")),
        Err(pkg_cache_refusal("1.0.101", temp.path()))
    );
}

#[test]
fn an_empty_version_directory_does_not_count_as_a_cache() {
    let temp = tempfile::tempdir().unwrap();
    prime_pkg_cache(temp.path(), "1.0.101", false);
    assert_eq!(
        probe_copilot_pkg_cache(temp.path(), Some("1.0.101")),
        Err(pkg_cache_refusal("1.0.101", temp.path()))
    );
}

#[test]
fn without_a_pe_version_exactly_one_cache_directory_is_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let dir = prime_pkg_cache(temp.path(), "1.0.101", true);
    assert_eq!(probe_copilot_pkg_cache(temp.path(), None), Ok(dir));
}

#[test]
fn without_a_pe_version_two_cache_directories_or_none_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    assert!(probe_copilot_pkg_cache(temp.path(), None).is_err(), "none");
    prime_pkg_cache(temp.path(), "1.0.100", true);
    prime_pkg_cache(temp.path(), "1.0.101", true);
    let error = probe_copilot_pkg_cache(temp.path(), None).expect_err("two dirs");
    assert!(
        error.contains("run copilot --version once outside the box with COPILOT_PKG_CACHE_HOME="),
        "{error}"
    );
}

// ------------------------------------------------- validate_cwd (AC7)

#[test]
fn cwd_may_not_be_under_the_agentdesktop_harness_directory() {
    for cwd in [
        p("home/dev/.agentdesktop"),
        p("home/dev/.agentdesktop/harness"),
        p("home/dev/.agentdesktop/harness/opencode/config"),
    ] {
        let error = validate_cwd(&cwd, &cwd_context(), &FakeFs::default())
            .expect_err(&cwd.display().to_string());
        assert!(error.contains("agentdesktop harness directory"), "{error}");
    }
}

#[test]
fn cwd_may_not_contain_the_agentdesktop_harness_directory_through_a_junction() {
    let mut fs = FakeFs::default();
    fs.mapping.insert(p("home/dev/.agentdesktop"), p("data/ad"));
    let error = validate_cwd(&p("data"), &cwd_context(), &fs).expect_err("contains it");
    assert!(error.contains("agentdesktop harness directory"), "{error}");
    assert!(error.starts_with("workspace may not contain"), "{error}");
}

#[test]
fn cwd_may_be_a_sibling_named_like_the_agentdesktop_directory() {
    accepted(&p("home/dev/.agentdesktop-work/app"));
    accepted(&p("home/dev/agentdesktop/app"));
}

// ------------------------------------------------- pre-create list (D5, N2)

#[test]
fn the_pre_create_list_holds_the_harness_directories_of_each_profile() {
    let home = PathBuf::from(HOME);
    let listed = |program: &str| -> BTreeSet<String> {
        harness_pre_create(profile_of(program), &home)
            .iter()
            .map(|path| path.to_str().unwrap().replace('/', "\\"))
            .collect()
    };
    let opencode = listed("opencode");
    for sub in ["data", "config", "state", "cache", r"config\opencode"] {
        let dir = format!(r"{HARNESS}\opencode\{sub}");
        assert!(opencode.contains(&dir), "{dir}: {opencode:?}");
    }
    let copilot = listed("copilot");
    for dir in [
        format!(r"{HARNESS}\copilot"),
        format!(r"{HARNESS}\copilot-pkg"),
    ] {
        assert!(copilot.contains(&dir), "{dir}: {copilot:?}");
    }
    let claude = listed("claude-code");
    assert!(claude.contains(&format!(r"{HOME}\.claude")), "{claude:?}");
    assert!(
        !claude.iter().any(|dir| is_under_str(dir, HARNESS)),
        "Claude gets no harness home: {claude:?}"
    );
}

/// D5 (round-2 N2): MXC refuses a denied path under a missing parent
/// (0x80070003). On a host with nothing but the fixture's base directories
/// and the pre-create list, every denied path's parent exists.
#[test]
fn the_plan_never_denies_a_path_whose_parent_is_absent() {
    for program in PROGRAMS_6A {
        let mut world: BTreeSet<String> = [
            HOME,
            CWD,
            LAUNCH_DIR,
            r"C:\Users\dev\.local\bin",
            r"C:\Program Files\Git",
            r"C:\lab\agentdesktop",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        world.insert(format!(r"{LAUNCH_DIR}\tmp"));
        world.extend(
            harness_pre_create(profile_of(program), &PathBuf::from(HOME))
                .iter()
                .map(|path| path.to_str().unwrap().replace('/', "\\")),
        );
        // create_dir_all: every ancestor exists too.
        let mut with_ancestors = world.clone();
        for path in &world {
            let mut current = path.as_str();
            while let Some((parent, _)) = current.rsplit_once('\\') {
                with_ancestors.insert(parent.to_owned());
                current = parent;
            }
        }
        let plan = plan_6a(program, &with_ancestors, &with_ancestors, |_| {});
        for denied in strings(&plan.denied) {
            let (parent, _) = denied.rsplit_once('\\').expect("a parent");
            assert!(
                with_ancestors.contains(parent),
                "{program}: {denied} is denied but its parent {parent} is absent"
            );
        }
    }
}

// ------------------------------------------------- the plan per profile (AC1, AC6, D9, D10)

#[test]
fn every_profile_carries_the_union_workspace_pins() {
    for program in PROGRAMS_6A {
        let plan = default_plan_6a(program);
        let protected = protected_6a(&plan);
        for pin in union_pins() {
            if is_under_str(&pin, &format!(r"{CWD}\.github")) {
                continue; // the fixture has no `.github`: see the next test
            }
            assert!(protected.contains(&pin), "{program}: {pin} (D9)");
            assert!(!writable_6a(&plan, &pin) || protected.contains(&pin));
        }
        // Present entries are read-only, absent ones denied, except the three
        // OpenCode workspace entries in an OpenCode box: they are read-only
        // whether or not they exist (a denied absent path ends OpenCode's
        // config scan), and an OpenCode box never carries them as denials.
        let readonly: BTreeSet<String> = strings(&plan.readonly).into_iter().collect();
        assert!(readonly.contains(&format!(r"{CWD}\.mcp.json")), "{program}");
        let denied: BTreeSet<String> = strings(&plan.denied).into_iter().collect();
        for absent in ["opencode.json", "opencode.jsonc", ".opencode"] {
            let path = format!(r"{CWD}\{absent}");
            if program == "opencode" {
                assert!(readonly.contains(&path), "{program}: {absent} read-only");
                assert!(!denied.contains(&path), "{program}: {absent} not denied");
            } else {
                assert!(denied.contains(&path), "{program}: {absent}");
                assert!(!readonly.contains(&path), "{program}: {absent}");
            }
        }
        assert!(denied.contains(&format!(r"{CWD}\HEAD")), "{program}: HEAD");
    }
}

/// DV-11 (lab B-1): an absent workspace union entry under an existing parent
/// can be granted read-only; MXC answers the box's read/info calls with
/// ENOENT, not EPERM, which is what keeps OpenCode's config scan alive in a
/// workspace that has none of the three. The entry still cannot be written.
/// Every other program keeps read-only-when-present/denied-when-absent, and
/// an OpenCode box keeps that rule for the rest of the workspace pins.
#[test]
fn opencode_workspace_union_entries_are_read_only_whether_or_not_they_exist() {
    let trio = ["opencode.json", "opencode.jsonc", ".opencode"];
    for program in PROGRAMS_6A {
        // Absent.
        let plan = default_plan_6a(program);
        let readonly = strings(&plan.readonly);
        let denied = strings(&plan.denied);
        for entry in trio {
            let path = format!(r"{CWD}\{entry}");
            if program == "opencode" {
                assert!(readonly.contains(&path), "{program}: {path} (DV-11)");
                assert!(!denied.contains(&path), "{program}: {path} (DV-11)");
            } else {
                assert!(denied.contains(&path), "{program}: {path}");
                assert!(!readonly.contains(&path), "{program}: {path}");
            }
        }
        if program != "opencode" {
            continue;
        }
        // The rest of the workspace pins keep their rule.
        assert!(readonly.contains(&format!(r"{CWD}\.mcp.json")), "DV-11");
        assert!(denied.contains(&format!(r"{CWD}\HEAD")), "DV-11");

        // Present: the same read-only entries, still never denials.
        let mut existing = existing_6a(program);
        for entry in trio {
            existing.insert(format!(r"{CWD}\{entry}"));
        }
        let plan = plan_6a(program, &existing, &directories_6a(program), |input| {
            input.args.clear()
        });
        let readonly = strings(&plan.readonly);
        let denied = strings(&plan.denied);
        for entry in trio {
            let path = format!(r"{CWD}\{entry}");
            assert!(readonly.contains(&path), "{program}: {path} present");
            assert!(!denied.contains(&path), "{program}: {path} present");
        }
    }
}

#[test]
fn every_profile_denies_a_missing_github_folder_whole_and_pins_its_entries_when_present() {
    let github = format!(r"{CWD}\.github");
    for program in PROGRAMS_6A {
        let plan = default_plan_6a(program);
        let denied = strings(&plan.denied);
        assert!(denied.contains(&github), "{program}: {denied:?}");
        assert!(
            !protected_6a(&plan)
                .iter()
                .any(|path| path.starts_with(&format!(r"{github}\"))),
            "{program}: no entry under a missing .github"
        );

        let mut existing = existing_6a(program);
        let mut directories = directories_6a(program);
        existing.insert(github.clone());
        directories.insert(github.clone());
        existing.insert(format!(r"{github}\copilot"));
        directories.insert(format!(r"{github}\copilot"));
        let plan = plan_6a(program, &existing, &directories, |_| {});
        let readonly = strings(&plan.readonly);
        let denied = strings(&plan.denied);
        assert!(
            readonly.contains(&format!(r"{github}\copilot")),
            "{program}"
        );
        for absent in ["mcp.json", "hooks", "agents"] {
            assert!(
                denied.contains(&format!(r"{github}\{absent}")),
                "{program}: {absent}"
            );
        }
        assert!(
            !denied.contains(&github),
            "{program}: .github itself stays usable"
        );
    }
}

#[test]
fn non_claude_boxes_carry_no_claude_env_home_grant_or_installation() {
    for program in ["opencode", "copilot"] {
        let plan = default_plan_6a(program);
        for key in plan.env.keys() {
            assert!(
                !key.starts_with("CLAUDE_") && !key.starts_with("ANTHROPIC_"),
                "{program}: {key} (AC6)"
            );
        }
        let claude_dir = format!(r"{HOME}\.claude");
        for granted in strings(&plan.readwrite)
            .iter()
            .chain(&strings(&plan.readonly))
        {
            assert!(
                !is_under_str(granted, &claude_dir),
                "{program}: {granted} (AC6)"
            );
        }
        for path in strings(&plan.readwrite)
            .iter()
            .chain(&strings(&plan.readonly))
            .chain(&strings(&plan.denied))
        {
            assert!(
                !is_under_str(path, r"C:\Program Files\ClaudeCode")
                    && !is_under_str(path, r"C:\Users\dev\.local\share\claude"),
                "{program}: {path} (AC6)"
            );
        }
    }
}

#[test]
fn the_claude_box_keeps_its_env_and_claude_directory() {
    let plan = default_plan_6a("claude-code");
    assert_eq!(plan.env["CLAUDE_CONFIG_DIR"], format!(r"{HOME}\.claude"));
    assert_eq!(
        plan.env["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:4000/sandbox"
    );
    assert_eq!(
        plan.env["ANTHROPIC_CUSTOM_HEADERS"],
        "x-agentdesktop-pairing: test-pairing-value"
    );
    assert_eq!(plan.env["ANTHROPIC_AUTH_TOKEN"], "placeholder");
    assert!(strings(&plan.readwrite).contains(&format!(r"{HOME}\.claude")));
    let readonly = strings(&plan.readonly);
    assert!(readonly.contains(&r"C:\Program Files\ClaudeCode".to_owned()));
    assert!(readonly.contains(&r"C:\Users\dev\.local\share\claude".to_owned()));
    for key in plan.env.keys() {
        assert!(
            !key.starts_with("OPENCODE_")
                && !key.starts_with("COPILOT_")
                && !key.starts_with("XDG_"),
            "{key}: another program's env in the Claude box"
        );
    }
    assert!(
        !strings(&plan.readwrite)
            .iter()
            .any(|path| is_under_str(path, HARNESS)),
        "no harness home grant in the Claude box"
    );
}

#[test]
fn the_opencode_box_env_points_xdg_into_its_harness_home_and_pins_network_features() {
    let plan = default_plan_6a("opencode");
    let home = format!(r"{HARNESS}\opencode");
    for (key, sub) in [
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        assert_eq!(plan.env[key], format!(r"{home}\{sub}"), "{key}");
    }
    for (key, value) in [
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
    ] {
        assert_eq!(
            plan.env.get(key).map(String::as_str),
            Some(value),
            "{key} (D8)"
        );
    }
    for key in plan.env.keys() {
        assert!(!key.starts_with("COPILOT_"), "{key}");
    }
    for sub in ["data", "state", "cache"] {
        let dir = format!(r"{home}\{sub}");
        assert!(writable_6a(&plan, &dir), "{dir} is read-write (D5)");
    }
    assert_eq!(plan.egress, DENY);
}

#[test]
fn the_copilot_box_env_carries_byok_provider_model_and_harness_dirs() {
    let plan = default_plan_6a("copilot");
    assert_eq!(plan.env["COPILOT_HOME"], format!(r"{HARNESS}\copilot"));
    assert_eq!(
        plan.env["COPILOT_PKG_CACHE_HOME"],
        format!(r"{HARNESS}\copilot-pkg")
    );
    assert_eq!(plan.env["COPILOT_PROVIDER_TYPE"], "anthropic");
    assert_eq!(
        plan.env["COPILOT_PROVIDER_BASE_URL"],
        "http://127.0.0.1:4000/sandbox"
    );
    assert_eq!(plan.env["COPILOT_PROVIDER_API_KEY"], "placeholder");
    let headers = &plan.env["COPILOT_PROVIDER_HEADERS"];
    assert!(
        headers.contains("x-agentdesktop-pairing") && headers.contains("test-pairing-value"),
        "{headers}"
    );
    assert_eq!(plan.env["COPILOT_MODEL"], COPILOT_WIRE_MODEL);
    assert_eq!(plan.env["COPILOT_AUTO_UPDATE"], "false");
    assert!(!plan.env.contains_key("COPILOT_OFFLINE"), "D8");
    for key in plan.env.keys() {
        assert!(
            !key.starts_with("OPENCODE_") && !key.starts_with("XDG_"),
            "{key}"
        );
    }
    assert!(writable_6a(&plan, &format!(r"{HARNESS}\copilot")));
}

/// DV-12 (lab B-2): denying `config.json` alone ends Copilot CLI silently,
/// so it lives under the read-write `COPILOT_HOME` grant. The untrusted
/// parts are the other config-dir entries, which stay read-only or denied,
/// and the package cache.
#[test]
fn the_copilot_box_leaves_config_json_writable_and_grants_the_pkg_cache_read_only() {
    let plan = default_plan_6a("copilot");
    let config_json = format!(r"{HARNESS}\copilot\config.json");
    assert!(
        !strings(&plan.denied).contains(&config_json),
        "DV-12: config.json is not denied"
    );
    assert!(
        !strings(&plan.readonly).contains(&config_json),
        "DV-12: config.json is not a read-only pin"
    );
    assert!(
        writable_6a(&plan, &config_json),
        "DV-12: config.json sits under the read-write COPILOT_HOME grant"
    );
    let cache = format!(r"{HARNESS}\copilot-pkg");
    assert!(strings(&plan.readonly).contains(&cache), "D4");
    assert!(
        !writable_6a(&plan, &cache),
        "the cache is never writable (D4)"
    );
    for path in strings(&plan.readwrite)
        .iter()
        .chain(&strings(&plan.readonly))
    {
        assert!(
            !is_under_str(path, r"C:\Users\dev\AppData\Local\copilot"),
            "{path}: %LOCALAPPDATA%\\copilot stays invisible (D4)"
        );
    }
}

#[test]
fn harness_home_pins_are_read_only_when_present_and_denied_when_absent() {
    for program in ["opencode", "copilot"] {
        // DV-10: the pre-created pins are read-only whether or not they
        // exist; the rule below holds for every other pin.
        let pre_created = pre_created_pins(program);
        let (always_readonly, pins): (Vec<String>, Vec<String>) = home_pins(program)
            .into_iter()
            .partition(|pin| pre_created.contains(pin));
        let present = pins[0].clone();
        let mut existing = existing_6a(program);
        existing.insert(present.clone());
        let plan = plan_6a(program, &existing, &directories_6a(program), |_| {});
        let readonly = strings(&plan.readonly);
        let denied = strings(&plan.denied);
        assert!(readonly.contains(&present), "{program}: {present}");
        for pin in &pins[1..] {
            assert!(denied.contains(pin), "{program}: {pin} (D5)");
        }
        for pin in &always_readonly {
            assert!(readonly.contains(pin), "{program}: {pin} (DV-10)");
            assert!(!denied.contains(pin), "{program}: {pin} (DV-10)");
        }
    }
}

/// Fix round 2 (DV-10, lab F-oc-1): MXC answers a denied path with EPERM,
/// and OpenCode's bootstrap fails on EPERM reading its global `config.json`.
/// In a fresh harness home the config trio is granted read-only (the daemon
/// pre-creates it), never denied; it stays a pin and an integrity root. The
/// legacy TOML `config` stays denied when absent.
#[test]
fn the_opencode_global_config_trio_is_read_only_not_denied_in_a_fresh_home() {
    let plan = default_plan_6a("opencode");
    let readonly = strings(&plan.readonly);
    let denied = strings(&plan.denied);
    let roots = recursive_roots("opencode");
    let pins = home_pins("opencode");
    for pin in pre_created_pins("opencode") {
        assert!(readonly.contains(&pin), "{pin} (DV-10)");
        assert!(!denied.contains(&pin), "{pin} (DV-10)");
        assert!(pins.contains(&pin), "{pin} stays a D5 pin");
        assert!(roots.contains(&pin), "{pin} stays a D12 root");
    }
    let legacy = format!(r"{HARNESS}\opencode\config\opencode\config");
    assert!(denied.contains(&legacy), "{legacy} (DV-1)");
    assert!(!readonly.contains(&legacy), "{legacy} (DV-1)");
}

/// DV-10/DV-12: the pre-created harness pins are per program: OpenCode's
/// config trio and Copilot CLI's `providers.json`, each in a folder on the
/// pre-create list; Claude Code gets none.
#[test]
fn the_pre_created_files_are_per_program() {
    let home = PathBuf::from(HOME);
    for program in PROGRAMS_6A {
        let files: Vec<String> = harness_pre_create_files(profile_of(program), &home)
            .iter()
            .map(|path| path.to_str().unwrap().replace('/', "\\"))
            .collect();
        assert_eq!(files, pre_created_pins(program), "{program}");
        let directories: Vec<String> = harness_pre_create(profile_of(program), &home)
            .iter()
            .map(|path| path.to_str().unwrap().replace('/', "\\"))
            .collect();
        for file in &files {
            let (parent, _) = file.rsplit_once('\\').unwrap();
            assert!(directories.iter().any(|dir| dir == parent), "{file}");
        }
    }
    assert_eq!(PRE_CREATED_CONTENTS, b"{}\n");
    let parsed: serde_json::Value =
        serde_json::from_slice(PRE_CREATED_CONTENTS).expect("valid JSON");
    assert_eq!(parsed, serde_json::json!({}));
}

/// Fix round 2 (DV-10): preparing the OpenCode home writes `{}` into each
/// absent trio file and leaves an existing one byte for byte as it was; the
/// legacy `config` is not created.
#[test]
fn preparing_the_opencode_home_pre_creates_absent_config_files_without_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let profile = profile_of("opencode");
    let files = harness_pre_create_files(profile, home);
    assert_eq!(files.len(), 3);
    std::fs::create_dir_all(files[1].parent().unwrap()).unwrap();
    std::fs::write(&files[1], b"{\"theme\":\"user\"}").unwrap();
    prepare_program_home(profile, home).unwrap();
    assert_eq!(std::fs::read(&files[0]).unwrap(), PRE_CREATED_CONTENTS);
    assert_eq!(std::fs::read(&files[1]).unwrap(), b"{\"theme\":\"user\"}");
    assert_eq!(std::fs::read(&files[2]).unwrap(), PRE_CREATED_CONTENTS);
    let legacy = super::harness_path(home, r"opencode\config\opencode\config");
    assert!(!legacy.exists(), "the legacy TOML file is never created");
    // A second launch leaves the pre-created files as they are.
    std::fs::write(&files[0], b"{\"edited\":true}").unwrap();
    prepare_program_home(profile, home).unwrap();
    assert_eq!(std::fs::read(&files[0]).unwrap(), b"{\"edited\":true}");
    for program in ["claude-code", "copilot"] {
        let temp = tempfile::tempdir().unwrap();
        prepare_program_home(profile_of(program), temp.path()).unwrap();
        assert!(
            !super::harness_path(temp.path(), r"opencode").exists(),
            "{program}"
        );
    }
}

/// DV-12 (lab B-2): Copilot CLI reads its BYOK provider registry at startup
/// and refuses to start when the file is missing or denied. Preparing the
/// home writes `{}` into an absent `providers.json` and leaves an existing
/// one byte for byte as it was; the file stays a pin and an integrity root.
#[test]
fn preparing_the_copilot_home_pre_creates_providers_json_without_overwriting() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let profile = profile_of("copilot");
    let files = harness_pre_create_files(profile, home);
    assert_eq!(files.len(), 1, "Copilot pre-creates exactly providers.json");
    let providers = files[0].clone();
    prepare_program_home(profile, home).unwrap();
    assert_eq!(std::fs::read(&providers).unwrap(), PRE_CREATED_CONTENTS);

    let existing = b"{\"github\":{\"enterprise\":\"example\"}}";
    std::fs::write(&providers, existing).unwrap();
    prepare_program_home(profile, home).unwrap();
    assert_eq!(std::fs::read(&providers).unwrap(), existing);

    let pin = format!(r"{HARNESS}\copilot\providers.json");
    assert!(home_pins("copilot").contains(&pin), "{pin} (D5)");
    assert!(recursive_roots("copilot").contains(&pin), "{pin} (D12)");
    let plan = default_plan_6a("copilot");
    assert!(strings(&plan.readonly).contains(&pin), "{pin} (DV-12)");
    assert!(!strings(&plan.denied).contains(&pin), "{pin} (DV-12)");
}

/// DV-13 (lab O10): a box can rename the read-write cache root and recreate
/// `cache\opencode\bin` with a planted executable. Preparing the home
/// empties the bin before the plan and the integrity snapshot, so nothing a
/// previous box put there is granted read-only and run.
#[test]
fn preparing_the_opencode_home_empties_a_non_empty_tool_bin() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let bin = super::harness_path(home, r"opencode\cache\opencode\bin");
    std::fs::create_dir_all(bin.join("vscode-eslint")).unwrap();
    std::fs::write(bin.join("rg.exe"), b"planted").unwrap();
    std::fs::write(bin.join("vscode-eslint").join("server.js"), b"planted").unwrap();
    let marker = super::harness_path(home, r"opencode\cache\marker.txt");
    std::fs::write(&marker, b"kept").unwrap();

    prepare_program_home(profile_of("opencode"), home).unwrap();

    assert!(bin.is_dir(), "the bin is recreated as a directory");
    assert_eq!(
        std::fs::read_dir(&bin).unwrap().count(),
        0,
        "every child is removed"
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"kept");
    assert!(!bin.join("vscode-eslint").exists());
}

#[test]
fn preparing_the_opencode_home_leaves_an_empty_tool_bin_alone() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let bin = super::harness_path(home, r"opencode\cache\opencode\bin");
    prepare_program_home(profile_of("opencode"), home).unwrap();
    assert!(bin.is_dir(), "the pre-create pass makes the bin");
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
    // A launch over a prepared home is a no-op.
    prepare_program_home(profile_of("opencode"), home).unwrap();
    assert!(bin.is_dir());
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
}

#[test]
fn preparing_the_opencode_home_replaces_a_file_at_the_tool_bin() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let bin = super::harness_path(home, r"opencode\cache\opencode\bin");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"not a directory").unwrap();

    prepare_program_home(profile_of("opencode"), home).unwrap();

    assert!(bin.is_dir(), "the file is replaced by the directory");
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
}

/// A reparse point at the bin itself fails the launch, naming the path: the
/// cleanup must not be redirected outside the harness home.
#[cfg(unix)]
#[test]
fn preparing_the_opencode_home_refuses_a_reparse_point_at_the_tool_bin() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let target = temp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep.txt"), b"keep").unwrap();
    let bin = super::harness_path(home, r"opencode\cache\opencode\bin");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &bin).unwrap();

    let error = prepare_program_home(profile_of("opencode"), home).unwrap_err();

    let message = format!("{error:#}");
    assert!(
        message.contains(bin.to_str().unwrap()),
        "the error names the path: {message}"
    );
    assert!(
        std::fs::symlink_metadata(&bin)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself is left in place"
    );
    assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"keep");
}

/// A reparse point above the bin (the box renamed the read-write cache root
/// and planted a link) would make the leaf lookup resolve to the target's
/// real bin. Preparing must fail at the link, naming it, and leave the
/// target untouched.
#[cfg(unix)]
#[test]
fn preparing_the_opencode_home_refuses_a_reparse_point_above_the_tool_bin() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let target = temp.path().join("target");
    std::fs::create_dir_all(target.join("bin")).unwrap();
    std::fs::write(target.join("bin").join("sentinel.txt"), b"sentinel").unwrap();
    let parent = super::harness_path(home, r"opencode\cache\opencode");
    std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &parent).unwrap();

    let error = prepare_program_home(profile_of("opencode"), home).unwrap_err();

    let message = format!("{error:#}");
    assert!(
        message.contains(parent.to_str().unwrap()),
        "the error names the link: {message}"
    );
    assert!(
        std::fs::symlink_metadata(&parent)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself is left in place"
    );
    assert_eq!(
        std::fs::read(target.join("bin").join("sentinel.txt")).unwrap(),
        b"sentinel",
        "the target's bin is untouched"
    );
}

/// A link inside the bin is removed as the link it is; the target keeps its
/// contents.
#[cfg(unix)]
#[test]
fn preparing_the_opencode_home_removes_a_symlink_inside_the_tool_bin_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let target = temp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("keep.txt"), b"keep").unwrap();
    let bin = super::harness_path(home, r"opencode\cache\opencode\bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&target, bin.join("planted-dir")).unwrap();
    std::fs::write(bin.join("planted-file"), b"planted").unwrap();

    prepare_program_home(profile_of("opencode"), home).unwrap();

    assert!(
        std::fs::symlink_metadata(bin.join("planted-dir")).is_err(),
        "the link is gone, not followed"
    );
    assert!(!bin.join("planted-file").exists());
    assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
}

/// Fix round 1 (DV-6): OpenCode runs ripgrep and LSP servers from
/// `<XDG_CACHE_HOME>\opencode\bin`. The folder is pre-created and read-only
/// inside the read-write cache, so a box cannot plant an executable that a
/// later box runs; the rest of the cache stays writable.
#[test]
fn the_opencode_tool_bin_is_pre_created_and_read_only_inside_the_cache() {
    let bin = format!(r"{HARNESS}\opencode\cache\opencode\bin");
    let pre_created: Vec<String> = harness_pre_create(profile_of("opencode"), &PathBuf::from(HOME))
        .iter()
        .map(|path| path.to_str().unwrap().replace('/', "\\"))
        .collect();
    assert!(pre_created.contains(&bin), "{pre_created:?}");
    let plan = default_plan_6a("opencode");
    let readonly = strings(&plan.readonly);
    assert!(readonly.contains(&bin), "{readonly:?}");
    assert!(!strings(&plan.readwrite).contains(&bin));
    for planted in ["rg.exe", "gopls.exe", r"vscode-eslint\server.js"] {
        let path = format!(r"{bin}\{planted}");
        assert!(
            readonly.iter().any(|entry| is_under_str(&path, entry)),
            "{path} is not under a read-only grant"
        );
    }
    assert!(writable_6a(
        &plan,
        &format!(r"{HARNESS}\opencode\cache\opencode\models.json")
    ));
    for program in ["claude-code", "copilot"] {
        assert!(
            !strings(&default_plan_6a(program).readonly).contains(&bin),
            "{program}"
        );
    }
}

/// Fix round 1 (DV-7): the read-write grant is `config\opencode` itself, so
/// the folder holding the pins is a grant root the box cannot rename or
/// replace; `XDG_CONFIG_HOME` still names its parent.
#[test]
fn the_opencode_config_grant_is_the_pinned_folder_not_its_parent() {
    let config = format!(r"{HARNESS}\opencode\config");
    let pinned = format!(r"{config}\opencode");
    let plan = default_plan_6a("opencode");
    assert_eq!(plan.env["XDG_CONFIG_HOME"], config);
    let readwrite = strings(&plan.readwrite);
    assert!(readwrite.contains(&pinned), "{readwrite:?}");
    assert!(
        !readwrite.iter().any(|root| is_under_str(&config, root)),
        "nothing grants {config} or an ancestor read-write: {readwrite:?}"
    );
    let protected = protected_6a(&plan);
    for pin in home_pins("opencode") {
        assert!(protected.contains(&pin), "{pin}");
        let (parent, _) = pin.rsplit_once('\\').unwrap();
        assert_eq!(parent, pinned, "{pin} sits directly in the grant root");
    }
    let pre_created: Vec<String> = harness_pre_create(profile_of("opencode"), &PathBuf::from(HOME))
        .iter()
        .map(|path| path.to_str().unwrap().replace('/', "\\"))
        .collect();
    assert!(pre_created.contains(&pinned), "{pre_created:?}");
}

#[test]
fn no_managed_opencode_config_is_granted_to_the_box() {
    let plan = default_plan_6a("opencode");
    for path in strings(&plan.readwrite)
        .iter()
        .chain(&strings(&plan.readonly))
    {
        assert!(
            !is_under_str(path, r"C:\ProgramData\opencode"),
            "{path}: the managed layer must not leak in (D2a)"
        );
    }
}

#[test]
fn the_shared_git_surface_is_the_same_for_every_profile() {
    let git_config = format!(r"{LAUNCH_DIR}\git_config");
    for program in PROGRAMS_6A {
        let plan = default_plan_6a(program);
        assert_eq!(plan.env["HOME"], HOME, "{program}");
        assert_eq!(plan.env["GIT_CONFIG_GLOBAL"], git_config, "{program}");
        assert_eq!(plan.env["GIT_TERMINAL_PROMPT"], "0", "{program}");
        assert!(strings(&plan.readonly).contains(&git_config), "{program}");
        assert!(
            strings(&plan.denied).contains(&format!(r"{HOME}\.gitconfig")),
            "{program}"
        );
        for secret in [".ssh", ".aws", ".azure", ".kube"] {
            assert!(
                strings(&plan.denied).contains(&format!(r"{HOME}\{secret}")),
                "{program}: {secret}"
            );
        }
    }
}

/// What turning ssh on adds to a plan: env entries (PATH as entries) and
/// grants.
fn ssh_delta(
    program: &str,
) -> (
    BTreeMap<String, String>,
    BTreeSet<String>,
    [BTreeSet<String>; 3],
) {
    let without = default_plan_6a(program);
    let with = plan_6a(
        program,
        &existing_6a(program),
        &directories_6a(program),
        |input| input.ssh_enabled = true,
    );
    let env: BTreeMap<String, String> = with
        .env
        .iter()
        .filter(|(key, value)| *key != "PATH" && without.env.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let path_before: BTreeSet<&str> = without.env["PATH"].split(';').collect();
    let path_added: BTreeSet<String> = with.env["PATH"]
        .split(';')
        .filter(|entry| !path_before.contains(entry))
        .map(str::to_owned)
        .collect();
    let added = |after: &[PathBuf], before: &[PathBuf]| -> BTreeSet<String> {
        let before: BTreeSet<String> = strings(before).into_iter().collect();
        strings(after)
            .into_iter()
            .filter(|path| !before.contains(path))
            .collect()
    };
    (
        env,
        path_added,
        [
            added(&with.readwrite, &without.readwrite),
            added(&with.readonly, &without.readonly),
            added(&with.denied, &without.denied),
        ],
    )
}

/// Contract (round-2 N3): the ssh surface is shared; its delta does not
/// depend on the profile.
#[test]
fn the_ssh_delta_is_profile_independent() {
    let claude = ssh_delta("claude-code");
    assert!(claude.0.contains_key("SSH_AUTH_SOCK"), "{:?}", claude.0);
    assert!(claude.0.contains_key("GIT_SSH"), "{:?}", claude.0);
    for program in ["opencode", "copilot"] {
        assert_eq!(ssh_delta(program), claude, "{program}");
    }
}

#[test]
fn each_profile_names_its_window_title_in_the_plan() {
    for (program, title) in [
        ("claude-code", "agentdesktop: claude"),
        ("opencode", "agentdesktop: opencode"),
        ("copilot", "agentdesktop: copilot"),
    ] {
        assert_eq!(default_plan_6a(program).window_title, title);
    }
}

fn request_6a(program: &str) -> serde_json::Value {
    serde_json::to_value(engine::mxc::MxcEngine.to_request(&default_plan_6a(program), LAUNCH_ID))
        .expect("the request serializes")
}

/// AC1: the checked-in request (`testdata/request-opencode.json`), generated
/// from this plan and reviewed by hand.
#[test]
fn the_opencode_request_matches_the_checked_in_snapshot() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("testdata/request-opencode.json")).unwrap();
    assert_eq!(sorted_paths(request_6a("opencode")), sorted_paths(expected));
}

/// AC1: as above, for Copilot.
#[test]
fn the_copilot_request_matches_the_checked_in_snapshot() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("testdata/request-copilot.json")).unwrap();
    assert_eq!(sorted_paths(request_6a("copilot")), sorted_paths(expected));
}

// ------------------------------------------------- integrity roots (AC9, D12)

fn recursive_roots(program: &str) -> BTreeSet<String> {
    super::integrity::integrity_paths_for(
        profile_of(program),
        &PathBuf::from(HOME),
        &PathBuf::from(CWD),
    )
    .iter()
    .filter(|root| root.mode == super::integrity::RootMode::Recursive)
    .map(|root| root.path.to_str().unwrap().replace('/', "\\"))
    .collect()
}

#[test]
fn integrity_roots_per_profile_are_the_union_and_the_harness_home_pins() {
    for program in PROGRAMS_6A {
        let roots = recursive_roots(program);
        for pin in union_pins().into_iter().chain(home_pins(program)) {
            assert!(
                roots.contains(&pin),
                "{program}: {pin} is not an integrity root"
            );
        }
        for other in PROGRAMS_6A.iter().filter(|other| **other != program) {
            for pin in home_pins(other) {
                assert!(!roots.contains(&pin), "{program}: {pin} belongs to {other}");
            }
        }
    }
}

#[test]
fn the_pkg_cache_and_harness_read_write_roots_are_not_integrity_roots() {
    for program in ["opencode", "copilot"] {
        let roots = recursive_roots(program);
        for not_a_root in [
            format!(r"{HARNESS}\copilot-pkg"),
            format!(r"{HARNESS}\copilot"),
            format!(r"{HARNESS}\opencode"),
            format!(r"{HARNESS}\opencode\data"),
            format!(r"{HARNESS}\opencode\state"),
            format!(r"{HARNESS}\opencode\cache"),
        ] {
            assert!(
                !roots
                    .iter()
                    .any(|root| is_under_str(root, &not_a_root)
                        && !home_pins(program).contains(root)),
                "{program}: {not_a_root} (D12)"
            );
        }
    }
}

/// Contract (round-2 N4): the reverse classification, fed per profile.
/// Every recursive root is protected, and every protected entry is a root
/// or on the profile's named list.
#[test]
fn every_profiles_integrity_roots_are_protected_and_every_protected_entry_is_classified() {
    let shared: Vec<String> = [
        r"C:\Users\dev\.local\bin".to_owned(),
        r"C:\Program Files\Git".into(),
        r"C:\lab\agentdesktop".into(),
        r"C:\Users\dev\.gitconfig".into(),
        format!(r"{LAUNCH_DIR}\credential"),
        format!(r"{LAUNCH_DIR}\git_config"),
        r"C:\Users\dev\.claude\.credentials.json".into(),
        r"C:\Users\dev\.claude\history.jsonl".into(),
        r"C:\Users\dev\.claude\ide".into(),
        r"C:\Users\dev\.ssh".into(),
        r"C:\Users\dev\.aws".into(),
        r"C:\Users\dev\.azure".into(),
        r"C:\Users\dev\.kube".into(),
        // `.github` is denied whole when absent (D9 parent rule).
        format!(r"{CWD}\.github"),
        // An absent workspace `.claude` is denied whole (PR 1 rule).
        format!(r"{CWD}\.claude"),
    ]
    .into_iter()
    .collect();
    for program in PROGRAMS_6A {
        let mut named = shared.clone();
        match program {
            "claude-code" => named.extend([
                r"C:\Users\dev\.local\share\claude".to_owned(),
                r"C:\Program Files\ClaudeCode".to_owned(),
            ]),
            "opencode" => {
                for sub in [
                    "",
                    r"\data",
                    r"\config",
                    r"\state",
                    r"\cache",
                    r"\config\opencode",
                    // Read-only tool folder inside the cache, like Copilot's
                    // package cache not a root.
                    r"\cache\opencode\bin",
                ] {
                    named.push(format!(r"{HARNESS}\opencode{sub}"));
                }
            }
            _ => named.extend([
                format!(r"{HARNESS}\copilot"),
                format!(r"{HARNESS}\copilot-pkg"),
            ]),
        }
        let plan = default_plan_6a(program);
        let protected = protected_6a(&plan);
        let roots = recursive_roots(program);
        for root in &roots {
            if is_under_str(root, &format!(r"{CWD}\.github"))
                && protected.contains(&format!(r"{CWD}\.github"))
            {
                continue; // covered by the whole-folder denial
            }
            assert!(
                protected.contains(root),
                "{program}: root {root} is not protected"
            );
        }
        for entry in &protected {
            assert!(
                roots.contains(entry) || named.contains(entry),
                "{program}: unclassified protected entry {entry}: add it to integrity_paths_for or name it here"
            );
        }
    }
}

// ------------------------------------------------- the launch handler (AC3-AC5, AC8, D11, G1, G2)

const SIX_A_CONFIG: &str = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode: {}
  openCode:
    model: claude-sonnet
    models:
      claude-sonnet: {}
  copilot:
    models:
      sonnet:
        provider: agentdesktop-anthropic
"#;

impl Harness {
    /// Places the program's native exe in `<home>\.local\bin` and, for
    /// Copilot, a primed package cache (one version directory, D4 fallback:
    /// the stub exe carries no PE version).
    fn install(&self, program: &str) {
        let bin = self.home.join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(exe_of(program)), b"MZ").unwrap();
        if program == "copilot" {
            prime_pkg_cache(
                &self
                    .home
                    .join(".agentdesktop")
                    .join("harness")
                    .join("copilot-pkg"),
                "1.0.101",
                true,
            );
        }
    }

    fn body_for(&self, program: &str, args: &[&str]) -> serde_json::Value {
        self.body_with(|body| {
            body["program"] = program.into();
            body["args"] = serde_json::json!(args);
        })
    }
}

#[tokio::test(start_paused = true)]
async fn opencode_and_copilot_launch_201_with_the_program_id_and_no_claude_posture() {
    for program in ["opencode", "copilot"] {
        let h = Harness::start(Setup {
            config: SIX_A_CONFIG,
            managed_file: Some(
                r#"{"allowManagedHooksOnly":true,"allowManagedMcpServersOnly":true}"#,
            ),
            ..Setup::default()
        })
        .await;
        h.install(program);
        let (status, body) = h.launch(&h.body_for(program, &[])).await;
        assert_eq!(status, 201, "{program}: {body}");
        let launched: Launch = serde_json::from_str(&body).unwrap();
        assert_eq!(launched.program, program, "AC8");
        assert_eq!(launched.managed_settings_source, None, "{program} (G2)");
        assert!(!launched.managed_hooks_locked, "{program} (G2)");
        assert!(!launched.managed_mcp_locked, "{program} (G2)");
        let listed = h.listed().await;
        assert_eq!(listed[0].program, program);
        assert_eq!(listed[0].managed_settings_source, None);
        assert_eq!(
            h.credentials.calls(),
            vec![(program.to_owned(), true)],
            "client id = program id (D11)"
        );
        let spawns = h.executor.spawns();
        let request: serde_json::Value =
            serde_json::from_str(spawns[0].request_json.as_deref().unwrap()).unwrap();
        let env = env_map(&request);
        assert!(
            !env.keys().any(|key| key.starts_with("ANTHROPIC_")),
            "{program}: {env:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn opencode_and_copilot_under_egress_allow_answer_409_and_start_nothing() {
    for program in ["opencode", "copilot"] {
        let h = Harness::start(Setup {
            config: SIX_A_CONFIG,
            egress: ALLOW,
            ..Setup::default()
        })
        .await;
        h.install(program);
        assert_eq!(
            h.launch(&h.body_for(program, &[])).await,
            (409, EGRESS_ALLOW_REFUSAL.to_owned()),
            "{program} (D1b)"
        );
        assert!(h.executor.spawns().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn claude_code_under_egress_allow_still_launches() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        egress: ALLOW,
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launch(&h.body()).await.0, 201);
}

#[tokio::test(start_paused = true)]
async fn a_sandbox_argument_to_copilot_answers_400() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        ..Setup::default()
    })
    .await;
    h.install("copilot");
    for list in [vec!["--sandbox"], vec!["--sandbox=true"]] {
        assert_eq!(
            h.launch(&h.body_for("copilot", &list)).await,
            (400, SANDBOX_REFUSAL.to_owned()),
            "{list:?}"
        );
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn serve_and_listener_flags_to_opencode_answer_400() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        ..Setup::default()
    })
    .await;
    h.install("opencode");
    for (list, token) in [
        (vec!["serve"], "serve"),
        (vec!["web"], "web"),
        (vec!["--print-logs", "serve"], "serve"),
        (vec!["--port", "4096"], "--port"),
        (vec!["--hostname", "0.0.0.0"], "--hostname"),
        (vec!["--mdns"], "--mdns"),
    ] {
        assert_eq!(
            h.launch(&h.body_for("opencode", &list)).await,
            (400, listener_refusal(token)),
            "{list:?}"
        );
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn an_opencode_client_id_outside_allowed_client_ids_answers_409() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        session_policy: Arc::new(crate::llm_proxy::sessions::ConfigPolicy {
            config: serde_yaml::from_str(
                "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [claude-code, copilot-cli]\n",
            )
            .unwrap(),
            state_dir: std::env::temp_dir(),
        }),
        ..Setup::default()
    })
    .await;
    for program in ["opencode", "copilot"] {
        h.install(program);
        assert_eq!(
            h.launch(&h.body_for(program, &[])).await,
            (
                409,
                format!("client id {program} is not in llmGateway.authentication.allowedClientIds")
            ),
            "D11: the boxed id is the program id, not copilot-cli"
        );
    }
    h.assert_untouched();
}

#[tokio::test(start_paused = true)]
async fn a_copilot_launch_without_a_primed_pkg_cache_answers_409_and_spawns_nothing() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        ..Setup::default()
    })
    .await;
    let bin = h.home.join(".local").join("bin");
    std::fs::write(bin.join("copilot.exe"), b"MZ").unwrap();
    let (status, body) = h.launch(&h.body_for("copilot", &[])).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.starts_with("Copilot package cache not found for "),
        "{body}"
    );
    assert!(
        body.contains("run copilot --version once outside the box with COPILOT_PKG_CACHE_HOME=")
            && body.contains("copilot-pkg"),
        "{body}"
    );
    assert!(
        h.executor.spawns().is_empty(),
        "never spawned unboxed or boxed (AC5)"
    );
}

const SIX_A_WIRE_MODEL_CONFIG: &str = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  copilot:
    models:
      sonnet:
        provider: agentdesktop-anthropic
        wireModel: 5
"#;

#[tokio::test(start_paused = true)]
async fn a_copilot_non_string_wire_model_answers_409() {
    let h = Harness::start(Setup {
        config: SIX_A_WIRE_MODEL_CONFIG,
        credentials_must_not_be_reached: true,
        ..Setup::default()
    })
    .await;
    h.install("copilot");
    assert_eq!(
        h.launch(&h.body_for("copilot", &[])).await,
        (
            409,
            "programs.copilot.models entry sonnet: wireModel must be a string".to_owned()
        )
    );
    h.assert_untouched();
}

/// D13 G1: `daemon.claudeCode.config` refuses claude-code launches only.
#[tokio::test(start_paused = true)]
async fn a_claude_settings_override_refuses_only_claude_code_launches() {
    let h = Harness::start(Setup {
        config: SIX_A_CONFIG,
        claude_settings_overridden: true,
        ..Setup::default()
    })
    .await;
    let (status, body) = h.launch(&h.body()).await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("daemon.claudeCode.config"), "{body}");
    h.assert_untouched();
    h.install("opencode");
    assert_eq!(h.launch(&h.body_for("opencode", &[])).await.0, 201);
}
