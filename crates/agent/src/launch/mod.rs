//! Sandboxed launch of a managed program in a Windows AppContainer (MXC).
//!
//! `POST /v1/launch` writes an MXC request into a private launch directory,
//! registers a relay session and starts `wxc-exec.exe` on it. The box reaches
//! the gateway only through the local LLM proxy's relay. Everything that
//! touches the host (environment, filesystem, process creation, credentials)
//! goes through the traits below, so the behaviour is testable on every
//! platform; the production implementations live in [`host`].

use std::{
    collections::BTreeMap,
    io,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{api, secure_fs};
use agentdesktop_core::{
    config::{ContainmentStartupConfig, DaemonConfig},
    model::{
        ContainmentInfo, DaemonScope, Launch, LaunchRequest, LaunchStatus, LlmGatewayCredential,
    },
};
use anyhow::Context;
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};

pub(crate) mod engine;
pub mod git_ssh;
pub mod host;
pub mod integrity;
pub(crate) mod plan;
pub(crate) mod profile;
pub(crate) mod ssh;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod writer_tests;

/// Provider id of the only program the launcher can start.
const PROGRAM: &str = crate::provider::claude_code::ClaudeCode::ID;
/// Exited launches kept in the registry after the running ones.
const KEPT_EXITED: usize = 32;
/// How long the handler watches a fresh box for an early exit; the ssh
/// pipe setup polls for the box process within the same window (AC 3).
pub(crate) const EARLY_EXIT_WATCH: Duration = Duration::from_secs(3);
/// Poll cadence for the exit of a running launch.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Bytes of `wxc-exec.log` returned with an early-exit error.
const LOG_TAIL: usize = 4096;

/// Startup snapshot for `daemon-info`. `None` when `daemon.containment` is
/// unconfigured; otherwise `available` only with
/// `user && windows && exists(executor) && !claude_settings_overridden`.
/// The managed-settings posture fields are filled per request by `api::daemon_info`.
pub fn containment_info(
    cfg: Option<&ContainmentStartupConfig>,
    user: bool,
    windows: bool,
    claude_settings_overridden: bool,
    exists: &dyn Fn(&Path) -> bool,
    engine_name: &'static str,
) -> Option<ContainmentInfo> {
    let cfg = cfg?;
    let executor = cfg.executor.display().to_string();
    let error = if !user {
        Some("requires user mode".to_owned())
    } else if !windows {
        Some("Windows only".to_owned())
    } else if !exists(&cfg.executor) {
        Some(format!("executor not found: {executor}"))
    } else if claude_settings_overridden {
        Some(
            "daemon.claudeCode.config must be unset for run (the box reads <home>\\.claude\\settings.json)"
                .to_owned(),
        )
    } else {
        None
    };
    Some(ContainmentInfo {
        executor,
        engine: engine_name.to_owned(),
        available: error.is_none(),
        error,
        managed_settings_source: None,
        managed_hooks_locked: false,
        managed_mcp_locked: false,
        managed_settings_error: None,
    })
}

/// Identity of the process at the other end of a pipe connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipePeer {
    /// SID string of the peer's token user; `None` when it could not be read.
    pub user_sid: Option<String>,
    /// Whether the peer runs in an AppContainer.
    pub app_container: bool,
}

/// `user_sid == daemon_sid` and `!app_container`; a missing peer or an
/// unreadable SID is never allowed.
pub fn peer_allowed(peer: Option<&PipePeer>, daemon_sid: Option<&str>) -> bool {
    match (peer, daemon_sid) {
        (Some(peer), Some(daemon_sid)) => {
            !peer.app_container && peer.user_sid.as_deref() == Some(daemon_sid)
        }
        _ => false,
    }
}

/// Reads the identity of process `pid` (`TokenUser`, `TokenIsAppContainer`).
#[cfg(windows)]
pub use host::peer_identity;

/// `D:P(A;;GA;;;SY)(A;;GA;;;<sid>)`: the user-mode pipe admits SYSTEM and the
/// daemon's own user only.
pub fn user_mode_pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathKind {
    pub is_dir: bool,
}

pub trait CwdFs {
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn kind(&self, p: &Path) -> io::Result<PathKind>;
}

/// Raw (uncanonicalised) directories the workspace must not be, or be under.
#[derive(Clone, Debug)]
pub struct CwdContext {
    pub home: PathBuf,
    pub state_dir: PathBuf,
    pub system_dirs: Vec<PathBuf>,
    pub denied: Vec<PathBuf>,
    pub app_data: Vec<PathBuf>,
}

/// Characters `cmd.exe` interprets inside a quoted path.
const PATH_FORBIDDEN: [char; 3] = ['"', '%', '!'];

/// Removes the `\\?\` (and `\\?\UNC\`) prefix `canonicalize` adds on Windows.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// The form two paths are compared in: as they are on Unix, lower-cased on
/// Windows (NTFS is case-insensitive by default).
fn comparable(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_owned()
    }
}

fn is_under(path: &Path, directory: &Path) -> bool {
    comparable(path).starts_with(comparable(directory))
}

/// A drive, volume or UNC share root: no normal component at all.
fn is_root(path: &Path) -> bool {
    !path
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
}

/// Canonicalises `cwd` and applies the workspace rules; returns the canonical
/// path (`\\?\` stripped) or the reason for the rejection.
pub fn validate_cwd(cwd: &Path, ctx: &CwdContext, fs: &dyn CwdFs) -> Result<PathBuf, String> {
    let text = cwd.to_string_lossy();
    if text.contains(PATH_FORBIDDEN) {
        return Err("workspace path contains a character cmd.exe interprets".to_owned());
    }
    let canonical = fs
        .canonicalize(cwd)
        .map(strip_verbatim)
        .map_err(|error| format!("workspace {}: {error}", cwd.display()))?;
    match fs.kind(&canonical) {
        Ok(PathKind { is_dir: true }) => {}
        Ok(PathKind { is_dir: false }) => {
            return Err(format!("workspace {} is not a directory", cwd.display()));
        }
        Err(error) => return Err(format!("workspace {}: {error}", cwd.display())),
    }
    if canonical.to_string_lossy().contains(PATH_FORBIDDEN) {
        return Err("workspace path contains a character cmd.exe interprets".to_owned());
    }
    if is_root(&canonical) {
        return Err("workspace may not be a drive or share root".to_owned());
    }
    // Context directories are compared in their canonical form too, so a
    // junction or symlink to one of them is caught; a directory that does not
    // exist (such as `%ProgramFiles(x86)%` on ARM) still guards by its raw path.
    let canonical_dir = |raw: &Path| {
        fs.canonicalize(raw)
            .map(strip_verbatim)
            .unwrap_or_else(|_| raw.to_owned())
    };
    let home = canonical_dir(&ctx.home);
    if is_under(&home, &canonical) {
        return Err("workspace may not be the home directory or one of its ancestors".to_owned());
    }
    let mut guarded: Vec<(PathBuf, &str)> = vec![
        (canonical_dir(&ctx.state_dir), "the daemon state directory"),
        (canonical_dir(&home.join("AppData")), "AppData"),
        (
            canonical_dir(&home.join(".claude")),
            "the Claude Code directory",
        ),
    ];
    guarded.extend(
        ctx.system_dirs
            .iter()
            .map(|dir| (canonical_dir(dir), "a system directory")),
    );
    guarded.extend(
        ctx.denied
            .iter()
            .map(|dir| (canonical_dir(dir), "a denied directory")),
    );
    guarded.extend(
        ctx.app_data
            .iter()
            .map(|dir| (canonical_dir(dir), "AppData")),
    );
    for (directory, what) in guarded {
        if is_under(&canonical, &directory) {
            return Err(format!(
                "workspace may not be under {what} ({})",
                directory.display()
            ));
        }
        if is_under(&directory, &canonical) {
            return Err(format!(
                "workspace may not contain {what} ({})",
                directory.display()
            ));
        }
    }
    Ok(canonical)
}

