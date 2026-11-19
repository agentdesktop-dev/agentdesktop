//! Same-terminal console hosting (Windows).
//!
//! `run --wait` started from a console asks the daemon to run the boxed
//! program on that console. The daemon validates the requesting pid (same
//! user, same session) and starts this crate's hidden `console-attach`
//! subcommand, which attaches to the client's console and spawns the
//! executor with the console's handles inherited. The boxed program then
//! runs in the caller's terminal and `cmd /c` (no `start`) returns the
//! program's own exit code.

/// The `console-attach` helper's exit code when its own setup fails (attach
/// refused, job or spawn failure) before the executor runs. The daemon does
/// not key on this value — a program may legitimately exit with it — it
/// distinguishes a real setup failure from the program's exit by the
/// `ATTACH_STARTED_MARKER`, written only once the executor has been spawned.
pub(crate) const ATTACH_FAILED_EXIT: i32 = 125;

/// Marker file the helper writes into the launch directory once the executor
/// is spawned and in the job. Its presence means a subsequent exit is the
/// program's own result, not an attach-setup failure.
pub(crate) const ATTACH_STARTED_MARKER: &str = "attach-started";

#[cfg(windows)]
pub(crate) use imp::*;

#[cfg(not(windows))]
pub(crate) fn client_console_available() -> bool {
    false
}

#[cfg(not(windows))]
pub(crate) fn ignore_ctrl_c() {}

#[cfg(not(windows))]
pub(crate) fn run_attach(
    _client_pid: u32,
    _executor: &std::path::Path,
    _request: &std::path::Path,
    _log: &std::path::Path,
) -> anyhow::Result<i32> {
    anyhow::bail!("same-terminal hosting is only supported on Windows")
}

#[cfg(windows)]
mod imp {
    use std::{
        ffi::c_void,
        io::{self, Write},
        path::Path,
        process::{Child, Command, Stdio},
    };

