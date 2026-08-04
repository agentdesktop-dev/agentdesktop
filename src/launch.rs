use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

#[derive(Debug, Eq, PartialEq)]
struct EnvironmentVariable {
    name: &'static str,
    value: &'static str,
}

#[derive(Debug, Eq, PartialEq)]
struct Profile {
    name: &'static str,
    environment: &'static [EnvironmentVariable],
    preflight: Option<Preflight>,
    transparent_capture: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct Preflight {
    address: SocketAddr,
    request: &'static [u8],
}

const CUSTOM_PROFILE: Profile = Profile {
    name: "custom",
    environment: &[],
    preflight: None,
    transparent_capture: false,
};
const CLAUDE_PROFILE: Profile = Profile {
    name: "claude",
    environment: &[],
    preflight: Some(Preflight {
        address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080)),
        request:
            b"GET /_agentdesktop/healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    }),
    transparent_capture: true,
};
const PROFILES: &[Profile] = &[CUSTOM_PROFILE, CLAUDE_PROFILE];

#[derive(Debug, Args)]
pub struct LaunchArgs {
    /// Application profile associated with this execution scope.
    #[arg(long, default_value = "custom")]
    profile: String,

    /// Skip profile readiness checks before launching.
    #[arg(long)]
    skip_preflight: bool,

    /// Command and arguments to run in the execution scope.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct LaunchChildArgs {
    #[arg(long)]
    gate_directory: PathBuf,

    #[arg(long)]
    controller_pid: u32,

    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

pub fn run(args: LaunchArgs) -> Result<ExitStatus> {
    let executable = std::env::current_exe().context("resolve the Agent Desktop executable")?;
    let capture_helper = Path::new("/usr/libexec/agentdesktop-capture-setup");
    run_with_systemd(
        &args,
        Path::new("systemd-run"),
        Path::new("systemctl"),
        Path::new("/sys/fs/cgroup"),
        &executable,
        |cgroup| {
            verify_inspection_trust()?;
            TcpStream::connect_timeout(
                &SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 15001)),
                Duration::from_secs(1),
            )
            .context("transparent-capture relay is unavailable")?;
            run_capture_helper(capture_helper, "install", Some(cgroup))
        },
        |cgroup| run_capture_helper(capture_helper, "remove", Some(cgroup)),
    )
}

fn verify_inspection_trust() -> Result<()> {
    let config_root = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")
                .map(|home| home.join(".config"))
        },
        |root| Ok(PathBuf::from(root)),
    )?;
    let certificate = config_root.join("agentgateway/inspection-ca/ca.crt");
    let contents = std::fs::read(&certificate)
        .with_context(|| format!("read inspection CA certificate {}", certificate.display()))?;
    let fingerprint = Sha256::digest(&contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let anchor = Path::new("/etc/pki/ca-trust/source/anchors")
        .join(format!("agentdesktop-{fingerprint}.pem"));
    if std::fs::read(&anchor).ok().as_deref() != Some(contents.as_slice()) {
        bail!("local inspection trust is missing or changed; run `agentdesktop trust install`");
    }
    Ok(())
}

fn run_capture_helper(helper: &Path, action: &str, cgroup: Option<&Path>) -> Result<()> {
    if !helper.is_file() {
        bail!(
            "transparent-capture helper {} is not installed",
            helper.display()
        );
    }
    let mut command = Command::new("pkexec");
    command.arg(helper).arg(action);
    if let Some(cgroup) = cgroup {
        command.arg("--cgroup").arg(cgroup);
    }
    let status = command
        .status()
        .with_context(|| format!("authorize transparent-capture {action}"))?;
    if !status.success() {
        bail!("transparent-capture {action} failed with {status}");
    }
    Ok(())
}

