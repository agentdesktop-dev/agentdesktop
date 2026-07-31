use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST: &str = "agentgateway-edge-install.json";
const SYSTEMD_UNIT: &str = "share/systemd/user/agentgateway-edge.service";

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Install or remove a standalone Agent Gateway edge bundle"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Install {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        connector: PathBuf,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        agentgateway: PathBuf,
        #[arg(long)]
        starter_config: PathBuf,
        #[arg(long)]
        control: Option<PathBuf>,
        #[arg(long)]
        gateway_config: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        root: PathBuf,
    },
    Verify {
        #[arg(long)]
        root: PathBuf,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    Enable {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value = "systemctl", hide = true)]
        systemctl: PathBuf,
    },
    Disable {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value = "systemctl", hide = true)]
        systemctl: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallManifest {
    format_version: u32,
    connector_version: String,
    files: Vec<InstalledFile>,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstalledFile {
    path: String,
    sha256: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install {
            root,
            connector,
            identity,
            agentgateway,
            starter_config,
            control,
            gateway_config,
        } => {
            let mut files: Vec<(&Path, &str, bool)> = vec![
                (connector.as_path(), "bin/agentgateway-edge-connector", true),
                (identity.as_path(), "bin/agentgateway-edge-identity", true),
                (agentgateway.as_path(), "bin/agentgateway", true),
                (
                    starter_config.as_path(),
                    "share/examples/agentgateway.yaml",
                    false,
                ),
            ];
            if let Some(control) = &control {
                files.push((control.as_path(), "bin/agentgateway-edge-install", true));
            }
            install(&root, &files, gateway_config.as_deref())
        }
        Command::Uninstall { root } => uninstall(&root),
        Command::Verify { root } => {
            validate_owned_tree(&root)?;
            println!("verified standalone bundle at {}", root.display());
            Ok(())
        }
        Command::Service { command } => match command {
            ServiceCommand::Enable { root, systemctl } => service(&root, &systemctl, true),
            ServiceCommand::Disable { root, systemctl } => service(&root, &systemctl, false),
        },
    }
}

fn service(root: &Path, systemctl: &Path, enable: bool) -> Result<()> {
    validate_owned_tree(root)?;
    if enable {
        run_systemctl(
            systemctl,
            ["enable"],
            Some(root.join(SYSTEMD_UNIT).as_path()),
        )?;
        run_systemctl(
            systemctl,
            ["restart"],
            Some(Path::new("agentgateway-edge.service")),
        )?;
    } else {
        run_systemctl(
            systemctl,
            ["disable", "--now"],
            Some(Path::new("agentgateway-edge.service")),
        )?;
    }
    println!(
        "{} standalone user service",
        if enable { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn run_systemctl<const N: usize>(
    systemctl: &Path,
    args: [&str; N],
    unit: Option<&Path>,
) -> Result<()> {
    let mut command = ProcessCommand::new(systemctl);
    command.arg("--user").args(args);
    if let Some(unit) = unit {
        command.arg(unit);
    }
    let output = command.output().context("failed to run systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn install(
    root: &Path,
    files: &[(&Path, &str, bool)],
    gateway_config: Option<&Path>,
) -> Result<()> {
    if !root.is_absolute() {
        bail!("install root must be absolute");
    }
    for (source, _, _) in files {
        if !source.is_file() {
            bail!("install source {} is not a regular file", source.display());
        }
    }
    let parent = root.parent().context("install root has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = sibling(root, "staging");
    let backup = sibling(root, "backup");
    remove_owned_tree_if_present(&staging)?;
    remove_owned_tree_if_present(&backup)?;
    fs::create_dir(&staging)?;

    let stage_result = (|| -> Result<()> {
        for (source, relative, executable) in files {
            let destination = staging.join(relative);
            fs::create_dir_all(destination.parent().expect("destination has parent"))?;
            fs::copy(source, &destination).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
            set_mode(&destination, if *executable { 0o755 } else { 0o600 })?;
        }
        let unit = staging.join(SYSTEMD_UNIT);
        fs::create_dir_all(unit.parent().expect("systemd unit has parent"))?;
        fs::write(&unit, systemd_unit(root, gateway_config))?;
        set_mode(&unit, 0o644)?;

        let files = files
            .iter()
            .map(|(_, relative, _)| *relative)
            .chain(std::iter::once(SYSTEMD_UNIT))
            .map(|relative| {
                let destination = staging.join(relative);
                Ok(InstalledFile {
                    path: relative.to_owned(),
                    sha256: file_sha256(&destination)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let manifest = InstallManifest {
            format_version: 2,
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            files,
        };
        fs::write(
            staging.join(MANIFEST),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(())
    })();
    if let Err(error) = stage_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if root.exists() {
        validate_owned_tree(root)?;
        fs::rename(root, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, root) {
        if backup.exists() {
            let _ = fs::rename(&backup, root);
        }
        return Err(error.into());
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    println!("installed standalone bundle at {}", root.display());
    Ok(())
}

fn systemd_unit(root: &Path, gateway_config: Option<&Path>) -> String {
    let connector = quote_systemd_arg(&root.join("bin/agentgateway-edge-connector"));
    let agentgateway = quote_systemd_arg(&root.join("bin/agentgateway"));
    let config =
        quote_systemd_arg(gateway_config.unwrap_or(&root.join("share/examples/agentgateway.yaml")));
    format!(
        "[Unit]\nDescription=Agent Gateway Edge Connector\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={connector} --mode standalone --upstream http://127.0.0.1:4000 --gateway-binary {agentgateway} --gateway-config {config}\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectHome=read-only\n\n[Install]\nWantedBy=default.target\n"
    )
}

fn quote_systemd_arg(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn uninstall(root: &Path) -> Result<()> {
    validate_owned_tree(root)?;
    fs::remove_dir_all(root)?;
    println!("removed standalone bundle from {}", root.display());
    Ok(())
}

fn validate_owned_tree(root: &Path) -> Result<()> {
    let manifest = root.join(MANIFEST);
    let parsed: InstallManifest = serde_json::from_slice(
        &fs::read(&manifest)
            .with_context(|| format!("{} is not an edge bundle", root.display()))?,
    )?;
    if parsed.format_version != 2 {
        bail!("unsupported install manifest format");
    }
    for installed in parsed.files {
        let relative = Path::new(&installed.path);
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            bail!("install manifest contains an unsafe path");
        }
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("installed file {} is missing", path.display()))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!("installed path {} is not a regular file", path.display());
        }
        if file_sha256(&path)? != installed.sha256 {
            bail!("installed file {} has been modified", path.display());
        }
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let digest = Sha256::digest(fs::read(path)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn remove_owned_tree_if_present(path: &Path) -> Result<()> {
    if path.exists() {
        validate_owned_tree(path)?;
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn sibling(root: &Path, suffix: &str) -> PathBuf {
    let name = root
        .file_name()
        .expect("install root has a file name")
        .to_string_lossy();
    root.with_file_name(format!(".{name}.{suffix}"))
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
