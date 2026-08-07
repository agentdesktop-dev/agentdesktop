use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Output};
use std::thread;
use std::time::{Duration, Instant};

use agentdesktop::customization::read_customized_bootstrap;
use agentdesktop::organization::OrganizationBootstrap;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use sha2::{Digest, Sha256};

struct EmbeddedPayload {
    name: &'static str,
    sha256: &'static str,
    compressed: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded_payload.rs"));

#[derive(Debug, Parser)]
#[command(version, about = "Install the self-contained Agent Desktop bundle")]
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
    #[arg(long, help = "Trust the local inspection CA without prompting")]
    trust_inspection: bool,
    #[arg(
        long,
        help = "Trust the organization's managed Gateway CA without prompting"
    )]
    trust_organization_ca: bool,
}

const SUPPORT_URL: &str = "https://github.com/agentdesktop-dev/agentdesktop/issues/new";
const CA_CERT_PLACEHOLDER: &str = "__AGENTDESKTOP_INSPECTION_CA_CERT__";
const CA_KEY_PLACEHOLDER: &str = "__AGENTDESKTOP_INSPECTION_CA_KEY__";
const INSPECTION_CA_COMMON_NAME: &str = "Agent Gateway Local Inspection CA";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nAgent Desktop setup could not finish.");
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
            trust_inspection: false,
            trust_organization_ca: false,
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
    if args.trust_organization_ca {
        bail!("--trust-organization-ca is only available for managed installers");
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
        .prefix("agentdesktop-installer-")
        .tempdir_in(parent)
        .context("failed to create temporary extraction directory")?;
    extract_payload(payload.path())?;
    let ca_directory = config
        .parent()
        .context("Agent Gateway config has no parent")?
        .join("inspection-ca");
    let config_created = if config.exists() {
        seed_config(&payload.path().join("config"), &config, None)?
    } else {
        initialize_inspection_ca(&ca_directory)?;
        seed_config(&payload.path().join("config"), &config, Some(&ca_directory))?
    };

    let capture_enabled = config_created
        || (ca_directory.join("ca.crt").is_file() && ca_directory.join("ca.key").is_file());
    let mut install_arguments = vec![
        "install".to_owned(),
        "--root".to_owned(),
        root.to_string_lossy().into_owned(),
        "--connector".to_owned(),
        payload
            .path()
            .join("connector")
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
    ];
    if capture_enabled {
        install_arguments.push("--capture-enabled".to_owned());
    }
    run_internal(payload.path(), install_arguments)?;

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
    let trust_inspection = capture_enabled
        && (args.trust_inspection || (!args.yes && confirm_inspection_trust(&ca_directory)?));
    if args.trust_inspection && !capture_enabled {
        bail!("inspection trust requires an installer-created capture configuration");
    }
    install_system_integration(
        &payload.path().join("capture-setup"),
        trust_inspection
            .then(|| ca_directory.join("ca.crt"))
            .as_deref(),
    )?;
    if trust_inspection {
        println!("  Trust:   local inspection CA installed");
    } else if capture_enabled {
        println!("  Trust:   not installed; transparent capture remains unavailable");
    } else {
        println!("  Capture: unavailable with the preserved custom Agent Gateway config");
    }
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
            root.join("bin/agentdesktop-install").display(),
            root.display()
        );
    } else {
        println!("\nAgent Desktop is ready.");
        if args.connect_agents {
            run_agent_setup(payload.path(), true)?;
        } else if args.yes {
            println!("AI agent settings were not changed.");
        } else {
            run_agent_setup(payload.path(), false)?;
        }
        println!("\nUpdate AI agent connections later with:\n  agentdesktop connect-agents");
        print_command_path_warning(&command_link);
    }
    Ok(())
}