pub fn run_child(args: LaunchChildArgs) -> Result<()> {
    let ready = args.gate_directory.join("ready");
    let release = args.gate_directory.join("release");
    create_gate_file(&ready)?;
    while !release.is_file() {
        if !Path::new("/proc")
            .join(args.controller_pid.to_string())
            .is_dir()
        {
            bail!("launch controller exited before releasing the application");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (program, arguments) = args
        .command
        .split_first()
        .context("launch child requires a command")?;
    Err(Command::new(program).args(arguments).exec())
        .with_context(|| format!("failed to execute {}", program.to_string_lossy()))
}

fn run_with_systemd(
    args: &LaunchArgs,
    systemd_run: &Path,
    systemctl: &Path,
    cgroup_root: &Path,
    executable: &Path,
    prepare_capture: impl FnOnce(&Path) -> Result<()>,
    cleanup_capture: impl FnOnce(&Path) -> Result<()>,
) -> Result<ExitStatus> {
    let profile = resolve_profile(&args.profile)?;
    if !args.skip_preflight
        && let Some(preflight) = &profile.preflight
    {
        check_preflight(profile.name, preflight)?;
    }
    if profile.transparent_capture {
        crate::apps::claude::ensure_capture_routing_is_clear()?;
    }
    let (program, arguments) = args
        .command
        .split_first()
        .context("launch requires a command")?;
    let unit = scope_name()?;
    let description = format!("Agent Desktop {} execution scope", profile.name);
    let gate = tempfile::Builder::new()
        .prefix("agentdesktop-launch-")
        .tempdir()
        .context("create the application launch gate")?;

    let mut command = Command::new(systemd_run);
    command
        .args(["--user", "--scope", "--collect", "--quiet", "--unit"])
        .arg(&unit)
        .arg("--property")
        .arg("KillMode=control-group")
        .arg("--property")
        .arg(format!("Description={description}"));
    for variable in profile.environment {
        command
            .arg("--setenv")
            .arg(format!("{}={}", variable.name, variable.value));
    }
    command
        .arg("--")
        .arg(executable)
        .arg("_launch-child")
        .arg("--gate-directory")
        .arg(gate.path())
        .arg("--controller-pid")
        .arg(std::process::id().to_string())
        .arg("--")
        .arg(program)
        .args(arguments)
        .spawn()
        .with_context(|| {
            format!(
                "failed to create the Linux execution scope with {}",
                systemd_run.display()
            )
        })
        .and_then(|mut child| {
            wait_for_gate(&mut child, &gate.path().join("ready"))?;
            let cgroup = scope_control_group(systemctl, cgroup_root, &unit)
                .inspect_err(|_| terminate_child(&mut child))?;
            if profile.transparent_capture {
                prepare_capture(&cgroup).inspect_err(|_| terminate_child(&mut child))?;
            }
            let result = create_gate_file(&gate.path().join("release"))
                .inspect_err(|_| terminate_child(&mut child))
                .and_then(|()| child.wait().context("wait for the Linux execution scope"));
            if profile.transparent_capture {
                let cleanup = cleanup_capture(&cgroup);
                match (result, cleanup) {
                    (Ok(status), Ok(())) => Ok(status),
                    (Err(error), _) => Err(error),
                    (Ok(_), Err(error)) => {
                        Err(error.context("application exited but capture cleanup failed"))
                    }
                }
            } else {
                result
            }
        })
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn scope_control_group(systemctl: &Path, cgroup_root: &Path, unit: &str) -> Result<PathBuf> {
    let output = Command::new(systemctl)
        .args(["--user", "show", "--property", "ControlGroup", "--value"])
        .arg(unit)
        .output()
        .with_context(|| format!("inspect application scope {unit}"))?;
    if !output.status.success() {
        bail!(
            "could not inspect application scope {unit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let cgroup = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    if !cgroup.is_absolute() || cgroup == Path::new("/") {
        bail!(
            "application scope {unit} returned invalid cgroup {}",
            cgroup.display()
        );
    }
    let host_path = cgroup_root.join(
        cgroup
            .strip_prefix("/")
            .expect("absolute cgroup has a root prefix"),
    );
    if !host_path.is_dir() {
        bail!(
            "application scope {unit} cgroup {} does not exist",
            cgroup.display()
        );
    }
    Ok(cgroup)
}

fn create_gate_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create launch gate file {}", path.display()))?;
    Ok(())
}

fn wait_for_gate(child: &mut Child, ready: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if ready.is_file() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("application scope exited before its launch gate was ready: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("application scope did not reach its launch gate");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check_preflight(profile: &str, preflight: &Preflight) -> Result<()> {
    let mut stream = connect_preflight(preflight.address)
        .with_context(|| preflight_error(profile, "Agent Desktop is not running"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .with_context(|| preflight_error(profile, "could not check Agent Desktop readiness"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .with_context(|| preflight_error(profile, "could not check Agent Desktop readiness"))?;
    stream
        .write_all(preflight.request)
        .with_context(|| preflight_error(profile, "could not check Agent Desktop readiness"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .with_context(|| preflight_error(profile, "could not check Agent Desktop readiness"))?;
    if response.starts_with("HTTP/1.1 200") && response.contains(r#""status":"ok""#) {
        return Ok(());
    }
    if response.starts_with("HTTP/1.1 503") {
        bail!(
            "{}",
            preflight_error(
                profile,
                "Agent Desktop is running, but Agent Gateway is unavailable"
            )
        );
    }
    bail!(
        "{}",
        preflight_error(profile, "Agent Desktop returned an unhealthy response")
    )
}

fn connect_preflight(address: SocketAddr) -> std::io::Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error),
        }
    }
}

fn preflight_error(profile: &str, reason: &str) -> String {
    format!(
        "{reason}; start it with `systemctl --user start agentdesktop.service`, then retry. If Agent Desktop is not installed, run the installer. To bypass this check for debugging, use `agentdesktop launch --skip-preflight --profile {profile} -- ...`"
    )
}

fn resolve_profile(name: &str) -> Result<&'static Profile> {
    PROFILES
        .iter()
        .find(|profile| profile.name == name)
        .with_context(|| {
            let available = PROFILES
                .iter()
                .map(|profile| profile.name)
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown launch profile '{name}'; available profiles: {available}")
        })
}

fn scope_name() -> Result<String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).context("failed to generate an execution scope ID")?;
    let identifier = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "agentdesktop-launch-{}-{identifier}.scope",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::{LaunchArgs, Preflight, check_preflight, resolve_profile, run_with_systemd};
    use std::ffi::OsString;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::thread;

    #[test]
    fn preserves_command_arguments_and_exit_status() {
        let temporary = tempfile::tempdir().unwrap();
        let runner = temporary.path().join("systemd-run");
        let systemctl = temporary.path().join("systemctl");
        let launcher = temporary.path().join("agentdesktop");
        let cgroup_root = temporary.path().join("cgroup");
        let systemd_arguments = temporary.path().join("systemd-arguments");
        let command_arguments = temporary.path().join("command-arguments");
        fs::write(
            &runner,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
                systemd_arguments.display()
            ),
        )
        .unwrap();
        fs::create_dir_all(cgroup_root.join("user.slice/user-1000.slice/app.slice/test.scope"))
            .unwrap();
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nshift\nwhile [ \"$1\" != -- ]; do if [ \"$1\" = --gate-directory ]; then shift; gate=$1; fi; shift; done\nshift\n: > \"$gate/ready\"\nwhile [ ! -f \"$gate/release\" ]; do :; done\nprintf '%s\\n' \"$@\" > '{}'\nexit 23\n",
                command_arguments.display()
            ),
        )
        .unwrap();
        fs::write(
            &systemctl,
            "#!/bin/sh\nprintf '/user.slice/user-1000.slice/app.slice/test.scope\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o700)).unwrap();

        let status = run_with_systemd(
            &LaunchArgs {
                profile: "custom".to_owned(),
                skip_preflight: true,
                command: vec![
                    OsString::from("/opt/Claude Code/claude"),
                    OsString::from("--continue"),
                    OsString::from("argument with spaces"),
                ],
            },
            &runner,
            &systemctl,
            &cgroup_root,
            &launcher,
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(status.code(), Some(23));
        let arguments = fs::read_to_string(systemd_arguments).unwrap();
        assert!(arguments.contains("--user\n--scope\n--collect\n--quiet\n"));
        assert!(arguments.contains("KillMode=control-group\n"));
        assert!(arguments.contains("Description=Agent Desktop custom execution scope\n"));
        assert!(!arguments.contains("--setenv\n"));
        let arguments = fs::read_to_string(command_arguments).unwrap();
        assert_eq!(
            arguments,
            "/opt/Claude Code/claude\n--continue\nargument with spaces\n"
        );
    }

    #[test]
    fn resolves_only_embedded_profiles() {
        assert_eq!(resolve_profile("custom").unwrap().environment, &[]);
        assert_eq!(resolve_profile("claude").unwrap().environment, &[]);
        assert!(resolve_profile("claude").unwrap().transparent_capture);
        let error = resolve_profile("unknown").unwrap_err().to_string();
        assert!(error.contains("available profiles: custom, claude"));
    }

    #[test]
    fn captured_profile_prepares_exact_cgroup_before_starting_the_application() {
        let temporary = tempfile::tempdir().unwrap();
        let runner = temporary.path().join("systemd-run");
        let systemctl = temporary.path().join("systemctl");
        let launcher = temporary.path().join("agentdesktop");
        let cgroup_root = temporary.path().join("cgroup");
        let marker = temporary.path().join("executed");
        fs::write(
            &runner,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        )
        .unwrap();
        fs::create_dir_all(cgroup_root.join("user.slice/claude.scope")).unwrap();
        fs::write(
            &systemctl,
            "#!/bin/sh\nprintf '/user.slice/claude.scope\\n'\n",
        )
        .unwrap();
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nshift\nwhile [ \"$1\" != -- ]; do if [ \"$1\" = --gate-directory ]; then shift; gate=$1; fi; shift; done\nshift\n: > \"$gate/ready\"\nwhile [ ! -f \"$gate/release\" ]; do :; done\n: > '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        for executable in [&runner, &systemctl, &launcher] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let mut prepared_cgroup = None;
        let error = run_with_systemd(
            &LaunchArgs {
                profile: "claude".to_owned(),
                skip_preflight: true,
                command: vec![OsString::from("claude")],
            },
            &runner,
            &systemctl,
            &cgroup_root,
            &launcher,
            |cgroup| {
                prepared_cgroup = Some(cgroup.to_owned());
                anyhow::bail!("capture preparation failed")
            },
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("capture preparation failed"));
        assert_eq!(
            prepared_cgroup,
            Some(Path::new("/user.slice/claude.scope").to_owned())
        );
        assert!(!marker.exists());
    }

    #[test]
    fn invalid_cgroup_terminates_scope_without_releasing_target() {
        let temporary = tempfile::tempdir().unwrap();
        let runner = temporary.path().join("systemd-run");
        let systemctl = temporary.path().join("systemctl");
        let launcher = temporary.path().join("agentdesktop");
        let marker = temporary.path().join("executed");
        fs::write(
            &runner,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        )
        .unwrap();
        fs::write(&systemctl, "#!/bin/sh\nprintf '/missing.scope\\n'\n").unwrap();
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nshift\nwhile [ \"$1\" != -- ]; do if [ \"$1\" = --gate-directory ]; then shift; gate=$1; fi; shift; done\nshift\n: > \"$gate/ready\"\nwhile [ ! -f \"$gate/release\" ]; do :; done\n: > '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        for executable in [&runner, &systemctl, &launcher] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let error = run_with_systemd(
            &LaunchArgs {
                profile: "custom".to_owned(),
                skip_preflight: true,
                command: vec![OsString::from("ignored")],
            },
            &runner,
            &systemctl,
            &temporary.path().join("cgroup"),
            &launcher,
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("cgroup /missing.scope does not exist"));
        assert!(!marker.exists());
    }

    #[test]
    fn custom_profile_does_not_require_a_preflight() {
        assert!(resolve_profile("custom").unwrap().preflight.is_none());
        assert!(resolve_profile("claude").unwrap().preflight.is_some());
    }

    #[test]
    fn accepts_a_healthy_profile_service() {
        let (preflight, server) =
            preflight_response("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}");
        check_preflight("claude", &preflight).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn reports_an_unavailable_gateway_with_recovery_steps() {
        let (preflight, server) = preflight_response(
            "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n{\"status\":\"degraded\"}",
        );
        let error = check_preflight("claude", &preflight)
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(error.contains("Agent Gateway is unavailable"));
        assert!(error.contains("systemctl --user start agentdesktop.service"));
        assert!(error.contains("--skip-preflight"));
    }

    #[test]
    fn reports_an_absent_service_with_recovery_steps() {
        let error = check_preflight(
            "claude",
            &Preflight {
                address: "127.0.0.1:0".parse().unwrap(),
                request: b"GET / HTTP/1.1\r\n\r\n",
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Agent Desktop is not running"));
        assert!(error.contains("run the installer"));
        assert!(error.contains("--skip-preflight"));
    }

    fn preflight_response(response: &'static str) -> (Preflight, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            assert!(
                request[..bytes_read]
                    .windows(4)
                    .any(|bytes| bytes == b"\r\n\r\n")
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (
            Preflight {
                address,
                request: b"GET /_agentdesktop/healthz HTTP/1.1\r\n\r\n",
            },
            server,
        )
    }
}
