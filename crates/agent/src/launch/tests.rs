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

use super::*;

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
    }
}

#[test]
fn containment_info_is_absent_when_unconfigured() {
    assert_eq!(containment_info(None, true, true, false, &|_| true), None);
}

#[test]
fn containment_info_is_available_for_a_user_mode_windows_daemon_with_the_executor() {
    let cfg = executor_cfg(r"C:\mxc\x64\wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, true, false, &|_| true).unwrap();
    assert_eq!(
        info,
        ContainmentInfo {
            executor: r"C:\mxc\x64\wxc-exec.exe".into(),
            available: true,
            error: None,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            managed_settings_error: None,
        }
    );
}

#[test]
fn containment_info_needs_user_mode() {
    let cfg = executor_cfg(r"C:\mxc\wxc-exec.exe");
    let info = containment_info(Some(&cfg), false, true, false, &|_| true).unwrap();
    assert!(!info.available);
    assert_eq!(info.error.as_deref(), Some("requires user mode"));
    assert_eq!(info.executor, r"C:\mxc\wxc-exec.exe");
}

#[test]
fn containment_info_is_windows_only() {
    let cfg = executor_cfg("/opt/mxc/wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, false, false, &|_| true).unwrap();
    assert!(!info.available);
    assert_eq!(info.error.as_deref(), Some("Windows only"));
}

#[test]
fn containment_info_names_a_missing_executor() {
    let cfg = executor_cfg(r"C:\missing\wxc-exec.exe");
    let seen = RefCell::new(Vec::new());
    let info = containment_info(Some(&cfg), true, true, false, &|path| {
        seen.borrow_mut().push(path.to_owned());
        false
    })
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

#[test]
fn containment_info_is_unavailable_when_the_claude_settings_path_is_overridden() {
    let cfg = executor_cfg(r"C:\mxc\wxc-exec.exe");
    let info = containment_info(Some(&cfg), true, true, true, &|_| true).unwrap();
    assert!(!info.available);
    let error = info.error.expect("a reason");
    assert!(
        error.starts_with("daemon.claudeCode.config must be unset for run"),
        "{error}"
    );
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
        gateway_url: "https://alpha-dev.aks.solo.sademo.de".into(),
        base_env: base_env(),
        exists,
        is_dir,
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
        r"C:\Users\dev\.config\git\config".into(),
        format!(r"{LAUNCH_DIR}\credential"),
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
            "ingress": {"default": "allow", "hostLoopback": "deny"},
        })
    );
    assert_eq!(request["process"]["inheritDefaultEnv"], false);
    assert!(request["process"].get("timeout").is_none());
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
    assert_eq!(
        env["ANTHROPIC_BASE_URL"],
        "https://alpha-dev.aks.solo.sademo.de"
    );
    assert_eq!(
        env["AGENTDESKTOP_CREDENTIAL_FILE"],
        format!(r"{LAUNCH_DIR}\credential\token")
    );
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
    for key in [
        "PATH",
        "TEMP",
        "TMP",
        "CLAUDE_CONFIG_DIR",
        "AGENTDESKTOP_CREDENTIAL_FILE",
    ] {
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
        input.gateway_url = "https://gateway.example.com/anthropic".into();
    });
    assert_eq!(
        env_map(&request)["ANTHROPIC_BASE_URL"],
        "https://gateway.example.com/anthropic"
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
        &format!(r"{LAUNCH_DIR}\credential"),
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
    for path in readwrite(&request)
        .into_iter()
        .chain(readonly(&request))
        .chain(denied(&request))
    {
        assert!(!path.contains(r"\.git"), "{path}");
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
fn write_request_creates_the_private_launch_dir_and_writes_json_without_a_bom() {
    let temp = tempfile::tempdir().unwrap();
    let launch_dir = temp.path().join("launches").join(LAUNCH_ID);
    let existing = snapshot_existing();
    let directories = snapshot_directories();
    let exists = |path: &Path| existing.contains(path.to_str().unwrap());
    let is_dir = |path: &Path| directories.contains(path.to_str().unwrap());
    let request = build_request(&launch_input(&exists, &is_dir));

    let path = write_request(&launch_dir, &request).unwrap();

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

    let path = write_request(&launch_dir, &request).unwrap();

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&launch_dir), 0o700);
    assert_eq!(mode(&path), 0o600);
}

// ============================================================ refresh (AC 8)

#[test]
fn refresh_runs_fifty_seconds_before_expiry() {
    assert_eq!(refresh_at(1_000, 100), 950);
}

