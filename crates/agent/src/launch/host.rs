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

use agentdesktop_core::{
    config::{DaemonConfig, LlmGatewayAuthentication},
    model::LlmGatewayCredential,
};
use async_trait::async_trait;
use axum::http::StatusCode;

use super::{
    CredentialSource, CwdContext, CwdFs, Executor, LaunchChild, LaunchHost, PathKind, ProgramFs,
    preconditions,
};
use crate::api;

/// Environment variables copied from the daemon into the box.
const BASE_ENV: [&str; 13] = [
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

/// Directories a workspace may never be under, by environment variable.
const SYSTEM_DIRS: [&str; 4] = [
    "SystemRoot",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramData",
];

/// Directories under the home the box must never see; the workspace may not
/// be under them either.
const DENIED_HOME_DIRS: [&str; 4] = [".ssh", ".aws", ".azure", ".kube"];

/// The daemon's user, as the process environment describes it.
pub struct HostEnvironment {
    home: PathBuf,
    state_dir: PathBuf,
    client_exe_dir: PathBuf,
}

impl HostEnvironment {
    pub fn new(home: PathBuf, state_dir: PathBuf, client_exe_dir: PathBuf) -> Self {
        Self {
            home,
            state_dir,
            client_exe_dir,
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
        BASE_ENV
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
            denied: DENIED_HOME_DIRS
                .iter()
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
/// subscription never included and no browser sign-in for a refresh.
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
        preconditions(&effective).map_err(|message| (StatusCode::CONFLICT, message))?;
        let effective = if interactive {
            effective
        } else {
            non_interactive(effective)
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

/// With the device grant selected, a missing or unrefreshable OIDC token is a
/// `401` instead of a browser sign-in: a background refresh never prompts.
fn non_interactive(mut effective: DaemonConfig) -> DaemonConfig {
    if let Some(gateway) = effective.llm_gateway.as_mut()
        && let Some(LlmGatewayAuthentication::Oidc {
            device_authorization,
            ..
        }) = gateway.authentication.as_mut()
    {
        *device_authorization = true;
    }
    effective
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
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenIsAppContainer, TokenUser,
        },
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    use super::super::PipePeer;

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

    /// Reads the identity of process `pid` (`TokenUser`, `TokenIsAppContainer`).
    pub fn peer_identity(pid: u32) -> anyhow::Result<PipePeer> {
        let process = open_process(pid)?;
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: valid process handle and out-parameter.
        if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("open process token");
        }
        let token = Handle(token);

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
        // SAFETY: GetTokenInformation(TokenUser) fills a TOKEN_USER at the buffer start.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` points into `buffer`, which outlives the call.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("convert SID to string");
        }
        // SAFETY: a null-terminated UTF-16 string allocated by the system.
        let user_sid = unsafe {
            let mut len = 0usize;
            while *text.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast::<c_void>());
            sid
        };

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
        Ok(PipePeer {
            user_sid: Some(user_sid),
            app_container: app_container != 0,
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
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