fn install_managed(args: InstallArgs) -> Result<()> {
    if args.config.is_some() || args.no_start || args.connect_agents {
        bail!("managed installation does not accept standalone service or agent setup options");
    }
    if args.trust_inspection {
        bail!("--trust-inspection is only available for standalone installers");
    }
    let root = args.root.map_or_else(default_root, Ok)?;
    let command_link = default_command_link()?;
    if !root.is_absolute() {
        bail!("install root must be absolute");
    }
    let bootstrap = load_organization(args.organization.as_deref())?;
    if args.trust_organization_ca && bootstrap.trust.is_none() {
        bail!("this organization installer does not include a managed Gateway CA");
    }
    print_managed_summary(&root, &bootstrap);
    if !args.yes && !confirm()? {
        println!("Installation cancelled; no files were changed.");
        return Ok(());
    }

    let parent = root.parent().context("install root has no parent")?;
    fs::create_dir_all(parent)?;
    let payload = tempfile::Builder::new()
        .prefix("agentdesktop-installer-")
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

    let trust_organization_ca = match bootstrap.trust.as_ref() {
        Some(_) if args.trust_organization_ca => true,
        Some(trust) if !args.yes => confirm_organization_trust(&bootstrap, trust)?,
        _ => false,
    };
    if trust_organization_ca {
        let trust = bootstrap
            .trust
            .as_ref()
            .expect("managed trust was checked before installation");
        let certificate = payload.path().join("organization-ca.crt");
        fs::write(&certificate, &trust.certificate_pem)?;
        set_mode(&certificate, 0o600)?;
        install_system_integration(&payload.path().join("capture-setup"), Some(&certificate))?;
    }

    println!("\nInstallation complete");
    println!("  Organization: {}", bootstrap.organization.display_name);
    println!("  Files:        {}", root.display());
    println!("  Service:      installed, awaiting user sign-in");
    if trust_organization_ca {
        println!("  Trust:        organization Gateway CA installed");
    } else if bootstrap.trust.is_some() {
        println!("  Trust:        unchanged; organization CA was not installed");
    } else {
        println!("  Trust:        organization CA expected to be preinstalled");
    }
    println!("\nNo AI agent settings were changed.");
    println!("To sign in and connect your AI agents, run:\n  agentdesktop connect-agents");
    print_command_path_warning(&command_link);
    Ok(())
}

fn default_command_link() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/bin/agentdesktop"))
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
        "Agent Desktop for {}\n",
        bootstrap.organization.display_name
    );
    println!("This installs for the current user:");
    println!("  - Agent Desktop");
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
                &["--user", "status", "--no-pager", "agentdesktop.service"],
            ),
        ),
        (
            "Recent service log",
            command_output(
                "journalctl",
                &[
                    "--user-unit",
                    "agentdesktop.service",
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
    Ok(state.join("agentdesktop/install-support.txt"))
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
    writeln!(report, "Agent Desktop installer support report")?;
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
            b"GET /_agentdesktop/healthz HTTP/1.1\r\nHost: 127.0.0.1:8081\r\nConnection: close\r\n\r\n",
        )?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.1 200") && response.contains(r#""status":"ok""#))
    })();
    result.unwrap_or(false)
}

fn default_root() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set; pass --root explicitly")?;
    Ok(PathBuf::from(home).join(".local/lib/agentdesktop"))
}

fn default_config() -> Result<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join("agentgateway/config.yaml"));
    }
    let home = env::var_os("HOME").context("HOME is not set; pass --config explicitly")?;
    Ok(PathBuf::from(home).join(".config/agentgateway/config.yaml"))
}

