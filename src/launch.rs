use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::apps::claude::{CONNECTOR_BASE_URL, PLACEHOLDER_CREDENTIAL};

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
};
const CLAUDE_PROFILE: Profile = Profile {
    name: "claude",
    environment: &[
        EnvironmentVariable {
            name: "ANTHROPIC_BASE_URL",
            value: CONNECTOR_BASE_URL,
        },
        EnvironmentVariable {
            name: "ANTHROPIC_API_KEY",
            value: PLACEHOLDER_CREDENTIAL,
        },
    ],
    preflight: Some(Preflight {
        address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080)),
        request:
            b"GET /_agentdesktop/healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    }),
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

pub fn run(args: LaunchArgs) -> Result<ExitStatus> {
    run_with_systemd(&args, Path::new("systemd-run"))
}

fn run_with_systemd(args: &LaunchArgs, systemd_run: &Path) -> Result<ExitStatus> {
    let profile = resolve_profile(&args.profile)?;
    if !args.skip_preflight
        && let Some(preflight) = &profile.preflight
    {
        check_preflight(profile.name, preflight)?;
    }
    let (program, arguments) = args
        .command
        .split_first()
        .context("launch requires a command")?;
    let unit = scope_name()?;
    let description = format!("Agent Desktop {} execution scope", profile.name);

    let mut command = Command::new(systemd_run);
    command
        .args(["--user", "--scope", "--collect", "--quiet", "--unit"])
        .arg(unit)
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
        .arg(program)
        .args(arguments)
        .status()
        .with_context(|| {
            format!(
                "failed to create the Linux execution scope with {}",
                systemd_run.display()
            )
        })
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
    use std::thread;

    #[test]
    fn preserves_command_arguments_and_exit_status() {
        let temporary = tempfile::tempdir().unwrap();
        let runner = temporary.path().join("systemd-run");
        let output = temporary.path().join("arguments");
        fs::write(
            &runner,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 23\n",
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();

        let status = run_with_systemd(
            &LaunchArgs {
                profile: "claude".to_owned(),
                skip_preflight: true,
                command: vec![
                    OsString::from("/opt/Claude Code/claude"),
                    OsString::from("--continue"),
                    OsString::from("argument with spaces"),
                ],
            },
            &runner,
        )
        .unwrap();

        assert_eq!(status.code(), Some(23));
        let arguments = fs::read_to_string(output).unwrap();
        assert!(arguments.contains("--user\n--scope\n--collect\n--quiet\n"));
        assert!(arguments.contains("KillMode=control-group\n"));
        assert!(arguments.contains("Description=Agent Desktop claude execution scope\n"));
        assert!(arguments.contains("--setenv\nANTHROPIC_BASE_URL=http://127.0.0.1:8080\n"));
        assert!(arguments.contains("--setenv\nANTHROPIC_API_KEY=local-gateway-placeholder\n"));
        assert!(
            arguments.contains("--\n/opt/Claude Code/claude\n--continue\nargument with spaces\n")
        );
    }

    #[test]
    fn resolves_only_embedded_profiles() {
        assert_eq!(resolve_profile("custom").unwrap().environment, &[]);
        assert_eq!(resolve_profile("claude").unwrap().environment.len(), 2);
        let error = resolve_profile("unknown").unwrap_err().to_string();
        assert!(error.contains("available profiles: custom, claude"));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = check_preflight(
            "claude",
            &Preflight {
                address,
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