    use anyhow::{Context, bail};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
        Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, SECURITY_ATTRIBUTES,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            Console::{
                AttachConsole, CTRL_C_EVENT, FreeConsole, GetConsoleMode, GetStdHandle,
                STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{
                CREATE_SUSPENDED, CreateProcessW, DETACHED_PROCESS, GetExitCodeProcess, INFINITE,
                OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread,
                STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
            },
        },
    };

    use super::super::host::peer_identity;

    /// `TRUE` for Ctrl+C, `FALSE` for everything else (close, logoff, break).
    unsafe extern "system" fn ignore_ctrl_c_handler(event: u32) -> windows_sys::core::BOOL {
        if event == CTRL_C_EVENT { 1 } else { 0 }
    }

    /// Whether the calling process owns a console on stdin and stdout
    /// (Windows Terminal / ConPTY included): only then does `run --wait`
    /// ask for same-terminal hosting.
    pub(crate) fn client_console_available() -> bool {
        // SAFETY: plain console queries on the process's own handles.
        unsafe {
            let mut mode = 0u32;
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            !input.is_null()
                && input != INVALID_HANDLE_VALUE
                && GetConsoleMode(input, &mut mode) != 0
                && !output.is_null()
                && output != INVALID_HANDLE_VALUE
                && GetConsoleMode(output, &mut mode) != 0
        }
    }

    /// Ignore Ctrl+C in this process so the attached box receives it and the
    /// waiter (client or helper) stays alive.
    pub(crate) fn ignore_ctrl_c() {
        // SAFETY: the handler is 'static and returns per the documented contract.
        unsafe {
            SetConsoleCtrlHandler(Some(ignore_ctrl_c_handler), 1);
        }
    }

    fn session_of(pid: u32) -> Option<u32> {
        let mut session = 0u32;
        // SAFETY: valid out-parameter.
        let ok = unsafe { ProcessIdToSessionId(pid, &mut session) };
        (ok != 0 && session != 0).then_some(session)
    }

    /// The integrity-level RID of process `pid`'s token (e.g. 0x2000 medium,
    /// 0x3000 high). `AttachConsole` from the medium-integrity daemon is
    /// refused against a higher-integrity client (an elevated terminal), so
    /// the daemon compares levels and downgrades such a client to windowed
    /// rather than failing the launch after the helper's attach is denied
    /// (lab AC-1: the first run's terminals were elevated).
    fn integrity_level(pid: u32) -> anyhow::Result<u32> {
        // SAFETY: plain system calls; every handle is checked and closed.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("open process {pid}"));
            }
            let mut token: HANDLE = std::ptr::null_mut();
            let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
            CloseHandle(process);
            if opened == 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("open process {pid}'s token"));
            }
            let mut size = 0u32;
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
            if size == 0 {
                CloseHandle(token);
                bail!("integrity-level size query failed for process {pid}");
            }
            let mut buffer = vec![0u8; size as usize];
            let ok = GetTokenInformation(
                token,
                TokenIntegrityLevel,
                buffer.as_mut_ptr().cast::<c_void>(),
                size,
                &mut size,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("read process {pid}'s integrity level"));
            }
            let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let sid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                bail!("integrity SID of process {pid} has no sub-authority");
            }
            Ok(*GetSidSubAuthority(sid, (count - 1) as u32))
        }
    }

    /// Whether `client_pid` may host the box's console: alive, in the daemon's
    /// session, of the daemon's own user, and no higher integrity than the
    /// daemon (an elevated terminal is refused). Any failure is downgraded to
    /// a windowed launch by the caller.
    pub(crate) fn validate_client(client_pid: u32) -> anyhow::Result<()> {
        if client_pid == std::process::id() {
            bail!("the daemon cannot attach to its own console");
        }
        let ours =
            session_of(std::process::id()).context("the daemon's session could not be read")?;
        let theirs = session_of(client_pid)
            .with_context(|| format!("process {client_pid} is not running in a session"))?;
        if ours != theirs {
            bail!("process {client_pid} runs in another session");
        }
        let daemon = peer_identity(std::process::id()).context("read the daemon's identity")?;
        let client = peer_identity(client_pid)
            .with_context(|| format!("read process {client_pid}'s identity"))?;
        if daemon.user_sid.is_none() || daemon.user_sid != client.user_sid {
            bail!("process {client_pid} runs as another user");
        }
        let our_integrity =
            integrity_level(std::process::id()).context("read the daemon's integrity level")?;
        let client_integrity = integrity_level(client_pid)?;
        if client_integrity > our_integrity {
            bail!(
                "process {client_pid} runs at a higher integrity level \
                 (0x{client_integrity:x} > 0x{our_integrity:x}); AttachConsole would be refused"
            );
        }
        Ok(())
    }

    fn wide(value: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        value.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn append_log(log: &Path, line: &str) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(file, "console-attach: {line}");
        }
    }

    /// The hidden `console-attach` subcommand: attach to `client_pid`'s
    /// console, run the executor there, and return the executor's exit code
    /// (which `cmd /c` makes the boxed program's own code).
    pub(crate) fn run_attach(
        client_pid: u32,
        executor: &Path,
        request: &Path,
        log: &Path,
    ) -> anyhow::Result<i32> {
        ignore_ctrl_c();
        // AttachConsole refuses (ERROR_ACCESS_DENIED) while the caller holds
        // any console, including the windowless one CREATE_NO_WINDOW allocates
        // (lab AC-2, first run). The helper is spawned console-free
        // (DETACHED_PROCESS), and this guards any other way of starting it.
        // SAFETY: plain system call; failure (no console held) is fine.
        unsafe { FreeConsole() };
        // SAFETY: plain system call.
        if unsafe { AttachConsole(client_pid) } == 0 {
            let error = io::Error::last_os_error();
            append_log(
                log,
                &format!("attach to process {client_pid} failed: {error}"),
            );
            return Err(error).with_context(|| format!("attach to process {client_pid}'s console"));
        }
        // FreeConsole and AttachConsole reset the process's console-control
        // handler table, so re-install the ignore-Ctrl+C handler now that we
        // hold the client's console; otherwise a CTRL_C_EVENT would reach the
        // default handler and the kill-on-close job would take the box down
        // instead of letting the program handle the interrupt (Copilot review).
        ignore_ctrl_c();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: the name is a static UTF-16 literal; the attributes are valid
        // for the call's duration.
        let (input, output) = unsafe {
            (
                CreateFileW(
                    wide(Path::new("CONIN$")).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &attributes,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                ),
                CreateFileW(
                    wide(Path::new("CONOUT$")).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &attributes,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                ),
            )
        };
        if input == INVALID_HANDLE_VALUE || output == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            append_log(log, &format!("open the attached console: {error}"));
            return Err(error).context("open the attached console");
        }
        // SAFETY: no attributes and no name.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            let error = io::Error::last_os_error();
            append_log(log, &format!("create the launch job: {error}"));
            return Err(error).context("create the launch job");
        }
        // Killing or losing the helper kills the executor and the box with it.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the info struct is valid for the documented size.
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            // Fail closed: without kill-on-close the job cannot clean up the box
            // (Copilot review: a silently unconfigured job disables cleanup).
            let error = io::Error::last_os_error();
            append_log(log, &format!("configure the launch job: {error}"));
            // SAFETY: valid handles owned here.
            unsafe {
                CloseHandle(job);
                CloseHandle(input);
                CloseHandle(output);
            }
            return Err(error).context("configure the launch job");
        }
        let mut command_line: Vec<u16> =
            format!("\"{}\" \"{}\"", executor.display(), request.display())
                .encode_utf16()
                .chain(Some(0))
                .collect();
        let application = wide(executor);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = input;
        startup.hStdOutput = output;
        startup.hStdError = output;
        let mut process: windows_sys::Win32::System::Threading::PROCESS_INFORMATION =
            unsafe { std::mem::zeroed() };
        // SAFETY: all pointers are valid for the call's duration and the
        // handles are inheritable.
        let started = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut process,
            )
        };
        if started == 0 {
            let error = io::Error::last_os_error();
            append_log(log, &format!("start the executor: {error}"));
            return Err(error).context("start the executor");
        }
        // Assign to the job before resuming, so no descendant can be created in
        // the window between spawn and assignment and survive the kill-on-close
        // cleanup (Copilot review). The executor was created suspended.
        // SAFETY: the process/thread handles are valid until closed below.
        let assigned = unsafe { AssignProcessToJobObject(job, process.hProcess) };
        if assigned == 0 {
            let error = io::Error::last_os_error();
            append_log(
                log,
                &format!("assign the executor to the launch job: {error}"),
            );
            // SAFETY: valid handles owned here; terminate the still-suspended executor.
            unsafe {
                TerminateProcess(process.hProcess, 1);
                CloseHandle(process.hProcess);
                CloseHandle(process.hThread);
                CloseHandle(input);
                CloseHandle(output);
                CloseHandle(job);
            }
            return Err(error).context("assign the executor to the launch job");
        }
        // The executor is spawned and in the job: from here any exit is the
        // program's own result, so mark the launch started before resuming.
        // The daemon reads this marker instead of overloading an exit code
        // (Copilot review: exit 125 is a valid program result too).
        if let Some(dir) = log.parent()
            && let Err(error) = std::fs::write(dir.join(super::ATTACH_STARTED_MARKER), [])
        {
            append_log(log, &format!("write the attach-started marker: {error}"));
        }
        // SAFETY: valid thread/process handles; resume now that the job is in place.
        unsafe {
            ResumeThread(process.hThread);
            WaitForSingleObject(process.hProcess, INFINITE);
        }
        let mut code = 1u32;
        // SAFETY: valid process handle and out-parameter.
        unsafe {
            GetExitCodeProcess(process.hProcess, &mut code);
            CloseHandle(process.hProcess);
            CloseHandle(process.hThread);
            CloseHandle(input);
            CloseHandle(output);
            CloseHandle(job);
        }
        Ok(code as i32)
    }

    /// Starts the hidden `console-attach` subcommand for a launch. The
    /// daemon keeps a normal executor child (pid + start time) and kills it
    /// through the helper; the helper's job kills the executor and the box.
    pub(crate) fn spawn_helper(
        client_pid: u32,
        executor: &Path,
        request: &Path,
        log: &Path,
    ) -> io::Result<Child> {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("console-attach")
            .arg("--client-pid")
            .arg(client_pid.to_string())
            .arg("--executor")
            .arg(executor)
            .arg("--request")
            .arg(request)
            .arg("--log")
            .arg(log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Console-free, not CREATE_NO_WINDOW: AttachConsole only works
            // from a process without a console, and CREATE_NO_WINDOW would
            // allocate a windowless one (lab AC-2, first run).
            .creation_flags(DETACHED_PROCESS);
        command.spawn()
    }
}
