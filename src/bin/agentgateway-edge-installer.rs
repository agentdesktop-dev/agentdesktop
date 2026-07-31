use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Output};
use std::thread;
use std::time::{Duration, Instant};

use agentgateway_edge_connector::customization::read_customized_bootstrap;
use agentgateway_edge_connector::organization::OrganizationBootstrap;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

struct EmbeddedPayload {
    name: &'static str,
    sha256: &'static str,
    compressed: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded_payload.rs"));

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Install the self-contained Agent Gateway Edge bundle"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Install(InstallArgs),
}

#[derive(Debug, clap::Args)]
struct InstallArgs {
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long, help = "Use or create this Agent Gateway configuration")]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "Use this organization bootstrap (managed development builds)"
    )]
    organization: Option<PathBuf>,
    #[arg(long, help = "Accept the installation summary without prompting")]
    yes: bool,
    #[arg(long, help = "Install files without starting the user service")]
    no_start: bool,
    #[arg(
        long,
        help = "Connect supported AI agents without a separate confirmation"
    )]
    connect_agents: bool,
}

const SUPPORT_URL: &str = "https://github.com/solo-io/agent-desktop/issues/new";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nAgent Gateway Edge setup could not finish.");
            match write_support_report(&error) {
                Ok(path) => {
                    eprintln!("\nGet help:");
                    eprintln!("  1. Open {SUPPORT_URL}");
                    eprintln!("  2. Create an issue and attach this support report:");
                    eprintln!("     {}", path.display());
                }
                Err(report_error) => {
                    eprintln!("\nGet help at {SUPPORT_URL}");
                    eprintln!("The installer could not save a support report: {report_error:#}");
                    eprintln!("Installation error: {error:#}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    if !EMBEDDED {
        bail!(
            "this development build has no embedded payload; build it with scripts/build-embedded-installer.sh"
        );
    }
    let args = match Cli::parse().command {
        Some(Command::Install(args)) => args,
        None => InstallArgs {
            root: None,
            config: None,
            organization: None,
            yes: false,
            no_start: false,
            connect_agents: false,
        },
    };
    install(args)
}

fn install(args: InstallArgs) -> Result<()> {
    match INSTALLER_MODE {
        "standalone" => install_standalone(args),
        "managed" => install_managed(args),
        _ => bail!("embedded installer has an unsupported deployment mode"),
    }
}

fn install_standalone(args: InstallArgs) -> Result<()> {
    if args.organization.is_some() {
        bail!("--organization is only available for managed installers");
    }
    if args.no_start && args.connect_agents {
        bail!("--connect-agents requires the service to start");
    }
    let root = args.root.map_or_else(default_root, Ok)?;
    let command_link = default_command_link()?;
    if !root.is_absolute() {
        bail!("install root must be absolute");
    }
    let config = args.config.map_or_else(default_config, Ok)?;
    if !config.is_absolute() {
        bail!("Agent Gateway config path must be absolute");
    }
    print_summary(&root, &config, !args.no_start);
    if !args.yes && !confirm()? {
        println!("Installation cancelled; no files were changed.");
        return Ok(());
    }

    let parent = root.parent().context("install root has no parent")?;
    fs::create_dir_all(parent)?;
    let payload = tempfile::Builder::new()
        .prefix("agentgateway-edge-installer-")
        .tempdir_in(parent)
        .context("failed to create temporary extraction directory")?;
    extract_payload(payload.path())?;
    let config_created = seed_config(&payload.path().join("config"), &config)?;

    run_internal(
        payload.path(),
        [
            "install".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
            "--connector".to_owned(),
            payload
                .path()
                .join("connector")
                .to_string_lossy()
                .into_owned(),
            "--identity".to_owned(),
            payload
                .path()
                .join("identity")
                .to_string_lossy()
                .into_owned(),
            "--agentgateway".to_owned(),
            payload
                .path()
                .join("agentgateway")
                .to_string_lossy()
                .into_owned(),
            "--starter-config".to_owned(),
            payload.path().join("config").to_string_lossy().into_owned(),
            "--control".to_owned(),
            payload
                .path()
                .join("installer")
                .to_string_lossy()
                .into_owned(),
            "--gateway-config".to_owned(),
            config.to_string_lossy().into_owned(),
            "--command-link".to_owned(),
            command_link.to_string_lossy().into_owned(),
        ],
    )?;

    if !args.no_start {
        run_internal(
            payload.path(),
            [
                "service".to_owned(),
                "enable".to_owned(),
                "--root".to_owned(),
                root.to_string_lossy().into_owned(),
            ],
        )?;
        wait_for_health()?;
    }

    println!("\nInstallation complete");
    println!("  Files:   {}", root.display());
    println!(
        "  Config:  {} ({})",
        config.display(),
        if config_created {
            "created"
        } else {
            "preserved"
        }
    );
    println!(
        "  Service: {}",
        if args.no_start {
            "not started"
        } else {
            "enabled and started"
        }
    );
    if args.no_start {
        println!(
            "\nStart later with:\n  {} service enable --root {}",
            root.join("bin/agentgateway-edge-install").display(),
            root.display()
        );
    } else {
        println!("\nAgent Gateway Edge is ready.");
        if args.connect_agents {
            run_agent_setup(payload.path(), true)?;
        } else if args.yes {
            println!("AI agent settings were not changed.");
        } else {
            run_agent_setup(payload.path(), false)?;
        }
        println!("\nUpdate AI agent connections later with:\n  agentgateway-edge connect-agents");
        print_command_path_warning(&command_link);
    }
    Ok(())
}

fn install_managed(args: InstallArgs) -> Result<()> {
    if args.config.is_some() || args.no_start || args.connect_agents {
        bail!("managed installation does not accept standalone service or agent setup options");
    }
    let root = args.root.map_or_else(default_root, Ok)?;
    let command_link = default_command_link()?;
    if !root.is_absolute() {
        bail!("install root must be absolute");
    }
    let bootstrap = load_organization(args.organization.as_deref())?;
    print_managed_summary(&root, &bootstrap);
    if !args.yes && !confirm()? {
        println!("Installation cancelled; no files were changed.");
        return Ok(());
    }

    let parent = root.parent().context("install root has no parent")?;
    fs::create_dir_all(parent)?;
    let payload = tempfile::Builder::new()
        .prefix("agentgateway-edge-installer-")
        .tempdir_in(parent)
        .context("failed to create temporary extraction directory")?;
    extract_payload(payload.path())?;
    let organization = payload.path().join("organization.json");
    fs::write(&organization, serde_json::to_vec_pretty(&bootstrap)?)?;
    set_mode(&organization, 0o600)?;
    run_internal(
        payload.path(),
        [
            "managed-install".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
            "--connector".to_owned(),
            payload
                .path()
                .join("connector")
                .to_string_lossy()
                .into_owned(),
            "--identity".to_owned(),
            payload
                .path()
                .join("identity")
                .to_string_lossy()
                .into_owned(),
            "--organization".to_owned(),
            organization.to_string_lossy().into_owned(),
            "--control".to_owned(),
            payload
                .path()
                .join("installer")
                .to_string_lossy()
                .into_owned(),
            "--command-link".to_owned(),
            command_link.to_string_lossy().into_owned(),
        ],
    )?;

    println!("\nInstallation complete");
    println!("  Organization: {}", bootstrap.organization.display_name);
    println!("  Files:        {}", root.display());
    println!("  Service:      installed, awaiting user sign-in");
    println!("\nNo AI agent settings were changed.");
    println!("To sign in and connect your AI agents, run:\n  agentgateway-edge connect-agents");
    print_command_path_warning(&command_link);
    Ok(())
}

fn default_command_link() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/bin/agentgateway-edge"))
}

