//! Production implementations of the launch traits: the daemon's own
//! environment and filesystem, `wxc-exec.exe` as a child process, the gateway
//! credential through the local API's own path, and the Windows identity calls.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use agentdesktop_core::model::LlmGatewayCredential;
use async_trait::async_trait;
use axum::http::StatusCode;

use super::{
    CredentialSource, CwdContext, CwdFs, Executor, LaunchChild, LaunchHost, PathKind, ProgramFs,
    program_preconditions,
};
use crate::api;

/// Directories a workspace may never be under, by environment variable.
const SYSTEM_DIRS: [&str; 4] = [
    "SystemRoot",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramData",
];

/// The daemon's user, as the process environment describes it.
pub struct HostEnvironment {
    home: PathBuf,
    state_dir: PathBuf,
    client_exe_dir: PathBuf,
    client_exe_name: String,
}

impl HostEnvironment {
    /// `client_executable` is the agentdesktop executable the box may run:
    /// its directory goes on the box PATH, its file stem names the
    /// interpreter of git's ssh launcher script.
    pub fn new(home: PathBuf, state_dir: PathBuf, client_executable: &Path) -> Self {
        Self {
            home,
            state_dir,
            client_exe_dir: client_executable
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            client_exe_name: client_executable
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

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

static REAL_FS: RealFs = RealFs;

impl LaunchHost for HostEnvironment {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default()
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        super::profile::SHARED
            .base_env_keys
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect()
    }

    fn cwd_context(&self) -> CwdContext {
        CwdContext {
            home: self.home.clone(),
            state_dir: self.state_dir.clone(),
            system_dirs: SYSTEM_DIRS
                .iter()
                .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
                .collect(),
            // Directories under the home the box must never see; the
            // workspace may not be under them either. These are the profile's
            // top-level denied secrets (the `.claude` entries are guarded by
            // the workspace rules separately).
            denied: super::profile::SHARED
                .denied_secrets
                .iter()
                .filter(|entry| !entry.contains('\\'))
                .map(|name| self.home.join(name))
                .collect(),
            app_data: ["APPDATA", "LOCALAPPDATA"]
                .iter()
                .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
                .collect(),
        }
    }

    fn cwd_fs(&self) -> &dyn CwdFs {
        &REAL_FS
    }

    fn program_fs(&self) -> &dyn ProgramFs {
        &REAL_FS
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }

    /// The Git root above the first `git.exe` on the daemon's PATH, when that
    /// directory is one of Git for Windows' launcher directories.
    fn git_dir(&self) -> Option<PathBuf> {
        self.path_dirs().into_iter().find_map(|dir| {
            if !dir.join("git.exe").is_file() {
                return None;
            }
            let name = dir.file_name()?.to_str()?.to_ascii_lowercase();
            let parent = dir.parent()?;
            match name.as_str() {
                "bin"
                    if parent
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("mingw64")) =>
                {
                    parent.parent().map(Path::to_path_buf)
                }
                "cmd" | "bin" => Some(parent.to_path_buf()),
                _ => None,
            }
        })
    }

    fn program_files_claude(&self) -> Option<PathBuf> {
        let dir = PathBuf::from(std::env::var_os("ProgramFiles")?).join("ClaudeCode");
        dir.is_dir().then_some(dir)
    }

    fn client_exe_dir(&self) -> PathBuf {
        self.client_exe_dir.clone()
    }

    fn client_exe_name(&self) -> String {
        self.client_exe_name.clone()
    }

    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default()
    }

    fn managed_sources(&self) -> crate::provider::claude_code::managed::ManagedSources {
        crate::provider::claude_code::managed::managed_sources_now()
    }
}

/// Runs `wxc-exec.exe <request.json>` with its output appended to the launch's
/// log file; no console window of its own.
pub struct ProcessExecutor;

struct ProcessChild {
    child: Child,
    pid: u32,
    start_time: u64,
}