pub trait ProgramFs {
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn is_file(&self, p: &Path) -> bool;
}

const PROGRAM_NOT_FOUND: &str = "Claude Code native install (claude.exe) not found for this user; npm installs are not supported in the box";

/// Finds the native `claude.exe` for the daemon's user: `<home>\.local\bin`
/// first, then each daemon `PATH` directory.
pub fn resolve_program(
    home: &Path,
    path_dirs: &[PathBuf],
    fs: &dyn ProgramFs,
) -> Result<PathBuf, String> {
    // An empty entry (a trailing `;` in PATH) would make every path "under" it.
    let path_dirs: Vec<&Path> = path_dirs
        .iter()
        .map(PathBuf::as_path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    let mut candidates = vec![home.join(".local").join("bin").join("claude.exe")];
    candidates.extend(path_dirs.iter().map(|dir| dir.join("claude.exe")));
    // A candidate counts only when it really lives under the user's home or a
    // PATH directory: both sides canonical, so a junction elsewhere is caught.
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in std::iter::once(home).chain(path_dirs.iter().copied()) {
        roots.push(root.to_owned());
        if let Ok(canonical) = fs.canonicalize(root) {
            roots.push(strip_verbatim(canonical));
        }
    }
    for candidate in candidates {
        if !fs.is_file(&candidate) {
            continue;
        }
        let Ok(canonical) = fs.canonicalize(&candidate).map(strip_verbatim) else {
            continue;
        };
        if roots.iter().any(|root| is_under(&canonical, root)) {
            return Ok(canonical);
        }
    }
    Err(PROGRAM_NOT_FOUND.to_owned())
}

/// Characters `cmd.exe` interprets even inside double quotes (`%`, `!`) or that
/// end or split the command (`" ^ & | < >`).
const ARG_FORBIDDEN: [char; 8] = ['"', '%', '^', '&', '|', '<', '>', '!'];

/// Rejects arguments `cmd.exe` would interpret and quotes the others.
pub fn check_args(args: &[String]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|arg| {
            if arg.contains(ARG_FORBIDDEN)
                || arg
                    .chars()
                    .any(|character| character.is_control() && character != '\t')
            {
                return Err("argument contains a character cmd.exe interprets".to_owned());
            }
            if arg.is_empty() {
                return Ok("\"\"".to_owned());
            }
            if !arg.contains([' ', '\t']) {
                return Ok(arg.clone());
            }
            // Inside quotes a trailing backslash would escape the closing
            // quote; the Windows argument parser wants each one doubled.
            let trailing = arg.len() - arg.trim_end_matches('\\').len();
            Ok(format!(
                "\"{}{}\"",
                &arg[..arg.len() - trailing],
                "\\".repeat(trailing * 2)
            ))
        })
        .collect()
}

/// Everything `plan` may use. Paths are Windows-style strings in a
/// `PathBuf`; `exists` is called with `\`-joined paths.
pub struct LaunchInput<'a> {
    pub launch_id: String,
    pub executable: PathBuf,
    pub client_exe_dir: PathBuf,
    pub cwd: PathBuf,
    /// Already quoted by `check_args`.
    pub args: Vec<String>,
    pub home: PathBuf,
    pub state_dir: PathBuf,
    pub launch_dir: PathBuf,
    pub git_dir: Option<PathBuf>,
    pub program_files_claude: Option<PathBuf>,
    pub egress: agentdesktop_core::config::ContainmentEgress,
    pub relay_base_url: String,
    pub pairing: String,
    pub base_env: BTreeMap<String, String>,
    /// Whether `daemon.containment.git.ssh.hosts` is non-empty: only then do
    /// the ssh env vars and file grants enter the request (AC 6; a launch
    /// without ssh keeps byte-identical request bytes, AC 9).
    pub ssh_enabled: bool,
    pub exists: &'a dyn Fn(&Path) -> bool,
    pub is_dir: &'a dyn Fn(&Path) -> bool,
}