#[test]
fn refresh_never_runs_sooner_than_ten_seconds_from_now() {
    assert_eq!(refresh_at(1_000, 990), 1_000);
    assert_eq!(refresh_at(1_000, 995), 1_005);
    assert_eq!(refresh_at(1_000, 2_000), 2_010);
}

#[test]
fn refresh_does_not_underflow_for_an_early_expiry() {
    assert_eq!(refresh_at(30, 0), 10);
}

#[test]
fn a_later_expiry_is_rewritten() {
    assert_eq!(
        next_action(&RefreshInput::Ok(5_000), 1_000, 950),
        RefreshAction::Rewrite
    );
}

#[test]
fn the_same_expiry_keeps_the_file_and_retries() {
    assert_eq!(
        next_action(&RefreshInput::Ok(1_000), 1_000, 950),
        RefreshAction::Keep
    );
}

#[test]
fn a_gateway_error_or_timeout_keeps_the_file_until_it_expires() {
    for input in [
        RefreshInput::Status(StatusCode::UNAUTHORIZED),
        RefreshInput::Status(StatusCode::BAD_GATEWAY),
        RefreshInput::Timeout,
    ] {
        assert_eq!(
            next_action(&input, 1_000, 950),
            RefreshAction::Keep,
            "{input:?} before expiry"
        );
        assert_eq!(
            next_action(&input, 1_000, 1_001),
            RefreshAction::Delete,
            "{input:?} after expiry"
        );
    }
}

#[test]
fn a_policy_or_precondition_failure_deletes_the_file_at_once() {
    for status in [StatusCode::CONFLICT, StatusCode::FAILED_DEPENDENCY] {
        assert_eq!(
            next_action(&RefreshInput::Status(status), 1_000, 100),
            RefreshAction::Delete,
            "{status}"
        );
    }
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
        credential_expires_at_unix_seconds: None,
        integrity_drift: false,
        managed_settings_source: None,
        managed_hooks_locked: false,
        managed_mcp_locked: false,
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
fn the_credential_expiry_of_a_running_launch_can_be_updated() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.set_credential_expiry("a", 3_600);
    assert_eq!(
        registry.list()[0].credential_expires_at_unix_seconds,
        Some(3_600)
    );
    registry.set_credential_expiry("a", 7_200);
    assert_eq!(
        registry.list()[0].credential_expires_at_unix_seconds,
        Some(7_200)
    );
}

#[test]
fn updating_an_unknown_launch_changes_nothing() {
    let registry = LaunchRegistry::new();
    registry.insert(launch("a", 10));
    registry.mark_exited("nope", Some(1), 5);
    registry.set_credential_expiry("nope", 5);
    assert_eq!(registry.list(), vec![launch("a", 10)]);
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
    clock: Clock,
    integrity_reads: IntegrityReads,
    managed_file: Option<&'static str>,
    managed_file_error: Option<&'static str>,
}