impl LaunchChild for ProcessChild {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| status.code().unwrap_or(-1)))
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn start_time(&self) -> u64 {
        self.start_time
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

impl Executor for ProcessExecutor {
    fn spawn(
        &self,
        _launch_id: &str,
        executor: &Path,
        request: &Path,
        log: &Path,
    ) -> io::Result<Box<dyn LaunchChild>> {
        // The daemon opens the log file itself (it inherits the launch
        // directory's private DACL); the executor only inherits the handle, so
        // a daemon stop later never breaks the box's output.
        let stdout = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let stderr = stdout.try_clone()?;
        let mut command = Command::new(executor);
        command
            .arg(request)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
            // The child gets its own stdio; the daemon's own standard handles
            // (a log redirect under a scheduled task) must not travel with it
            // into the box (lab F4: the box kept the daemon log open).
            windows::stop_inheriting_standard_handles();
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        // Without its creation time the sweep could not tell this box from a
        // reused pid later; better not to start it at all.
        let start_time = match process_start_time(&child) {
            Ok(start_time) => start_time,
            Err(error) => {
                let _ = child.kill();
                return Err(io::Error::other(format!(
                    "read process start time: {error:#}"
                )));
            }
        };
        Ok(Box::new(ProcessChild {
            child,
            pid,
            start_time,
        }))
    }
}

#[cfg(windows)]
fn process_start_time(child: &Child) -> anyhow::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    windows::creation_time(child.as_raw_handle() as _)
}

#[cfg(not(windows))]
fn process_start_time(_child: &Child) -> anyhow::Result<u64> {
    Ok(0)
}

/// The gateway credential for a launch: the local API's own path, with the
/// subscription never included. The pre-flight is interactive by design.
pub struct GatewayCredentialSource {
    pub app: api::AppState,
}

#[async_trait]
impl CredentialSource for GatewayCredentialSource {
    async fn credential(
        &self,
        client_id: &str,
        interactive: bool,
    ) -> Result<LlmGatewayCredential, (StatusCode, String)> {
        let effective =
            api::load_effective_config(&self.app.config, &self.app.state_dir).map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("read applied configuration: {error:#}"),
                )
            })?;
        // The configuration is re-checked for the launching program (its
        // client ID); the egress rule was applied by the launch handler
        // against the startup setting, so it is not repeated here.
        program_preconditions(
            &effective,
            client_id,
            agentdesktop_core::config::ContainmentEgress::Deny,
        )
        .map_err(|message| (StatusCode::CONFLICT, message))?;
        let effective = if interactive {
            effective
        } else {
            api::non_interactive(effective)
        };
        // `include_subscription` stays false whatever the configuration says:
        // no subscription token enters the box.
        let app = self.app.clone();
        let client_id = client_id.to_owned();
        tokio::spawn(
            async move { api::gateway_credential(&app, &effective, &client_id, false).await },
        )
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("join gateway credential task: {error:#}"),
            )
        })?
    }
}

/// `HKLM\SOFTWARE\Policies\GitHubCopilot`'s subkey path (also read under
/// `HKCU`).
#[cfg(windows)]
const COPILOT_POLICY_KEY: &str = r"SOFTWARE\Policies\GitHubCopilot";

/// Copilot CLI's local managed-policy documents, each parsed: the machine
/// and per-user policy keys (see [`registry_policy_document`]) and
/// `%ProgramFiles%\GitHubCopilot\managed-settings.json`. An absent source is
/// skipped; an unreadable or invalid file is logged and skipped (Copilot
/// would not apply it either).
#[cfg(windows)]
pub(crate) fn copilot_policies() -> Vec<serde_json::Value> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let mut policies = Vec::new();
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let document = registry_policy_document(
            [
                (COPILOT_POLICY_KEY.to_owned(), None),
                (format!(r"{COPILOT_POLICY_KEY}\sandbox"), Some("sandbox")),
            ]
            .into_iter()
            .flat_map(|(subkey, prefix)| {
                windows::policy_values(hive, &subkey)
                    .into_iter()
                    .map(move |(name, value)| (prefix, name, value))
            }),
        );
        if !document.is_empty() {
            policies.push(serde_json::Value::Object(document));
        }
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let path = PathBuf::from(program_files)
            .join("GitHubCopilot")
            .join("managed-settings.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(document) => policies.push(document),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "Copilot managed settings are not valid JSON; ignored by the launch pre-flight");
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Copilot managed settings unreadable; ignored by the launch pre-flight");
            }
        }
    }
    policies
}