/// A path as the request carries it: `\` separators only.
pub(crate) fn win(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn join(base: &str, rest: &str) -> String {
    format!("{}\\{rest}", base.trim_end_matches('\\'))
}

/// Resolves a launch input into a plan: the canonical executable, validated args,
/// final environment, and existence-filtered paths. Existence filtering reflects
/// MXC's grant rules. Pure: no host environment or filesystem reads beyond
/// `input.exists` and `input.is_dir`.
pub(crate) fn plan(input: &LaunchInput, profile: &profile::ClaudeCodeProfile) -> plan::LaunchPlan {
    let exists = |path: &str| (input.exists)(Path::new(path));
    let is_dir = |path: &str| (input.is_dir)(Path::new(path));
    let cwd = win(&input.cwd);
    let home = win(&input.home);
    let launch_dir = win(&input.launch_dir);
    let executable = win(&input.executable);
    let client_exe_dir = win(&input.client_exe_dir);
    let exe_dir = executable
        .rsplit_once('\\')
        .map(|(dir, _)| dir.to_owned())
        .unwrap_or_default();
    let git_dir = input.git_dir.as_deref().map(win);
    let program_files_claude = input.program_files_claude.as_deref().map(win);
    let claude_dir = join(&home, ".claude");
    let tmp = join(&launch_dir, "tmp");

    let system_root = input
        .base_env
        .get("SystemRoot")
        .cloned()
        .unwrap_or_else(|| r"C:\Windows".to_owned());
    let mut path = vec![
        join(&system_root, "System32"),
        system_root.clone(),
        join(&system_root, r"System32\WindowsPowerShell\v1.0"),
    ];
    // Windows OpenSSH (`ssh`, `ssh-add`) for the box's own ssh use; only with
    // ssh hosts, so launches without ssh keep byte-identical requests.
    if input.ssh_enabled {
        path.push(join(&system_root, r"System32\OpenSSH"));
    }
    if let Some(git_dir) = &git_dir {
        path.push(join(git_dir, "cmd"));
    }
    path.push(exe_dir.clone());
    path.push(client_exe_dir.clone());

    let mut env = input.base_env.clone();
    env.insert("PATH".into(), path.join(";"));
    env.insert("TEMP".into(), tmp.clone());
    env.insert("TMP".into(), tmp.clone());
    env.insert("CLAUDE_CONFIG_DIR".into(), claude_dir.clone());
    for (key, value) in profile.fixed_env {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    env.insert("ANTHROPIC_BASE_URL".into(), input.relay_base_url.clone());
    env.insert(
        "ANTHROPIC_CUSTOM_HEADERS".into(),
        format!("{}: {}", crate::llm_proxy::PAIRING_HEADER, input.pairing),
    );
    env.insert("HOME".into(), home.clone());
    // AC 6: the box's git reads only the generated per-launch gitconfig.
    // GIT_CONFIG_GLOBAL replaces both global files (XDG and ~/.gitconfig),
    // which also fixes the documented [include] fail-closed breakage and
    // stops the box reading PATs out of the user's configuration.
    let git_config = join(&launch_dir, GIT_CONFIG_FILE);
    env.insert("GIT_CONFIG_GLOBAL".into(), git_config.clone());
    // A 401 or refusal fails fast instead of git prompting on a console
    // that cannot answer.
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    // AC 6: with ssh hosts configured, the box's ssh talks to the per-launch
    // agent pipe (`IdentityAgent` did not work on Windows OpenSSH; the pipe
    // travels in SSH_AUTH_SOCK) and git runs ssh with the generated
    // per-launch client configuration. The pipe itself is created after
    // spawn, once the box's AppContainer SID can be read.
    if input.ssh_enabled {
        env.insert(
            "SSH_AUTH_SOCK".into(),
            format!("{}{}", ssh::pipe::AGENT_PIPE_PREFIX, input.launch_id),
        );
        // No shell between git and ssh: git runs a `GIT_SSH_COMMAND` (or
        // `core.sshCommand`) through `sh`, and Git for Windows' msys `sh`
        // cannot start in the AppContainer. `GIT_SSH` is run without a
        // shell; it names the launcher script, which adds
        // `-F <launch-dir>\ssh_config` (see `git_ssh`). The variant skips
        // git's `-G` probe of an unrecognized program name.
        env.insert("GIT_SSH".into(), join(&launch_dir, GIT_SSH_FILE));
        env.insert("GIT_SSH_VARIANT".into(), "ssh".into());
    }

    let mut readonly: Vec<String> = Vec::new();
    let mut denied: Vec<String> = Vec::new();
    // Runtime directories: granted read-only when they exist, simply absent
    // otherwise (nothing the box could create there matters). The profile's
    // home-relative entries keep their positions among the discovered
    // directories; the request bytes pin the order.
    let runtime = [
        Some(exe_dir),
        Some(join(&home, profile.runtime_share)),
        git_dir,
        Some(client_exe_dir),
        program_files_claude,
    ];
    for candidate in runtime.into_iter().flatten() {
        if exists(&candidate) && !readonly.contains(&candidate) {
            readonly.push(candidate);
        }
    }
    // The generated gitconfig: exactly the file, read-only (it is written
    // before the box starts).
    readonly.push(git_config);
    // AC 6: the generated ssh client configuration and git's ssh launcher
    // script, exactly the files, read-only (written before the box starts,
    // like the gitconfig).
    if input.ssh_enabled {
        readonly.push(join(&launch_dir, SSH_CONFIG_FILE));
        readonly.push(join(&launch_dir, GIT_SSH_FILE));
    }
    // Executable configuration: read-only when present, denied when absent, so
    // the box can neither change nor create it.
    let mut executable_config: Vec<String> = profile
        .home_executable_config
        .iter()
        .map(|entry| join(&claude_dir, entry))
        .collect();
    executable_config.push(join(&cwd, profile.mcp_json));
    executable_config.push(join(&cwd, profile.workspace_head));
    // MXC refuses a denied path whose parent folder does not exist
    // (lab-verified, 0x80070003), so a workspace without `.claude` gets the
    // folder itself denied instead of its executable-config entries.
    let workspace_claude = join(&cwd, ".claude");
    let workspace_claude_entries: Vec<String> = profile
        .workspace_claude_entries
        .iter()
        .map(|entry| join(&workspace_claude, entry))
        .collect();
    if exists(&workspace_claude) || workspace_claude_entries.iter().any(|path| exists(path)) {
        executable_config.extend(workspace_claude_entries);
    } else {
        denied.push(workspace_claude);
    }
    let git = join(&cwd, ".git");
    if !exists(&git) {
        // No repository: deny `.git` so the box cannot `git init` one that
        // the user's tools later trust.
        denied.push(git);
    } else if is_dir(&git) {
        // A `.git` directory: emit config, hooks and commondir guards whether
        // or not they exist. An empty directory gets the same guards as a
        // populated repository (prevents `git init` into an existing empty
        // folder). `.git\HEAD` stays writable: git replaces it by rename on
        // every branch switch, and the root `HEAD` rule already blocks a
        // bare-repo fall-through.
        executable_config.extend(
            profile
                .git_executable_config
                .iter()
                .map(|entry| join(&git, entry)),
        );
    }
    // A `.git` file (worktree, submodule) is left as it is.
    for path in executable_config {
        if exists(&path) {
            readonly.push(path);
        } else {
            denied.push(path);
        }
    }
    for secret in profile.denied_secrets {
        denied.push(join(&home, secret));
    }
    // AC 6: both real git user-configuration paths are denied, not granted
    // (the box reads the generated file via GIT_CONFIG_GLOBAL). Under MXC an
    // ungranted home path is unreachable by default, but we deny explicitly,
    // matching the denied-secrets posture. MXC's denied-path rule requires
    // the parent directory to exist at launch time (CreateProcessSecurityEnvironment
    // 0x80070003), else launch fails. `~/.gitconfig`'s parent ($HOME) always
    // exists; the XDG path's parent (`~/.config/git`) may not, so we only
    // deny it when the parent is present. Skipping the XDG deny when the
    // parent is absent is safe: under MXC that home path is unreachable anyway.
    denied.push(join(&home, profile.git_user_config));
    let xdg_config_path = join(&home, profile.git_xdg_config);
    // The XDG config path's parent directory may not exist on Windows hosts
    // without `~/.config/git`. MXC's denied-path rule requires the parent to
    // exist (CreateProcessSecurityEnvironment 0x80070003), so we only deny
    // the XDG config when the parent exists. Extract the parent by finding
    // the last backslash (these are Windows `\`-joined paths, even when
    // tested on Linux).
    if let Some(last_sep) = xdg_config_path.rfind('\\') {
        let parent = &xdg_config_path[..last_sep];
        if exists(parent) {
            denied.push(xdg_config_path);
        }
    }

    let mut readwrite = vec![
        PathBuf::from(cwd),
        PathBuf::from(claude_dir),
        PathBuf::from(tmp),
    ];
    // AC 6: the per-launch known_hosts is read-write, exactly the file, so
    // `accept-new` persists across connections within the launch. Worst case
    // of the grant is the box corrupting its own per-launch file (self-DoS;
    // it dies with the launch directory; no secret).
    if input.ssh_enabled {
        readwrite.push(PathBuf::from(join(&launch_dir, KNOWN_HOSTS_FILE)));
    }

    plan::LaunchPlan {
        program: input.executable.clone(),
        args: input.args.clone(),
        window_title: profile.window_title,
        cwd: input.cwd.clone(),
        env,
        readwrite,
        readonly: readonly.into_iter().map(PathBuf::from).collect(),
        denied: denied.into_iter().map(PathBuf::from).collect(),
        egress: input.egress,
    }
}

/// Creates `launch_dir` (private) and writes `request.json` (UTF-8, no BOM,
/// mode 0600) into it; returns the file's path.
pub fn write_request(launch_dir: &Path, contents: &[u8]) -> anyhow::Result<PathBuf> {
    secure_fs::ensure_private_dir(launch_dir)?;
    let path = launch_dir.join("request.json");
    secure_fs::atomic_write(&path, contents, 0o600)?;
    Ok(path)
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn credential(
        &self,
        client_id: &str,
        interactive: bool,
    ) -> Result<LlmGatewayCredential, (StatusCode, String)>;
}

/// The effective-configuration rules a launch needs; each failure names the
/// field.
pub(crate) fn preconditions(effective: &DaemonConfig) -> Result<(), String> {
    let Some(gateway) = effective.llm_gateway.as_ref() else {
        return Err("llmGateway (with authentication) must be configured for run".to_owned());
    };
    if gateway.authentication.is_none() {
        return Err("llmGateway.authentication must be configured for run".to_owned());
    }
    let Some(claude_code) = effective.programs.claude_code.as_ref() else {
        return Err("programs.claudeCode must be configured for run".to_owned());
    };
    if !claude_code.use_llm_gateway {
        return Err("programs.claudeCode.useLlmGateway must be true for run".to_owned());
    }
    if api::program_uses_subscription(effective, PROGRAM) {
        return Err(
            "programs.claudeCode.auth: subscription sign-in is not available inside the box in PR 1"
                .to_owned(),
        );
    }
    if effective.sandbox.is_some() {
        return Err(
            "Claude Code's own sandbox is not available inside the box; remove `sandbox` or launch without `run`"
                .to_owned(),
        );
    }
    Ok(())
}

pub trait LaunchChild: Send {
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    fn pid(&self) -> u32;
    /// Creation time of the process (FILETIME on Windows).
    fn start_time(&self) -> u64;
    /// Stops the process; `Ok` if it has already exited.
    fn kill(&mut self) -> io::Result<()>;
}

/// Hex of a file's content hash; "N/A" for everything that is not a file.
fn format_sha256(snapshot: &integrity::FileSnapshot) -> String {
    match snapshot {
        integrity::FileSnapshot::File { sha256, .. } => {
            let mut hex = String::with_capacity(64);
            for byte in sha256 {
                hex.push_str(&format!("{:02x}", byte));
            }
            hex
        }
        _ => "N/A".to_string(),
    }
}

/// The launch-time integrity state: the checked roots, their snapshot taken
/// before spawn, and the filesystem the checks read.
struct IntegrityCheck {
    roots: Vec<integrity::IntegrityRoot>,
    baseline: BTreeMap<PathBuf, integrity::FileSnapshot>,
    fs: Arc<dyn integrity::IntegrityFs>,
}

impl IntegrityCheck {
    /// Verifies against the launch-time snapshot off the async runtime, logs
    /// each change, and returns whether anything changed.
    async fn verify_and_log(&self, launch_id: &str) -> bool {
        let roots = self.roots.clone();
        let baseline = self.baseline.clone();
        let fs = self.fs.clone();
        let result = match tokio::task::spawn_blocking(move || {
            integrity::verify(&roots, &baseline, fs.as_ref())
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(launch_id, %error, "integrity check failed");
                return false;
            }
        };
        let mut any = false;
        for (path, change) in result.changed {
            any = true;
            let before_sha256 = format_sha256(&change.before);
            let after_sha256 = format_sha256(&change.after);
            tracing::warn!(
                launch_id,
                path = %path.display(),
                change = %change.kind,
                before_sha256,
                after_sha256,
                "executable config changed while launch ran"
            );
        }
        any
    }
}

fn stop_executor(child: &mut dyn LaunchChild, launch_id: &str) {
    if let Err(error) = child.kill() {
        tracing::warn!(launch_id, %error, "failed to stop the executor process");
    }
}

pub trait Executor: Send + Sync {
    fn spawn(
        &self,
        launch_id: &str,
        executor: &Path,
        request: &Path,
        log: &Path,
    ) -> io::Result<Box<dyn LaunchChild>>;
}

pub trait LaunchHost: Send + Sync {
    fn home(&self) -> PathBuf;
    fn path_dirs(&self) -> Vec<PathBuf>;
    fn base_env(&self) -> BTreeMap<String, String>;
    fn cwd_context(&self) -> CwdContext;
    fn cwd_fs(&self) -> &dyn CwdFs;
    fn program_fs(&self) -> &dyn ProgramFs;
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn git_dir(&self) -> Option<PathBuf>;
    fn program_files_claude(&self) -> Option<PathBuf>;
    fn client_exe_dir(&self) -> PathBuf;
    fn now(&self) -> u64;

    /// The file stem of the agentdesktop executable in `client_exe_dir`: the
    /// interpreter of git's ssh launcher script.
    fn client_exe_name(&self) -> String {
        "agentdesktop".to_owned()
    }

    fn integrity_fs(&self) -> Arc<dyn integrity::IntegrityFs> {
        Arc::new(integrity::REAL_FS)
    }
    fn managed_sources(&self) -> crate::provider::claude_code::managed::ManagedSources;

    /// The user's global git identity values the generated gitconfig copies
    /// read-only (AC 6), read host-side via `git config --global --get`
    /// (`--get-all` for the multi-valued `safe.directory`), which honors
    /// includes. Keys with no value are absent.
    fn git_identity(&self) -> Vec<(String, Vec<String>)> {
        read_git_identity()
    }

    /// The user's `known_hosts` entries for `hosts`, seeding the per-launch
    /// `known_hosts` file. The default reads them host-side with
    /// `ssh-keygen -F <host>`, comment lines (`# Host <h> found: ...`)
    /// stripped. A host without entries, or a missing `ssh-keygen`, is simply
    /// skipped: `StrictHostKeyChecking accept-new` then records that host's
    /// key on first use, per launch.
    fn known_hosts_seed(&self, hosts: &[String]) -> Vec<String> {
        let mut lines = Vec::new();
        for host in hosts {
            let Ok(output) = std::process::Command::new("ssh-keygen")
                .args(["-F", host])
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            lines.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(str::to_owned),
            );
        }
        lines
    }
}

/// Running launches plus the last 32 exited ones. Clones share state.
#[derive(Clone)]
pub struct LaunchRegistry {
    launches: Arc<Mutex<Vec<Launch>>>,
}

impl LaunchRegistry {
    pub fn new() -> Self {
        Self {
            launches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Launch>> {
        self.launches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn insert(&self, launch: Launch) {
        self.lock().push(launch);
    }

    pub fn mark_exited(&self, launch_id: &str, exit_code: Option<i32>, ended_at: u64) {
        let mut launches = self.lock();
        let Some(launch) = launches
            .iter_mut()
            .find(|launch| launch.launch_id == launch_id)
        else {
            return;
        };
        launch.status = LaunchStatus::Exited;
        launch.exit_code = exit_code;
        launch.ended_at_unix_seconds = Some(ended_at);
        // Keep every running launch and the most recently ended exits.
        let mut exited: Vec<usize> = launches
            .iter()
            .enumerate()
            .filter(|(_, launch)| launch.status == LaunchStatus::Exited)
            .map(|(index, _)| index)
            .collect();
        if exited.len() > KEPT_EXITED {
            exited.sort_by_key(|&index| {
                (
                    launches[index].ended_at_unix_seconds,
                    launches[index].started_at_unix_seconds,
                )
            });
            let mut drop: Vec<usize> = exited[..exited.len() - KEPT_EXITED].to_vec();
            drop.sort_unstable_by(|a, b| b.cmp(a));
            for index in drop {
                launches.remove(index);
            }
        }
    }

    pub fn set_integrity_drift(&self, launch_id: &str) {
        if let Some(launch) = self
            .lock()
            .iter_mut()
            .find(|launch| launch.launch_id == launch_id)
        {
            launch.integrity_drift = true;
        }
    }

    /// Ordered by `started_at_unix_seconds` ascending.
    pub fn list(&self) -> Vec<Launch> {
        let mut launches = self.lock().clone();
        launches.sort_by_key(|launch| launch.started_at_unix_seconds);
        launches
    }
}

impl Default for LaunchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct LaunchState {
    pub app: api::AppState,
    pub executor: Arc<dyn Executor>,
    pub credentials: Arc<dyn CredentialSource>,
    pub host: Arc<dyn LaunchHost>,
    pub registry: LaunchRegistry,
    pub availability: Option<ContainmentInfo>,
    /// The daemon's own SID; `Some` only for a Windows user-mode daemon.
    pub daemon_sid: Option<String>,
    pub(crate) sessions: crate::llm_proxy::sessions::SessionRegistry,
    pub(crate) proxy_address: Option<std::net::SocketAddr>,
    pub(crate) session_policy: Arc<dyn crate::llm_proxy::sessions::SessionPolicy>,
    pub(crate) egress: agentdesktop_core::config::ContainmentEgress,
    /// `daemon.containment.git`: the generated gitconfig's insteadOf rewrites
    /// come from its `allow` list.
    pub(crate) git: Option<agentdesktop_core::config::GitConfig>,
    pub(crate) engine: Arc<dyn engine::Engine>,
}

/// `POST /v1/launch` and `GET /v1/launches`, behind the peer check.
pub fn router(state: LaunchState) -> Router {
    Router::new()
        .route("/v1/launch", post(launch))
        .route("/v1/launches", get(launches))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// System mode has no user to launch for; in Windows user mode only the
/// daemon's own user outside an AppContainer may use the launch routes.
async fn guard(State(state): State<LaunchState>, request: Request, next: Next) -> Response {
    if state.app.daemon_info.scope == DaemonScope::System {
        return (
            StatusCode::CONFLICT,
            "containment needs the user-mode daemon (`agentdesktop daemon --user`); this is the system daemon",
        )
            .into_response();
    }
    if let Some(daemon_sid) = state.daemon_sid.as_deref()
        && !peer_allowed(request.extensions().get::<PipePeer>(), Some(daemon_sid))
    {
        return (
            StatusCode::FORBIDDEN,
            "launch is available to the daemon's own user only",
        )
            .into_response();
    }
    next.run(request).await
}

async fn launches(State(state): State<LaunchState>) -> Json<Vec<Launch>> {
    Json(state.registry.list())
}

type LaunchError = (StatusCode, String);

fn bad_request(message: String) -> LaunchError {
    (StatusCode::BAD_REQUEST, message)
}

async fn launch(
    State(state): State<LaunchState>,
    Json(request): Json<LaunchRequest>,
) -> Result<(StatusCode, Json<Launch>), LaunchError> {
    let executor = match state.availability.as_ref() {
        Some(info) if info.available => PathBuf::from(&info.executor),
        Some(info) => {
            return Err((
                StatusCode::CONFLICT,
                info.error
                    .clone()
                    .unwrap_or_else(|| "containment is unavailable".to_owned()),
            ));
        }
        None => {
            return Err((
                StatusCode::CONFLICT,
                "containment is not configured (daemon.containment.executor)".to_owned(),
            ));
        }
    };
    if request.program != PROGRAM {
        return Err(bad_request(format!("program must be {PROGRAM}")));
    }
    let host = state.host.clone();
    let cwd =
        validate_cwd(&request.cwd, &host.cwd_context(), host.cwd_fs()).map_err(bad_request)?;
    let args = check_args(&request.args).map_err(bad_request)?;
    let executable =
        resolve_program(&host.home(), &host.path_dirs(), host.program_fs()).map_err(bad_request)?;
    if win(&executable).contains(PATH_FORBIDDEN) {
        return Err(bad_request(
            "program path contains a character cmd.exe interprets".to_owned(),
        ));
    }
    let effective =
        api::load_effective_config(&state.app.config, &state.app.state_dir).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read applied configuration: {error:#}"),
            )
        })?;
    preconditions(&effective).map_err(|message| (StatusCode::CONFLICT, message))?;
    let Some(proxy_address) = state.proxy_address else {
        return Err((
            StatusCode::CONFLICT,
            "the local LLM proxy is not running (daemon.llmProxy.listen)".to_owned(),
        ));
    };
    // Refuse a full registry before any interactive sign-in starts; the
    // register call below stays the authoritative check.
    if state.sessions.len(state.host.now()) >= crate::llm_proxy::sessions::MAX_SESSIONS {
        return Err(too_many_sessions());
    }
    state.session_policy.check(PROGRAM).await?;
    let _credential = state.credentials.credential(PROGRAM, true).await?;

    // From here on the work runs in its own task: a client that disconnects
    // during the early-exit watch must not leave a running box unregistered,
    // unpolled and without its session revoked.
    let started = tokio::spawn(start_launch(
        state,
        StartInput {
            executor,
            executable,
            cwd,
            args,
            proxy_address,
        },
    ));
    match started.await {
        Ok(result) => result,
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("launch task failed: {error}"),
        )),
    }
}

fn too_many_sessions() -> LaunchError {
    (
        StatusCode::TOO_MANY_REQUESTS,
        crate::llm_proxy::sessions::too_many_sessions_message(),
    )
}

/// What `start_launch` needs beyond the state: the checked request.
struct StartInput {
    executor: PathBuf,
    executable: PathBuf,
    cwd: PathBuf,
    args: Vec<String>,
    proxy_address: std::net::SocketAddr,
}

/// Creates the launch directory, starts the executor, watches it for an early
/// exit and registers the launch with its poller.
async fn start_launch(
    state: LaunchState,
    input: StartInput,
) -> Result<(StatusCode, Json<Launch>), LaunchError> {
    let host = state.host.clone();
    let launch_id = format!("{:016x}", rand::random::<u64>());
    let state_dir = std::path::absolute(&state.app.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("resolve the daemon state directory: {error}"),
        )
    })?;

    let session = match state.sessions.register(
        PROGRAM,
        &format!("run {}", launch_id),
        crate::llm_proxy::sessions::DEFAULT_IDLE_TTL,
        host.now(),
    ) {
        Ok(session) => session,
        Err(crate::llm_proxy::sessions::SessionError::Full) => {
            return Err(too_many_sessions());
        }
        Err(crate::llm_proxy::sessions::SessionError::InvalidClientId) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid session request".to_owned(),
            ));
        }
        Err(crate::llm_proxy::sessions::SessionError::InvalidLabel) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid session request".to_owned(),
            ));
        }
    };
    let session_id = session.id.clone();

    let launch_dir = state_dir.join("launches").join(&launch_id);
    let log_path = launch_dir.join("wxc-exec.log");
    let fail = |launch_dir: &Path, message: String| {
        state.sessions.revoke(&session_id);
        tracing::warn!(
            launch_id = %launch_id,
            session_id = %session_id,
            %message,
            "sandboxed launch failed; its session is revoked"
        );
        let _ = std::fs::remove_dir_all(launch_dir);
        Err((StatusCode::INTERNAL_SERVER_ERROR, message))
    };
    // `~\.claude` is granted read-write and its entries are denied or
    // read-only: every one of those needs the folder to exist.
    if let Err(error) = std::fs::create_dir_all(host.home().join(".claude")) {
        return fail(
            &launch_dir,
            format!("prepare launch: create ~\\.claude: {error}"),
        );
    }
    if let Err(error) = create_launch_dir(&launch_dir) {
        return fail(&launch_dir, format!("prepare launch: {error:#}"));
    }
    // AC 6: the per-launch gitconfig is written before the box starts; the
    // plan grants exactly this file read-only and points GIT_CONFIG_GLOBAL
    // at it.
    let git_allow = state
        .git
        .as_ref()
        .map(|git| git.allow.clone())
        .unwrap_or_default();
    // AC 6: `git.ssh` with a non-empty host list is the whole ssh
    // surface switch; absent or empty, nothing below changes a launch.
    let git_ssh = state
        .git
        .as_ref()
        .and_then(|git| git.ssh.as_ref())
        .filter(|ssh| !ssh.hosts.is_empty())
        .cloned();
    let ssh_hosts: Vec<String> = git_ssh
        .as_ref()
        .map(|ssh| ssh.hosts.clone())
        .unwrap_or_default();
    // The identity read spawns up to a handful of `git config` processes:
    // off the async runtime, like the posture snapshot below.
    let identity = {
        let host = host.clone();
        tokio::task::spawn_blocking(move || host.git_identity())
            .await
            .unwrap_or_default()
    };
    if let Err(error) = generate_gitconfig(
        &launch_dir,
        &session.pairing,
        input.proxy_address.port(),
        &git_allow,
        &identity,
        &ssh_hosts,
    ) {
        return fail(&launch_dir, format!("prepare launch: {error:#}"));
    }
    if let Some(ssh) = &git_ssh {
        // The seed read shells `ssh-keygen -F` host-side: off the async
        // runtime, like the identity read above.
        let seeds = {
            let host = host.clone();
            let hosts = ssh.hosts.clone();
            tokio::task::spawn_blocking(move || host.known_hosts_seed(&hosts))
                .await
                .unwrap_or_default()
        };
        // AC 5: one loopback leg per ssh host, bound BEFORE spawn so the
        // assigned ports go into the generated ssh_config.
        let mut legs = Vec::new();
        for ssh_host in &ssh.hosts {
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(listener) => listener,
                Err(error) => {
                    return fail(
                        &launch_dir,
                        format!("bind an ssh leg for {ssh_host}: {error}"),
                    );
                }
            };
            let port = match listener.local_addr() {
                Ok(address) => address.port(),
                Err(error) => {
                    return fail(
                        &launch_dir,
                        format!("bind an ssh leg for {ssh_host}: {error}"),
                    );
                }
            };
            legs.push((ssh_host.clone(), listener, port));
        }
        let leg_ports: Vec<(String, u16)> = legs
            .iter()
            .map(|(ssh_host, _, port)| (ssh_host.clone(), *port))
            .collect();
        if let Err(error) =
            generate_ssh_files(&launch_dir, &leg_ports, &seeds, &host.client_exe_name())
        {
            return fail(&launch_dir, format!("prepare launch: {error:#}"));
        }
        // The leg tasks hold the session registry and die with the session
        // (D7): every existing stop path revokes it, including the `fail`
        // closure above, so a failed launch closes its legs with no new hook.
        for (ssh_host, listener, port) in legs {
            tokio::spawn(ssh::serve_leg(
                listener,
                ssh_host,
                port,
                state.sessions.clone(),
                session_id.clone(),
                launch_id.clone(),
                session_clock(&host),
            ));
        }
    }
    // AC 4: deny is the default; an explicit allow is loud.
    warn_on_open_egress(state.egress);
    let exists = |path: &Path| host.exists(path);
    let is_dir = |path: &Path| host.is_dir(path);
    let launch_plan = plan(
        &LaunchInput {
            launch_id: launch_id.clone(),
            executable: input.executable,
            client_exe_dir: host.client_exe_dir(),
            cwd: input.cwd.clone(),
            args: input.args,
            home: host.home(),
            state_dir,
            launch_dir: launch_dir.clone(),
            git_dir: host.git_dir(),
            program_files_claude: host.program_files_claude(),
            egress: state.egress,
            relay_base_url: crate::llm_proxy::sessions::sandbox_base_url(input.proxy_address),
            pairing: session.pairing.to_string(),
            base_env: host.base_env(),
            ssh_enabled: git_ssh.is_some(),
            exists: &exists,
            is_dir: &is_dir,
        },
        &profile::CLAUDE_CODE,
    );
    let request_bytes = match state.engine.request(&launch_plan, &launch_id) {
        Ok(bytes) => bytes,
        Err(error) => return fail(&launch_dir, format!("encode launch request: {error:#}")),
    };
    let request_path = match write_request(&launch_dir, &request_bytes) {
        Ok(path) => path,
        Err(error) => return fail(&launch_dir, format!("prepare launch: {error:#}")),
    };

    // Take the integrity snapshot before spawn
    let integrity_roots = integrity::integrity_paths(&host.home(), &input.cwd);
    let integrity_fs = host.integrity_fs();
    let baseline = tokio::task::spawn_blocking({
        let roots = integrity_roots.clone();
        let fs = integrity_fs.clone();
        move || integrity::snapshot(&roots, fs.as_ref())
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(launch_id, %error, "integrity snapshot failed; every root will be reported as appeared");
        BTreeMap::new()
    });

    // Over-budget roots are watched by structure (names, sizes, identities),
    // not content; that is expected on real machines (plugins), so debug.
    for (path, snap) in &baseline {
        match snap {
            integrity::FileSnapshot::TooLarge { .. } => {
                tracing::debug!(launch_id, path = %path.display(), "integrity check: over the content budget, structure only");
            }
            integrity::FileSnapshot::Unreadable { kind } => {
                tracing::warn!(launch_id, path = %path.display(), ?kind, "integrity check: root unreadable at launch; content unwatched while it stays so");
            }
            _ => {}
        }
    }
    let integrity_check = IntegrityCheck {
        roots: integrity_roots,
        baseline,
        fs: integrity_fs,
    };

    // Snapshot the managed-settings posture before spawning the executor
    let managed_posture = {
        let host_clone = host.clone();
        match tokio::task::spawn_blocking(move || {
            crate::provider::claude_code::managed::managed_settings_state(
                &host_clone.managed_sources(),
            )
        })
        .await
        {
            Ok(posture) => posture,
            Err(error) => {
                tracing::warn!(launch_id = %launch_id, "failed to join managed-settings posture task: {error:#}");
                crate::provider::claude_code::managed::ManagedSettingsPosture::default()
            }
        }
    };

    if let Some(error) = &managed_posture.error {
        tracing::warn!(
            launch_id = %launch_id,
            error = %error,
            "managed-settings posture could not be determined at launch"
        );
    }

    let mut child =
        match state
            .executor
            .spawn(&launch_id, &input.executor, &request_path, &log_path)
        {
            Ok(child) => child,
            Err(error) => return fail(&launch_dir, format!("start wxc-exec: {error}")),
        };
    let started_at = host.now();
    // The pid file is the sweep's only way to tell a live box from a stale
    // directory; without it the launch is not started.
    if let Err(error) = std::fs::write(
        launch_dir.join("pid"),
        format!("{} {}", child.pid(), child.start_time()),
    ) {
        stop_executor(child.as_mut(), &launch_id);
        integrity_check.verify_and_log(&launch_id).await;
        return fail(&launch_dir, format!("record the launch pid: {error}"));
    }

    // AC 3, fail-closed: with ssh hosts configured, ANY ssh setup
    // failure (SID read, pipe create) stops and fails the launch — the box
    // env already names the pipe, so a launch must not run pointed at a name
    // the daemon does not own. The pipe is created after spawn because the
    // box's per-launch AppContainer SID can only be read from the running
    // box. The `fail` closure revokes the session, which also closes the
    // legs bound above (D7).
    if let Some(ssh) = &git_ssh
        && let Err(error) = ssh::pipe::start_agent_pipe_server(
            &launch_id,
            child.pid(),
            child.start_time(),
            state.daemon_sid.as_deref(),
            ssh::AgentPolicy::from_config_keys(&ssh.keys),
            state.sessions.clone(),
            session_id.clone(),
            session_clock(&host),
        )
        .await
    {
        // A box that already died is reported like any early exit, with the
        // executor's log tail, rather than as a missing box process.
        let message = match child.try_wait() {
            Ok(Some(code)) => format!(
                "wxc-exec exited with code {code} within {}s: {}",
                EARLY_EXIT_WATCH.as_secs(),
                log_tail(&log_path)
            ),
            _ => format!("ssh agent proxy setup: {error:#}"),
        };
        stop_executor(child.as_mut(), &launch_id);
        integrity_check.verify_and_log(&launch_id).await;
        return fail(&launch_dir, message);
    }

    // A box that dies within seconds (bad request, missing tier, MXC refusal)
    // is reported to the caller instead of lingering as a running launch.
    let deadline = tokio::time::Instant::now() + EARLY_EXIT_WATCH;
    loop {
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(code)) => {
                integrity_check.verify_and_log(&launch_id).await;
                let tail = log_tail(&log_path);
                return fail(
                    &launch_dir,
                    format!(
                        "wxc-exec exited with code {code} within {}s: {tail}",
                        EARLY_EXIT_WATCH.as_secs()
                    ),
                );
            }
            Err(error) => {
                stop_executor(child.as_mut(), &launch_id);
                integrity_check.verify_and_log(&launch_id).await;
                return fail(&launch_dir, format!("watch wxc-exec: {error}"));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let launch = Launch {
        launch_id: launch_id.clone(),
        program: PROGRAM.to_owned(),
        cwd: input.cwd,
        container_id: state.engine.container_id(&launch_id),
        status: LaunchStatus::Running,
        exit_code: None,
        started_at_unix_seconds: started_at,
        ended_at_unix_seconds: None,
        integrity_drift: false,
        managed_settings_source: managed_posture.source,
        managed_hooks_locked: managed_posture.hooks_locked,
        managed_mcp_locked: managed_posture.mcp_locked,
        session_id: session_id.clone(),
    };
    state.registry.insert(launch.clone());
    tracing::info!(launch_id, cwd = %launch.cwd.display(), "sandboxed launch started");
    tracing::info!(launch_id, session_id, "sandboxed launch session created");

    // Start periodic integrity check
    let periodic = {
        let registry = state.registry.clone();
        let launch_id_clone = launch_id.clone();
        let on_drift = Arc::new(move |path: PathBuf, change: integrity::Change| {
            let before_sha256 = format_sha256(&change.before);
            let after_sha256 = format_sha256(&change.after);
            tracing::warn!(
                launch_id = launch_id_clone,
                path = %path.display(),
                change = %change.kind,
                before_sha256,
                after_sha256,
                "executable config changed during launch"
            );
            registry.set_integrity_drift(&launch_id_clone);
        });
        tokio::spawn(integrity::run_periodic(
            integrity_check.roots.clone(),
            integrity_check.baseline.clone(),
            integrity_check.fs.clone(),
            Duration::from_secs(60),
            on_drift,
        ))
    };

    tokio::spawn(poll_task(
        child,
        state.sessions.clone(),
        session_id,
        periodic.abort_handle(),
        state.registry.clone(),
        host.clone(),
        launch_id,
        launch_dir,
        integrity_check,
    ));
    Ok((StatusCode::CREATED, Json(launch)))
}