fn print_command_path_warning(command_link: &Path) {
    let Some(directory) = command_link.parent() else {
        return;
    };
    let on_path = env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|entry| entry == directory));
    if !on_path {
        println!(
            "\nThis terminal does not include {} in PATH. The command will be available after your user environment includes that directory.",
            directory.display()
        );
    }
}

fn load_organization(path: Option<&Path>) -> Result<OrganizationBootstrap> {
    if let Some(path) = path {
        return OrganizationBootstrap::parse(&fs::read(path).with_context(|| {
            format!("organization bootstrap {} is unavailable", path.display())
        })?);
    }
    read_customized_bootstrap(&env::current_exe()?)?
        .context("this generic managed installer requires --organization <file>")
}

fn print_managed_summary(root: &Path, bootstrap: &OrganizationBootstrap) {
    println!(
        "Agent Gateway Edge for {}\n",
        bootstrap.organization.display_name
    );
    println!("This installs for the current user:");
    println!("  - Edge connector");
    println!("  - Identity helper");
    println!("\nLocation:     {}", root.display());
    println!("Agent Gateway: {}", bootstrap.gateway.url);
    println!("Service:      installed but not started");
    println!("\nSign-in and AI agent settings are left to the user.");
}

fn wait_for_health() -> Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], 8080));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if health_check(address) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("the installed service did not become ready")
}

