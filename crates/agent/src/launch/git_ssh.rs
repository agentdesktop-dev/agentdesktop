//! Git's ssh launcher inside the box: the agentdesktop executable started as
//! the `#!` interpreter of the per-launch `GIT_SSH` script.
//!
//! Git runs `GIT_SSH` without a shell but passes it no arguments of ours, and
//! Windows OpenSSH takes its client configuration only from `-F` or the
//! user's profile. Git for Windows starts a `GIT_SSH` script as
//! `<interpreter> <script> <git's ssh arguments>` with no shell in between;
//! this module turns that into
//! `%SystemRoot%\System32\OpenSSH\ssh.exe -F <script dir>\ssh_config <git's ssh arguments>`,
//! the arguments passed through unchanged.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use super::{GIT_SSH_FILE, SSH_CONFIG_FILE};

/// The ssh program and arguments for a launcher invocation, or `None` when
/// `args` (the full argv, program first) is not one: the first argument must
/// be an absolute Windows path whose file name is the launcher script's.
pub(crate) fn ssh_invocation(
    args: &[OsString],
    system_root: Option<&OsStr>,
) -> Option<(PathBuf, Vec<OsString>)> {
    let script = args.get(1)?.to_str()?;
    let (dir, name) = script.rsplit_once(['\\', '/'])?;
    if name != GIT_SSH_FILE || !is_absolute_windows(script) {
        return None;
    }
    let system_root = system_root
        .and_then(OsStr::to_str)
        .filter(|root| !root.is_empty())
        .unwrap_or(r"C:\Windows")
        .trim_end_matches('\\');
    let program = PathBuf::from(format!(r"{system_root}\System32\OpenSSH\ssh.exe"));
    let mut ssh_args = vec![
        OsString::from("-F"),
        OsString::from(format!(r"{dir}\{SSH_CONFIG_FILE}")),
    ];
    ssh_args.extend(args[2..].iter().cloned());
    Some((program, ssh_args))
}

/// `X:\...` or a UNC path (`\\server\...`).
fn is_absolute_windows(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(r"\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// Runs ssh when this process was started as git's ssh launcher and returns
/// its exit code (255, ssh's own error code, when ssh cannot start); `None`
/// otherwise, and always off Windows. Stdio is inherited: git talks to ssh
/// through it.
pub fn run_if_invoked() -> Option<i32> {
    if !cfg!(windows) {
        return None;
    }
    let args: Vec<OsString> = std::env::args_os().collect();
    let system_root = std::env::var_os("SystemRoot");
    let (program, ssh_args) = ssh_invocation(&args, system_root.as_deref())?;
    Some(
        match std::process::Command::new(&program)
            .args(&ssh_args)
            .status()
        {
            Ok(status) => status.code().unwrap_or(255),
            Err(error) => {
                eprintln!("agentdesktop: cannot start {}: {error}", program.display());
                255
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn git_ssh_launcher_runs_windows_openssh_with_the_launch_config_and_git_args_verbatim() {
        let script = r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef\git_ssh";
        // What Git for Windows passes for a protocol-v2 fetch: options, the
        // destination, and the remote command as one argument. Characters a
        // shell would act on stay as they are.
        let git_args = [
            "-o",
            "SendEnv=GIT_PROTOCOL",
            "git@github.com",
            "git-upload-pack 'org/re po&x%PATH%\"q'",
        ];
        let mut args = vec!["agentdesktop", script];
        args.extend(git_args);

        let (program, ssh_args) =
            ssh_invocation(&argv(&args), Some(OsStr::new(r"D:\Win"))).expect("a launcher call");

        assert_eq!(program, PathBuf::from(r"D:\Win\System32\OpenSSH\ssh.exe"));
        let mut expected = argv(&[
            "-F",
            r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef\ssh_config",
        ]);
        expected.extend(argv(&git_args));
        assert_eq!(ssh_args, expected);
    }

    #[test]
    fn git_ssh_launcher_defaults_system_root_and_keeps_a_unc_launch_dir() {
        let args = argv(&[
            "agentdesktop",
            r"\\server\share\launches\id\git_ssh",
            "git@github.com",
            "git-receive-pack 'org/repo.git'",
        ]);

        let (program, ssh_args) = ssh_invocation(&args, None).expect("a launcher call");

        assert_eq!(
            program,
            PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")
        );
        assert_eq!(
            ssh_args,
            argv(&[
                "-F",
                r"\\server\share\launches\id\ssh_config",
                "git@github.com",
                "git-receive-pack 'org/repo.git'",
            ])
        );
        assert_eq!(
            ssh_invocation(&args, Some(OsStr::new(""))).map(|(program, _)| program),
            Some(PathBuf::from(r"C:\Windows\System32\OpenSSH\ssh.exe")),
            "an empty SystemRoot falls back too"
        );
    }

    #[test]
    fn ordinary_command_lines_are_not_launcher_calls() {
        for args in [
            vec!["agentdesktop"],
            vec!["agentdesktop", "status"],
            vec!["agentdesktop", "git_ssh", "host"],
            vec!["agentdesktop", r"launches\id\git_ssh", "host"],
            vec!["agentdesktop", r"C:\launches\id\git_ssh.cmd", "host"],
            vec!["agentdesktop", r"C:\launches\id\ssh_config", "host"],
            vec!["agentdesktop", "--socket", r"C:\launches\id\git_ssh"],
        ] {
            assert_eq!(
                ssh_invocation(&argv(&args), None),
                None,
                "{args:?} is not a launcher call"
            );
        }
    }
}