/// The launch host's clock, for the ssh accept loops' session checks.
fn session_clock(host: &Arc<dyn LaunchHost>) -> ssh::SessionClock {
    let host = host.clone();
    Arc::new(move || host.now())
}

/// Creates the private launch directory with the box's `tmp`.
pub(crate) fn create_launch_dir(launch_dir: &Path) -> anyhow::Result<()> {
    secure_fs::ensure_private_dir(launch_dir)?;
    std::fs::create_dir_all(launch_dir.join("tmp")).context("create launch tmp")?;
    Ok(())
}

/// The last 4 KiB of the executor's log, for an early-exit error.
fn log_tail(log: &Path) -> String {
    let bytes = std::fs::read(log).unwrap_or_default();
    let start = bytes.len().saturating_sub(LOG_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Watches the executor: copies new log lines to the daemon log. When it exits,
/// or when it can no longer be watched (then it is stopped first), revokes the
/// session, records the exit and removes the launch directory.
#[allow(clippy::too_many_arguments)]
async fn poll_task(
    mut child: Box<dyn LaunchChild>,
    sessions: crate::llm_proxy::sessions::SessionRegistry,
    session_id: String,
    periodic: tokio::task::AbortHandle,
    registry: LaunchRegistry,
    host: Arc<dyn LaunchHost>,
    launch_id: String,
    launch_dir: PathBuf,
    integrity_check: IntegrityCheck,
) {
    let log_path = launch_dir.join("wxc-exec.log");
    let mut log_offset = 0u64;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        log_offset = copy_log_lines(&launch_id, &log_path, log_offset);
        let exit = match child.try_wait() {
            Ok(None) => continue,
            Ok(Some(code)) => Some(code),
            Err(error) => {
                tracing::warn!(launch_id, %error, "lost track of the launch; stopping it");
                stop_executor(child.as_mut(), &launch_id);
                None
            }
        };

        // Stop the background tasks, then verify once against the launch
        // snapshot before the launch is recorded as exited.
        periodic.abort();

        if integrity_check.verify_and_log(&launch_id).await {
            registry.set_integrity_drift(&launch_id);
        }
        sessions.revoke(&session_id);

        registry.mark_exited(&launch_id, exit, host.now());
        if let Err(error) = std::fs::remove_dir_all(&launch_dir) {
            tracing::warn!(launch_id, %error, "failed to remove the launch directory");
        }
        tracing::info!(launch_id, exit_code = ?exit, "sandboxed launch exited");
        return;
    }
}

/// Logs the complete lines `wxc-exec.log` gained since `offset`; returns the
/// new offset.
fn copy_log_lines(launch_id: &str, log_path: &Path, offset: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(log_path) else {
        return offset;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return offset;
    }
    // Find the last newline in the raw bytes (not in the lossy string) so
    // non-UTF-8 bytes don't shift the file offset.
    let Some(complete) = bytes.iter().rposition(|&byte| byte == b'\n') else {
        return offset;
    };
    let text = String::from_utf8_lossy(&bytes[..complete]);
    for line in text.lines() {
        tracing::info!(launch_id, "wxc-exec: {line}");
    }
    offset + complete as u64 + 1
}

/// Removes what a previous daemon left behind in `<state_dir>\launches`:
/// `credential\`, `request.json` and `git_config` of every launch, and the whole launch
/// directory when `<launch_dir>\pid` (`"<pid> <start_time>"`) no longer names a
/// live process according to `alive(pid, start_time)`.
pub fn sweep(state_dir: &Path, alive: &dyn Fn(u32, u64) -> bool) -> anyhow::Result<()> {
    let launches = state_dir.join("launches");
    let entries = match std::fs::read_dir(&launches) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", launches.display()));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "skipping an unreadable launch directory entry");
                continue;
            }
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let launch_id = entry.file_name().to_string_lossy().into_owned();
        let live = std::fs::read_to_string(dir.join("pid"))
            .ok()
            .and_then(|text| parse_pid_file(&text))
            .is_some_and(|(pid, start_time)| alive(pid, start_time));
        // Secrets go first either way, so a directory that cannot be removed
        // as a whole never keeps a token behind. The generated gitconfig is
        // one of them: it carries the launch's session pairing.
        for secret in [
            dir.join("credential"),
            dir.join("request.json"),
            dir.join(GIT_CONFIG_FILE),
        ] {
            let removed = if secret.is_dir() {
                std::fs::remove_dir_all(&secret)
            } else {
                std::fs::remove_file(&secret)
            };
            if let Err(error) = removed
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(launch_id, %error, path = %secret.display(), "failed to remove a launch secret");
            }
        }
        if live {
            tracing::info!(
                launch_id,
                "launch {launch_id} still running; the restart revoked its relay session"
            );
        } else if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(launch_id, %error, "failed to remove a stale launch directory");
        }
    }
    Ok(())
}