fn write_support_report(error: &anyhow::Error) -> Result<PathBuf> {
    let path = support_report_path()?;
    let diagnostics = [
        (
            "Service status",
            command_output(
                "systemctl",
                &[
                    "--user",
                    "status",
                    "--no-pager",
                    "agentgateway-edge.service",
                ],
            ),
        ),
        (
            "Recent service log",
            command_output(
                "journalctl",
                &[
                    "--user-unit",
                    "agentgateway-edge.service",
                    "--no-pager",
                    "--lines=200",
                ],
            ),
        ),
    ];
    write_support_report_to(&path, error, &diagnostics)?;
    Ok(path)
}

fn support_report_path() -> Result<PathBuf> {
    let state = if let Some(state_home) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state_home)
    } else {
        let home = env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".local/state")
    };
    Ok(state.join("agent-desktop/install-support.txt"))
}

fn command_output(command: &str, args: &[&str]) -> String {
    match ProcessCommand::new(command).args(args).output() {
        Ok(output) => format_output(output),
        Err(error) => format!("Could not collect this information: {error}"),
    }
}

fn format_output(output: Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if text.trim().is_empty() {
        text = format!(
            "Command exited with {} and produced no output.",
            output.status
        );
    }
    text
}

fn write_support_report_to(
    path: &Path,
    error: &anyhow::Error,
    diagnostics: &[(&str, String)],
) -> Result<()> {
    fs::create_dir_all(path.parent().context("support report path has no parent")?)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut report = options
        .open(path)
        .with_context(|| format!("failed to create support report {}", path.display()))?;
    set_mode(path, 0o600)?;
    writeln!(report, "Agent Gateway Edge installer support report")?;
    writeln!(report, "Installer version: {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(report, "\nInstallation error:\n{error:#}")?;
    for (heading, content) in diagnostics {
        writeln!(report, "\n{heading}:\n{content}")?;
    }
    Ok(())
}

fn health_check(address: SocketAddr) -> bool {
    let result = (|| -> io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        stream.write_all(
            b"GET /_agentgateway/healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.1 200") && response.contains(r#""status":"ok""#))
    })();
    result.unwrap_or(false)
}

fn default_root() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set; pass --root explicitly")?;
    Ok(PathBuf::from(home).join(".local/lib/agentgateway-edge"))
}

fn default_config() -> Result<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join("agentgateway/config.yaml"));
    }
    let home = env::var_os("HOME").context("HOME is not set; pass --config explicitly")?;
    Ok(PathBuf::from(home).join(".config/agentgateway/config.yaml"))
}