#[cfg(not(windows))]
pub(crate) fn copilot_policies() -> Vec<serde_json::Value> {
    Vec::new()
}

/// One hive's policy-key values as a document: a value named `a.b` lands at
/// `a.b`, a value of the `sandbox` subkey (`prefix`) under `sandbox`.
/// Registry value names are case-insensitive, so names are lowercased
/// (`Sandbox.Enabled` reads as `sandbox.enabled`); the JSON policy file keeps
/// its case.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn registry_policy_document(
    values: impl IntoIterator<Item = (Option<&'static str>, String, serde_json::Value)>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut document = serde_json::Map::new();
    for (prefix, name, value) in values {
        let name = match prefix {
            Some(prefix) => format!("{prefix}.{name}"),
            None => name,
        };
        insert_dotted(&mut document, &name.to_lowercase(), value);
    }
    document
}

/// Inserts `value` at the dotted `name` (`sandbox.enabled`) of `document`.
#[cfg_attr(not(windows), allow(dead_code))]
fn insert_dotted(
    document: &mut serde_json::Map<String, serde_json::Value>,
    name: &str,
    value: serde_json::Value,
) {
    match name.split_once('.') {
        Some((head, rest)) if !head.is_empty() && !rest.is_empty() => {
            let child = document
                .entry(head.to_owned())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !child.is_object() {
                *child = serde_json::Value::Object(serde_json::Map::new());
            }
            if let Some(child) = child.as_object_mut() {
                insert_dotted(child, rest, value);
            }
        }
        _ => {
            document.insert(name.to_owned(), value);
        }
    }
}

/// The PE product version of `executable` from its version resource
/// (`VS_FIXEDFILEINFO`), formatted `a.b.c.d`; the file is read, never run.
#[cfg(windows)]
pub(crate) fn product_version(executable: &Path) -> Option<String> {
    windows::product_version(executable)
}

#[cfg(not(windows))]
pub(crate) fn product_version(_executable: &Path) -> Option<String> {
    None
}

/// Whether the process `pid` with creation time `start_time` is still alive:
/// the pid alone is not an identity, Windows reuses it.
#[cfg(windows)]
pub fn process_alive(pid: u32, start_time: u64) -> bool {
    windows::process_alive(pid, start_time)
}

#[cfg(windows)]
pub use windows::peer_identity;

#[cfg(windows)]
pub(crate) mod windows {
    use std::ffi::c_void;