fn parse_pid_file(text: &str) -> Option<(u32, u64)> {
    let mut parts = text.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let start_time = parts.next()?.parse().ok()?;
    Some((pid, start_time))
}

// --- PR 4: the generated per-launch gitconfig (AC 6).

/// File name of the generated gitconfig in the launch directory. Same
/// lifecycle as the launch's other secrets: created before the box starts,
/// removed with the launch directory, and in the secrets-first `sweep` list
/// (it carries the session pairing; never log its content).
pub(crate) const GIT_CONFIG_FILE: &str = "git_config";

/// File name of the generated per-launch ssh client configuration.
pub(crate) const SSH_CONFIG_FILE: &str = "ssh_config";

/// File name of the per-launch `known_hosts` the generated `ssh_config`
/// points at.
pub(crate) const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// File name of git's per-launch ssh launcher script (`GIT_SSH`), beside the
/// `ssh_config` it hands to ssh.
pub(crate) const GIT_SSH_FILE: &str = "git_ssh";

/// The identity allowlist the generated gitconfig copies from the user's
/// global git configuration, and whether the key is multi-valued (read with
/// `--get-all`). Intentionally NOT copied: aliases, advanced core settings,
/// credential helpers.
pub(crate) const GIT_IDENTITY_KEYS: &[(&str, bool)] = &[
    ("user.name", false),
    ("user.email", false),
    ("user.signingkey", false),
    ("commit.gpgsign", false),
    ("core.longpaths", false),
    ("safe.directory", true),
];