fn print_summary(root: &Path, config: &Path, start: bool) {
    println!("Agent Gateway Edge\n");
    println!("This installs for the current user:");
    println!("  - Agent Gateway");
    println!("  - Edge connector");
    println!("  - Identity helper");
    println!("\nLocation: {}", root.display());
    println!("Config:   {}", config.display());
    println!(
        "Service:  {}",
        if start {
            "start now and at login"
        } else {
            "do not start"
        }
    );
    println!("Network:  connector listens on loopback; review Agent Gateway config");
    println!("\nThe connector does not store provider credentials.");
}

fn seed_config(starter: &Path, destination: &Path) -> Result<bool> {
    if destination.exists() {
        let metadata = fs::symlink_metadata(destination)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!(
                "existing Agent Gateway config {} is not a regular file",
                destination.display()
            );
        }
        return Ok(false);
    }
    fs::create_dir_all(
        destination
            .parent()
            .context("Agent Gateway config has no parent")?,
    )?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(destination).with_context(|| {
        format!(
            "failed to create Agent Gateway config {}",
            destination.display()
        )
    })?;
    file.write_all(&fs::read(starter)?)?;
    Ok(true)
}

fn confirm() -> Result<bool> {
    print!("\nContinue? [Y/n] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    let bytes_read = io::stdin().read_line(&mut answer)?;
    Ok(bytes_read > 0
        && matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "" | "y" | "yes"
        ))
}

fn extract_payload(directory: &Path) -> Result<()> {
    for payload in PAYLOADS {
        let decoded = zstd::stream::decode_all(Cursor::new(payload.compressed))
            .with_context(|| format!("embedded {} payload is corrupt", payload.name))?;
        let digest = hex_sha256(&decoded);
        if digest != payload.sha256 {
            bail!(
                "embedded {} payload failed integrity verification",
                payload.name
            );
        }
        let path = directory.join(payload.name);
        fs::write(&path, decoded)?;
        set_mode(
            &path,
            if payload.name == "config" {
                0o600
            } else {
                0o700
            },
        )?;
    }
    Ok(())
}

fn run_internal<const N: usize>(directory: &Path, args: [String; N]) -> Result<()> {
    let status = ProcessCommand::new(directory.join("installer"))
        .args(args)
        .status()
        .context("failed to run the embedded installation engine")?;
    if !status.success() {
        bail!("installation engine failed with {status}");
    }
    Ok(())
}

fn run_agent_setup(directory: &Path, automatic: bool) -> Result<()> {
    let mut command = ProcessCommand::new(directory.join("connector"));
    command.arg("connect-agents");
    if automatic {
        command.arg("--yes");
    }
    let status = command
        .status()
        .context("failed to configure supported AI agents")?;
    if !status.success() {
        bail!("supported AI agent configuration failed with {status}");
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_connection_requires_an_explicit_install_flag() {
        let Cli {
            command: Some(Command::Install(defaults)),
        } = Cli::try_parse_from(["installer", "install", "--yes"]).unwrap()
        else {
            panic!("install command was not parsed");
        };
        assert!(defaults.yes);
        assert!(!defaults.connect_agents);

        let Cli {
            command: Some(Command::Install(automatic)),
        } = Cli::try_parse_from(["installer", "install", "--yes", "--connect-agents"]).unwrap()
        else {
            panic!("install command was not parsed");
        };
        assert!(automatic.connect_agents);
    }

    #[test]
    fn support_report_contains_failure_and_diagnostics() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("install-support.txt");
        fs::write(&path, "old report").unwrap();

        write_support_report_to(
            &path,
            &anyhow::anyhow!("service did not become ready"),
            &[("Service status", "service failed".to_owned())],
        )
        .unwrap();

        let report = fs::read_to_string(&path).unwrap();
        assert!(report.contains("service did not become ready"));
        assert!(report.contains("Service status:\nservice failed"));
        assert!(!report.contains("old report"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