impl LaunchHost for FakeHost {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
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
        &RealFs
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
        self.spawns.lock().unwrap().push(SpawnCall {
            launch_id: launch_id.to_owned(),
            executor: executor.to_owned(),
            request: request.to_owned(),
            log: log.to_owned(),
            request_json: std::fs::read_to_string(request).ok(),
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
    managed_file_error: Option<&'static str>,
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
            managed_file_error: None,
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
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

impl Harness {
    async fn start(setup: Setup) -> Self {
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
        });
        let executor_path = client_exe_dir.join("wxc-exec.exe");
        let availability = match setup.availability {
            Availability::Available => Some(ContainmentInfo {
                executor: executor_path.display().to_string(),
                available: true,
                error: None,
                managed_settings_source: None,
                managed_hooks_locked: false,
                managed_mcp_locked: false,
                managed_settings_error: None,
            }),
            Availability::Unavailable(error) => Some(ContainmentInfo {
                executor: executor_path.display().to_string(),
                available: false,
                error: Some(error.to_owned()),
                managed_settings_source: None,
                managed_hooks_locked: false,
                managed_mcp_locked: false,
                managed_settings_error: None,
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
            intercept: None,
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
                clock: clock.clone(),
                integrity_reads: integrity_reads.clone(),
                managed_file: setup.managed_file,
                managed_file_error: setup.managed_file_error,
            }),
            registry: LaunchRegistry::new(),
            availability,
            daemon_sid: setup.daemon_sid.map(str::to_owned),
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
        }
    }

    fn body(&self) -> serde_json::Value {
        serde_json::to_value(LaunchRequest {
            program: "claude-code".into(),
            cwd: self.cwd.clone(),
            args: vec![],
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

#[tokio::test(start_paused = true)]
async fn only_claude_code_can_be_launched() {
    let h = Harness::start(Setup::default()).await;
    for program in ["codex", "claude", ""] {
        let (status, body) = h
            .launch(&h.body_with(|body| body["program"] = program.into()))
            .await;
        assert_eq!(status, 400, "{program}: {body}");
        assert_eq!(body, "program must be claude-code", "{program}");
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
        (400, "program must be claude-code".to_owned())
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
async fn the_program_is_resolved_before_the_configuration_is_read() {
    let h = Harness::start(Setup {
        config: "controller:\n  address: https://controller.example.com\n",
        remote_config: Some("llmGateway: [this is not a gateway"),
        claude_installed: false,
        ..Setup::default()
    })
    .await;
    assert_eq!(h.launch(&h.body()).await.0, 400);
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
    let expires = launched
        .credential_expires_at_unix_seconds
        .expect("credential expiry");
    assert!(
        (BASE_TIME + CREDENTIAL_LIFETIME..BASE_TIME + CREDENTIAL_LIFETIME + 10).contains(&expires)
    );
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
    assert_eq!(spawn.token.as_deref(), Some("gateway-token-0"));
    let request: serde_json::Value =
        serde_json::from_str(spawn.request_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        request["containerId"],
        format!("agentdesktop-{}", launched.launch_id)
    );
    let env = env_map(&request);
    assert!(
        env["ANTHROPIC_BASE_URL"].starts_with("https://gateway.example.com"),
        "{env:?}"
    );
    assert!(
        env["AGENTDESKTOP_CREDENTIAL_FILE"].ends_with(r"credential\token"),
        "{env:?}"
    );
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
    assert_eq!(
        h.token(&launched.launch_id).as_deref(),
        Some("gateway-token-0")
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

    tokio::time::sleep(Duration::from_secs(2 * CREDENTIAL_LIFETIME)).await;
    assert_eq!(h.credentials.calls().len(), 1, "no refresh after the exit");
}

#[tokio::test(start_paused = true)]
async fn the_refresh_task_rewrites_the_token_before_it_expires_without_prompting() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;
    let first_expiry = launched.credential_expires_at_unix_seconds.unwrap();
    assert_eq!(
        h.token(&launched.launch_id).as_deref(),
        Some("gateway-token-0")
    );

    tokio::time::sleep(Duration::from_secs(CREDENTIAL_LIFETIME - 40)).await;

    assert_eq!(
        h.token(&launched.launch_id).as_deref(),
        Some("gateway-token-1")
    );
    let calls = h.credentials.calls();
    assert_eq!(calls[0], ("claude-code".to_owned(), true));
    assert_eq!(
        calls[1],
        ("claude-code".to_owned(), false),
        "the refresh never prompts"
    );
    let listed = h.listed().await;
    assert!(
        listed[0].credential_expires_at_unix_seconds.unwrap() > first_expiry,
        "the launch reports the new expiry"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_refused_by_policy_deletes_the_token_but_not_the_launch() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;
    h.credentials.fail_with(Some((
        StatusCode::CONFLICT,
        "programs.claudeCode must use the gateway",
    )));

    tokio::time::sleep(Duration::from_secs(CREDENTIAL_LIFETIME - 40)).await;

    assert_eq!(h.token(&launched.launch_id), None);
    assert_eq!(h.listed().await[0].status, LaunchStatus::Running);
}

#[tokio::test(start_paused = true)]
async fn a_gateway_error_keeps_the_token_until_it_expires_then_deletes_it() {
    let h = Harness::start(Setup::default()).await;
    let launched = h.launch_ok().await;
    h.credentials
        .fail_with(Some((StatusCode::BAD_GATEWAY, "upstream down")));

    tokio::time::sleep(Duration::from_secs(CREDENTIAL_LIFETIME - 40)).await;
    assert_eq!(
        h.token(&launched.launch_id).as_deref(),
        Some("gateway-token-0"),
        "a transient error keeps the still-valid token"
    );

    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        h.token(&launched.launch_id),
        None,
        "an expired token is removed"
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

    // Capture logs in a shared buffer
    let logs = Arc::new(Mutex::new(Vec::new()));
    let logs_writer = logs.clone();

    let subscriber = tracing_subscriber::registry().with(
        fmt::layer()
            .with_writer(move || LogWriter(logs_writer.clone()))
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
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
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