/// Reads the identity allowlist host-side via `git config --global --get`
/// (`--get-all` for `safe.directory`), which honors includes. Absent keys and
/// a missing git are simply skipped: the box then commits without them.
fn read_git_identity() -> Vec<(String, Vec<String>)> {
    let mut identity = Vec::new();
    for (key, multi) in GIT_IDENTITY_KEYS {
        let get = if *multi { "--get-all" } else { "--get" };
        let Ok(output) = std::process::Command::new("git")
            .args(["config", "--global", get, key])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let values: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if !values.is_empty() {
            identity.push(((*key).to_owned(), values));
        }
    }
    identity
}

/// The authority (verbatim, port only when configured) and path of an allow
/// entry; the route shape mirrors the entry so the insteadOf rewrite is an
/// exact string prefix of the configured remote URL.
fn split_prefix_authority(entry: &str) -> Option<(&str, &str)> {
    let rest = entry
        .strip_prefix("https://")
        .or_else(|| entry.strip_prefix("http://"))?;
    Some(match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    })
}

/// A git config value in git's quoted form: `\` and `"` escaped, so `;`, `#`
/// and Windows paths survive git's value parser.
fn git_config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Renders the per-launch gitconfig (AC 6): insteadOf rewrites of every
/// allowed prefix to the loopback `/git/...` route, the session pairing as a
/// url-scoped `extraHeader` (sent only to the loopback relay), an empty
/// url-scoped `proxy` (so the loopback requests bypass any ambient proxy the
/// user's environment carries — the daemon itself sets no proxy variables),
/// an emptied `credential.helper`, the copied identity values, and ssh-form
/// insteadOf mappings of allowlisted hosts to the `/git-ssh` refusal route —
/// except hosts in `daemon.containment.git.ssh.hosts` (`ssh_hosts`), which
/// use their real remotes over `GIT_SSH` (D5).
pub(crate) fn gitconfig_contents(
    pairing: &str,
    proxy_port: u16,
    git_allow: &[String],
    identity: &[(String, Vec<String>)],
    ssh_hosts: &[String],
) -> String {
    let loopback = format!("http://127.0.0.1:{proxy_port}/");
    let mut contents = String::new();
    contents.push_str("# Generated by agentdesktop for one sandboxed launch; do not edit.\n");
    // Git for Windows' SYSTEM config sets credential.helper=manager and the
    // git install dir is granted, so the empty first entry is necessary.
    contents.push_str("[credential]\n\thelper =\n");
    contents.push_str(&format!(
        "[http \"{loopback}\"]\n\textraHeader = {}: {pairing}\n\tproxy =\n",
        crate::llm_proxy::PAIRING_HEADER
    ));
    let mut refused_hosts: Vec<&str> = Vec::new();
    for entry in git_allow {
        let Some((authority, path)) = split_prefix_authority(entry) else {
            continue;
        };
        contents.push_str(&format!(
            "[url \"{loopback}git/{authority}{path}\"]\n\tinsteadOf = {}\n",
            git_config_quote(entry),
        ));
        let host = authority.split(':').next().unwrap_or(authority);
        // D5: a host on the ssh allowlist loses its `/git-ssh` refusal
        // insteadOf; git then uses the real remote and `GIT_SSH`.
        if !host.is_empty()
            && !refused_hosts.contains(&host)
            && !ssh_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
        {
            refused_hosts.push(host);
        }
    }
    for host in refused_hosts {
        contents.push_str(&format!(
            "[url \"{loopback}git-ssh/{host}/\"]\n\tinsteadOf = ssh://{host}/\n\tinsteadOf = git@{host}:\n",
        ));
    }
    for (key, values) in identity {
        let Some((section, name)) = key.split_once('.') else {
            continue;
        };
        contents.push_str(&format!("[{section}]\n"));
        for value in values {
            if value.chars().any(char::is_control) {
                continue;
            }
            contents.push_str(&format!("\t{name} = {}\n", git_config_quote(value)));
        }
    }
    contents
}

