use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::thread;
use std::time::{Duration, Instant};

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
    #[arg(long, help = "Accept the installation summary without prompting")]
    yes: bool,
    #[arg(long, help = "Install files without starting the user service")]
    no_start: bool,
}

fn main() -> Result<()> {
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
            yes: false,
            no_start: false,
        },
    };
    install(args)
}

fn install(args: InstallArgs) -> Result<()> {
    let root = args.root.map_or_else(default_root, Ok)?;
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
            "--claude".to_owned(),
            payload.path().join("claude").to_string_lossy().into_owned(),
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
        println!("\nCheck health:\n  curl http://127.0.0.1:8080/_agentgateway/healthz");
    }
    Ok(())
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
    bail!(
        "service started but did not become healthy; inspect it with `systemctl --user status agentgateway-edge.service`"
    )
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
    println!("  - Identity and Claude Code helpers");
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
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
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