    use anyhow::{Context, bail};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
            STILL_ACTIVE, SetHandleInformation,
        },
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation,
            TOKEN_APPCONTAINER_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenAppContainerSid,
            TokenIsAppContainer, TokenUser,
        },
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
    };

    use super::super::PipePeer;
    use super::super::wrap::{ProcessFact, WrapEntry};

    /// Clears the inherit flag on the daemon's standard handles, so a child
    /// started with explicit stdio cannot receive them.
    pub(super) fn stop_inheriting_standard_handles() {
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain system calls; a missing handle is skipped.
            unsafe {
                let handle = GetStdHandle(which);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0 {
                    tracing::debug!("SetHandleInformation failed for handle {which:?}");
                }
            }
        }
    }

    /// A handle closed on drop.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by an Open* call and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open_process(pid: u32) -> anyhow::Result<Handle> {
        // SAFETY: plain system call with valid arguments.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open process {pid}"));
        }
        Ok(Handle(handle))
    }

    fn filetime(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// The creation FILETIME of an open process handle.
    pub(crate) fn creation_time(handle: HANDLE) -> anyhow::Result<u64> {
        let empty = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (empty, empty, empty, empty);
        // SAFETY: the out-parameters are valid for writes for the call's duration.
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("GetProcessTimes");
        }
        Ok(filetime(creation))
    }

    pub(super) fn process_alive(pid: u32, start_time: u64) -> bool {
        let Ok(process) = open_process(pid) else {
            return false;
        };
        if creation_time(process.0).ok() != Some(start_time) {
            return false;
        }
        let mut code = 0u32;
        // SAFETY: valid handle and out-parameter.
        let ok = unsafe { GetExitCodeProcess(process.0, &mut code) };
        ok != 0 && code == STILL_ACTIVE as u32
    }

    /// The string form of a SID pointer (`ConvertSidToStringSidW`).
    ///
    /// # Safety
    /// `sid` must point at a valid SID that outlives the call.
    unsafe fn sid_string(sid: *mut c_void) -> anyhow::Result<String> {
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: the caller guarantees `sid`; `text` is a valid out-parameter.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("convert SID to string");
        }
        // SAFETY: a null-terminated UTF-16 string allocated by the system.
        Ok(unsafe {
            let mut len = 0usize;
            while *text.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast::<c_void>());
            sid
        })
    }

    /// The process token of `pid`, opened for query.
    fn open_process_token(pid: u32) -> anyhow::Result<Handle> {
        let process = open_process(pid)?;
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: valid process handle and out-parameter.
        if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("open process token");
        }
        Ok(Handle(token))
    }

    /// Whether a token reports `TokenIsAppContainer`.
    fn token_is_app_container(token: &Handle) -> anyhow::Result<bool> {
        let mut app_container = 0u32;
        let mut returned = 0u32;
        // SAFETY: a u32 out-parameter of the documented size.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenIsAppContainer,
                (&mut app_container as *mut u32).cast::<c_void>(),
                std::mem::size_of::<u32>() as u32,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("read token AppContainer flag");
        }
        Ok(app_container != 0)
    }

    /// Reads the identity of process `pid` (`TokenUser`, `TokenIsAppContainer`).
    pub fn peer_identity(pid: u32) -> anyhow::Result<PipePeer> {
        let token = open_process_token(pid)?;

        let mut length = 0u32;
        // SAFETY: a size query; the null buffer is allowed with length 0.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut length);
        }
        if length == 0 {
            bail!("TokenUser size query failed");
        }
        let mut buffer = vec![0u8; length as usize];
        // SAFETY: the buffer is `length` bytes, as the size query asked for.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                length,
                &mut length,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("read token user");
        }
        // SAFETY: GetTokenInformation(TokenUser) fills a TOKEN_USER at the buffer
        // start; the SID it names points into `buffer`, which outlives the call.
        let user_sid = unsafe { sid_string((*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid)? };

        Ok(PipePeer {
            user_sid: Some(user_sid),
            app_container: token_is_app_container(&token)?,
        })
    }

    /// The AppContainer SID of process `pid`'s token, `None` for a process
    /// outside an AppContainer (AC 3: the pipe DACL and the pipe-client
    /// check are scoped to the box's per-launch SID).
    pub(crate) fn app_container_sid(pid: u32) -> anyhow::Result<Option<String>> {
        let token = open_process_token(pid)?;
        if !token_is_app_container(&token)? {
            return Ok(None);
        }
        let mut length = 0u32;
        // SAFETY: a size query; the null buffer is allowed with length 0.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenAppContainerSid,
                std::ptr::null_mut(),
                0,
                &mut length,
            );
        }
        if length == 0 {
            bail!("TokenAppContainerSid size query failed");
        }
        let mut buffer = vec![0u8; length as usize];
        // SAFETY: the buffer is `length` bytes, as the size query asked for.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenAppContainerSid,
                buffer.as_mut_ptr().cast::<c_void>(),
                length,
                &mut length,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("read token AppContainer SID");
        }
        // SAFETY: GetTokenInformation(TokenAppContainerSid) fills a
        // TOKEN_APPCONTAINER_INFORMATION at the buffer start; the SID points
        // into `buffer`, which outlives the call.
        let sid = unsafe {
            sid_string(
                (*buffer.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer,
            )?
        };
        Ok(Some(sid))
    }

    /// The AppContainer SID of the first AppContainer descendant of `root`
    /// (AC 3, needs-lab-check L1): one process snapshot, depth-first
    /// over the parent-pid tree; descendants whose token cannot be read are
    /// skipped. `None` when no descendant is in an AppContainer yet.
    /// A candidate created before `root_start_time` (the executor's creation
    /// FILETIME) is never a descendant: the snapshot's parent pid is not
    /// updated when a parent exits, so after pid reuse an older process can
    /// name the executor's pid.
    pub(crate) fn app_container_descendant_sid(
        root: u32,
        root_start_time: u64,
    ) -> anyhow::Result<Option<String>> {
        // SAFETY: plain system call.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("snapshot the process list");
        }
        let snapshot = Handle(snapshot);
        // SAFETY: PROCESSENTRY32W is plain data; dwSize is set as documented.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut processes: Vec<(u32, u32)> = Vec::new();
        // SAFETY: valid snapshot handle and entry buffer.
        if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
            loop {
                processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
                // SAFETY: valid snapshot handle and entry buffer.
                if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                    break;
                }
            }
        }
        let mut frontier = vec![root];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(parent) = frontier.pop() {
            for (pid, parent_pid) in &processes {
                if *parent_pid != parent || *pid == root || !seen.insert(*pid) {
                    continue;
                }
                let created = open_process(*pid).and_then(|process| creation_time(process.0));
                if !created.is_ok_and(|created| created >= root_start_time) {
                    continue;
                }
                if let Ok(Some(sid)) = app_container_sid(*pid) {
                    return Ok(Some(sid));
                }
                frontier.push(*pid);
            }
        }
        Ok(None)
    }

    /// One fact per running process whose Toolhelp image name matches a wrap
    /// entry's executable file name (prefilter; the detector matches the full
    /// canonical image path). A process that cannot be opened is skipped (it
    /// cannot be the daemon user's); one whose token cannot be read is
    /// reported with `user_sid: None` and never counts as a violation.
    pub(crate) fn wrap_process_snapshot(wrap: &[WrapEntry]) -> Result<Vec<ProcessFact>, String> {
        let names: Vec<String> = wrap
            .iter()
            .filter_map(|entry| {
                let text = entry.executable.to_string_lossy().replace('/', "\\");
                text.rsplit('\\').next().map(str::to_lowercase)
            })
            .collect();
        // SAFETY: plain system call.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "snapshot the process list: {}",
                std::io::Error::last_os_error()
            ));
        }
        let snapshot = Handle(snapshot);
        // SAFETY: PROCESSENTRY32W is plain data; dwSize is set as documented.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut facts = Vec::new();
        // SAFETY: valid snapshot handle and entry buffer.
        if unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0 {
            loop {
                let exe_file: Vec<u16> = entry
                    .szExeFile
                    .iter()
                    .copied()
                    .take_while(|unit| *unit != 0)
                    .collect();
                let exe_file = String::from_utf16_lossy(&exe_file).to_lowercase();
                if names.contains(&exe_file)
                    && let Some(fact) = wrap_process_fact(entry.th32ProcessID)
                {
                    facts.push(fact);
                }
                // SAFETY: valid snapshot handle and entry buffer.
                if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
                    break;
                }
            }
        }
        Ok(facts)
    }

    /// The scanner's facts about one process: full image path, creation time,
    /// and token identity. `None` when the process cannot be opened or
    /// queried (it exited, or belongs to a user the daemon cannot read).
    fn wrap_process_fact(pid: u32) -> Option<ProcessFact> {
        let process = open_process(pid).ok()?;
        let start_time = creation_time(process.0).ok()?;
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: the buffer is valid for `length` UTF-16 units.
        let ok =
            unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) };
        if ok == 0 {
            return None;
        }
        let image = std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
        let (app_container, user_sid) = match peer_identity(pid) {
            Ok(peer) => (peer.app_container, peer.user_sid),
            // Fail closed: an unreadable identity never counts.
            Err(_) => (false, None),
        };
        Some(ProcessFact {
            pid,
            start_time,
            image,
            app_container,
            user_sid,
        })
    }

    /// The values of the registry key `subkey` under `hive`: `REG_SZ` and
    /// `REG_EXPAND_SZ` as strings, `REG_DWORD` as numbers; other types are
    /// skipped. A missing or unreadable key yields nothing.
    pub(crate) fn policy_values(
        hive: windows_sys::Win32::System::Registry::HKEY,
        subkey: &str,
    ) -> Vec<(String, serde_json::Value)> {
        use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
        use windows_sys::Win32::System::Registry::{
            HKEY, KEY_READ, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumValueW,
            RegOpenKeyExW,
        };

        let path: Vec<u16> = OsStr::new(subkey)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `path` is null-terminated; `key` receives the handle.
        if unsafe { RegOpenKeyExW(hive, path.as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut values = Vec::new();
        let mut data = vec![0u8; 1024];
        let mut index = 0u32;
        loop {
            // Value names are at most 16,383 characters.
            let mut name = vec![0u16; 16_384];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut data_len = data.len() as u32;
            // SAFETY: every buffer is valid for the length passed with it.
            let status = unsafe {
                RegEnumValueW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if status == ERROR_MORE_DATA {
                data.resize(data_len as usize, 0);
                continue;
            }
            if status != ERROR_SUCCESS {
                break;
            }
            index += 1;
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            let bytes = &data[..data_len as usize];
            let value = match kind {
                REG_SZ | REG_EXPAND_SZ => {
                    let wide: Vec<u16> = bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair))
                        .take_while(|unit| *unit != 0)
                        .collect();
                    serde_json::Value::String(String::from_utf16_lossy(&wide))
                }
                REG_DWORD if bytes.len() >= 4 => serde_json::Value::from(u32::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ])),
                _ => continue,
            };
            values.push((name, value));
        }
        // SAFETY: `key` was opened above.
        unsafe { RegCloseKey(key) };
        values
    }

    /// The `VS_FIXEDFILEINFO` product version of `executable`, `a.b.c.d`.
    pub(crate) fn product_version(executable: &std::path::Path) -> Option<String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
        };

        let path: Vec<u16> = executable
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut handle = 0u32;
        // SAFETY: `path` is null-terminated.
        let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut handle) };
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        // SAFETY: `block` holds `size` bytes.
        if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, block.as_mut_ptr().cast()) } == 0 {
            return None;
        }
        let root = [u16::from(b'\\'), 0];
        let mut info: *mut c_void = std::ptr::null_mut();
        let mut info_len = 0u32;
        // SAFETY: `block` is the version block filled above; `info` points
        // into it on success.
        if unsafe {
            VerQueryValueW(
                block.as_ptr().cast(),
                root.as_ptr(),
                &mut info,
                &mut info_len,
            )
        } == 0
            || info.is_null()
            || (info_len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        // SAFETY: `info` points at a VS_FIXEDFILEINFO inside `block`, which
        // outlives this read; the read is unaligned-safe.
        let fixed = unsafe { std::ptr::read_unaligned(info.cast::<VS_FIXEDFILEINFO>()) };
        if fixed.dwSignature != 0xFEEF_04BD {
            return None;
        }
        Some(format!(
            "{}.{}.{}.{}",
            fixed.dwProductVersionMS >> 16,
            fixed.dwProductVersionMS & 0xffff,
            fixed.dwProductVersionLS >> 16,
            fixed.dwProductVersionLS & 0xffff
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_executable_name_and_directory_from_the_path() {
        let env = HostEnvironment::new(
            PathBuf::from("/home/user"),
            PathBuf::from("/var/lib/agentdesktop"),
            Path::new("/opt/x/agentdesktop-windows-x64.exe"),
        );
        assert_eq!(env.client_exe_name(), "agentdesktop-windows-x64");
        assert_eq!(env.client_exe_dir(), PathBuf::from("/opt/x"));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn kill_stops_the_process_and_returns_ok_when_already_exited() {
        let child = Command::new("ping")
            .args(["-n", "300", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let pid = child.id();
        let mut process_child = ProcessChild {
            child,
            pid,
            start_time: 0,
        };

        process_child.kill().expect("kill the process");

        let mut exit_code = None;
        for _ in 0..100 {
            if let Ok(Some(code)) = process_child.try_wait() {
                exit_code = Some(code);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(exit_code, Some(1), "process exited with code 1");

        let result = process_child.kill();
        assert!(result.is_ok(), "second kill returns Ok");
    }
}