/// Writes the per-launch gitconfig into the launch directory (mode 0600; the
/// box gets exactly the file granted read-only) and returns its path.
pub(crate) fn generate_gitconfig(
    launch_dir: &Path,
    pairing: &str,
    proxy_port: u16,
    git_allow: &[String],
    identity: &[(String, Vec<String>)],
    ssh_hosts: &[String],
) -> anyhow::Result<PathBuf> {
    let path = launch_dir.join(GIT_CONFIG_FILE);
    let contents = gitconfig_contents(pairing, proxy_port, git_allow, identity, ssh_hosts);
    secure_fs::atomic_write(&path, contents.as_bytes(), 0o600)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Renders the per-launch ssh client configuration (AC 6): one `Host` block
/// per allowlisted ssh host, pointing ssh at that host's loopback leg with
/// the host's real name as `HostKeyAlias`, the per-launch `known_hosts`, and
/// `accept-new` for hosts without a seeded entry. `IdentityAgent` is not
/// used; the agent pipe travels in `SSH_AUTH_SOCK`.
pub(crate) fn ssh_config_contents(launch_dir: &Path, legs: &[(String, u16)]) -> String {
    let known_hosts = format!("\"{}\\{KNOWN_HOSTS_FILE}\"", win(launch_dir));
    let mut contents =
        String::from("# Generated by agentdesktop for one sandboxed launch; do not edit.\n");
    for (host, port) in legs {
        contents.push_str(&format!(
            "Host {host}\n\tHostName 127.0.0.1\n\tPort {port}\n\tHostKeyAlias {host}\n\tUserKnownHostsFile {known_hosts}\n\tStrictHostKeyChecking accept-new\n\tIdentitiesOnly no\n"
        ));
    }
    contents
}

/// Writes the per-launch `ssh_config` (granted read-only, exactly the file),
/// `known_hosts` (granted read-write so `accept-new` persists within the
/// launch) and git's ssh launcher script (granted read-only, exactly the
/// file) into the launch directory, all 0600. None carries a secret; all die
/// with the launch directory.
///
/// The script is one `#!` line naming the agentdesktop executable
/// (`client_exe_name`, the file stem; it is on the box PATH). Git for Windows
/// runs a `GIT_SSH` script as `<interpreter>.exe <script> <ssh args>`, the
/// interpreter found by name on PATH and started without a shell; the
/// executable then runs ssh with this directory's `ssh_config`
/// ([`git_ssh::run_if_invoked`]). Git cuts the interpreter name at the first
/// space, so a name with whitespace is refused.
pub(crate) fn generate_ssh_files(
    launch_dir: &Path,
    legs: &[(String, u16)],
    seeds: &[String],
    client_exe_name: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !client_exe_name.is_empty() && !client_exe_name.contains(char::is_whitespace),
        "git over ssh needs an agentdesktop executable name without spaces (found {client_exe_name:?})"
    );
    anyhow::ensure!(
        client_exe_name.len() <= 90,
        "git over ssh needs an agentdesktop executable name that fits the #! line (found {client_exe_name:?}, {} bytes)",
        client_exe_name.len()
    );
    let config = launch_dir.join(SSH_CONFIG_FILE);
    secure_fs::atomic_write(
        &config,
        ssh_config_contents(launch_dir, legs).as_bytes(),
        0o600,
    )
    .with_context(|| format!("write {}", config.display()))?;
    let mut known = seeds.join("\n");
    if !known.is_empty() {
        known.push('\n');
    }
    let known_hosts = launch_dir.join(KNOWN_HOSTS_FILE);
    secure_fs::atomic_write(&known_hosts, known.as_bytes(), 0o600)
        .with_context(|| format!("write {}", known_hosts.display()))?;
    let script = launch_dir.join(GIT_SSH_FILE);
    secure_fs::atomic_write(&script, git_ssh_script(client_exe_name).as_bytes(), 0o600)
        .with_context(|| format!("write {}", script.display()))?;
    Ok(())
}

/// The contents of git's ssh launcher script: `#!/<client_exe_name>`. Git for
/// Windows keeps only the last path component of a `#!` line.
pub(crate) fn git_ssh_script(client_exe_name: &str) -> String {
    format!("#!/{client_exe_name}\n")
}

/// AC 4: the egress default is deny; an explicit `egress: allow` keeps
/// today's open outbound and is called out once per launch.
pub(crate) fn warn_on_open_egress(egress: agentdesktop_core::config::ContainmentEgress) {
    if egress == agentdesktop_core::config::ContainmentEgress::Allow {
        tracing::warn!("egress allow: the box has unrestricted outbound");
    }
}