fn print_summary(root: &Path, config: &Path, start: bool) {
    println!("Agent Desktop\n");
    println!("This installs for the current user:");
    println!("  - Agent Gateway");
    println!("  - Agent Desktop");
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

fn seed_config(starter: &Path, destination: &Path, ca_directory: Option<&Path>) -> Result<bool> {
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
    let mut contents = fs::read_to_string(starter)?;
    if let Some(ca_directory) = ca_directory {
        contents = contents
            .replace(
                CA_CERT_PLACEHOLDER,
                &ca_directory.join("ca.crt").to_string_lossy(),
            )
            .replace(
                CA_KEY_PLACEHOLDER,
                &ca_directory.join("ca.key").to_string_lossy(),
            );
    }
    if contents.contains(CA_CERT_PLACEHOLDER) || contents.contains(CA_KEY_PLACEHOLDER) {
        bail!("starter Agent Gateway config requires inspection CA initialization");
    }
    file.write_all(contents.as_bytes())?;
    Ok(true)
}

fn initialize_inspection_ca(state_directory: &Path) -> Result<()> {
    fs::create_dir_all(state_directory).with_context(|| {
        format!(
            "create inspection CA state directory {}",
            state_directory.display()
        )
    })?;
    set_mode(state_directory, 0o700)?;

    let certificate_path = state_directory.join("ca.crt");
    let private_key_path = state_directory.join("ca.key");
    if certificate_path.exists() || private_key_path.exists() {
        bail!(
            "refusing to replace existing inspection CA material in {}",
            state_directory.display()
        );
    }

    let key = KeyPair::generate().context("generate inspection CA private key")?;
    let mut parameters = CertificateParams::default();
    parameters
        .distinguished_name
        .push(DnType::CommonName, INSPECTION_CA_COMMON_NAME);
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let certificate = parameters
        .self_signed(&key)
        .context("generate inspection CA certificate")?;

    write_exclusive(&private_key_path, key.serialize_pem().as_bytes(), 0o600)?;
    if let Err(error) = write_exclusive(&certificate_path, certificate.pem().as_bytes(), 0o644) {
        let _ = fs::remove_file(&private_key_path);
        return Err(error);
    }
    Ok(())
}

fn write_exclusive(path: &Path, contents: &[u8], _mode: u32) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(_mode);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn install_system_integration(source: &Path, certificate: Option<&Path>) -> Result<()> {
    let mut command = ProcessCommand::new("pkexec");
    command.arg(source).arg("system-install");
    if let Some(certificate) = certificate {
        command.arg("--certificate").arg(certificate);
    }
    let status = command
        .status()
        .context("authorize Agent Desktop system integration")?;
    if !status.success() {
        bail!("Agent Desktop system integration failed with {status}");
    }
    Ok(())
}

fn confirm_inspection_trust(ca_directory: &Path) -> Result<bool> {
    let certificate = ca_directory.join("ca.crt");
    let fingerprint = Sha256::digest(fs::read(&certificate)?);
    println!("\nOptional secure inspection");
    println!("Agent Desktop can trust the local Agent Gateway inspection CA.");
    println!("This applies only when you explicitly launch an app through Agent Desktop.");
    println!("CA SHA-256: {}", hex_digest(&fingerprint));
    print!("Enable secure inspection? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    let bytes_read = io::stdin().read_line(&mut answer)?;
    Ok(bytes_read > 0 && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn confirm_organization_trust(
    bootstrap: &OrganizationBootstrap,
    trust: &agentdesktop::organization::TrustBootstrap,
) -> Result<bool> {
    let fingerprint = Sha256::digest(trust.certificate_pem.as_bytes());
    println!("\nOptional organization trust");
    println!(
        "{} can install its managed Agent Gateway CA on this device.",
        bootstrap.organization.display_name
    );
    println!("Inspection scope: {}", trust.inspection_scope);
    println!("This changes the system trust store and requires administrator approval.");
    println!("CA SHA-256: {}", hex_digest(&fingerprint));
    print!("Install this organization CA? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    let bytes_read = io::stdin().read_line(&mut answer)?;
    Ok(bytes_read > 0 && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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

fn run_internal(directory: &Path, args: impl IntoIterator<Item = String>) -> Result<()> {
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
    fn initializes_owner_only_inspection_ca_without_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("inspection-ca");

        initialize_inspection_ca(&state).unwrap();

        assert!(
            fs::read_to_string(state.join("ca.crt"))
                .unwrap()
                .contains("BEGIN CERTIFICATE")
        );
        assert!(
            fs::read_to_string(state.join("ca.key"))
                .unwrap()
                .contains("BEGIN PRIVATE KEY")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&state).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(state.join("ca.key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(initialize_inspection_ca(&state).is_err());
    }

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
        assert!(!defaults.trust_organization_ca);

        let Cli {
            command: Some(Command::Install(trusted)),
        } = Cli::try_parse_from(["installer", "install", "--yes", "--trust-organization-ca"])
            .unwrap()
        else {
            panic!("install command was not parsed");
        };
        assert!(trusted.trust_organization_ca);

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
