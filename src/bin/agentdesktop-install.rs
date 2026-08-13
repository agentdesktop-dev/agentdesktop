use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;

use agentdesktop::organization::OrganizationBootstrap;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST: &str = "agentdesktop-install.json";
const SYSTEMD_UNIT: &str = "share/systemd/user/agentdesktop.service";
const MACHINE_SYSTEMD_UNIT: &str = "share/systemd/system/agentdesktop-forwarder.service";
const SESSION_SOCKET: &str = "/run/agentdesktop/sessions.sock";

#[derive(Debug, Parser)]
#[command(version, about = "Install or remove an Agent Desktop bundle")]
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
        agentgateway: PathBuf,
        #[arg(long)]
        capture_setup: Option<PathBuf>,
        #[arg(long)]
        starter_config: PathBuf,
        #[arg(long)]
        control: Option<PathBuf>,
        #[arg(long)]
        gateway_config: Option<PathBuf>,
        #[arg(long)]
        command_link: PathBuf,
        #[arg(long, default_value_t = false)]
        capture_enabled: bool,
    },
    ManagedInstall {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        connector: PathBuf,
        #[arg(long)]
        organization: PathBuf,
        #[arg(long)]
        control: Option<PathBuf>,
        #[arg(long)]
        command_link: PathBuf,
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
    #[serde(default)]
    command_link: Option<PathBuf>,
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
            agentgateway,
            capture_setup,
            starter_config,
            control,
            gateway_config,
            command_link,
            capture_enabled,
        } => {
            let mut files: Vec<(&Path, &str, u32)> = vec![
                (connector.as_path(), "bin/agentdesktop", 0o755),
                (agentgateway.as_path(), "bin/agentgateway", 0o755),
                (
                    starter_config.as_path(),
                    "share/examples/agentgateway.yaml",
                    0o600,
                ),
            ];
            if let Some(control) = &control {
                files.push((control.as_path(), "bin/agentdesktop-install", 0o755));
            }
            if let Some(capture_setup) = &capture_setup {
                files.push((
                    capture_setup.as_path(),
                    "bin/agentdesktop-capture-setup",
                    0o755,
                ));
            }
            install(
                &root,
                &files,
                &standalone_systemd_unit(&root, gateway_config.as_deref(), capture_enabled),
                None,
                "standalone",
                &command_link,
            )
        }
        Command::ManagedInstall {
            root,
            connector,
            organization,
            control,
            command_link,
        } => {
            let bootstrap = OrganizationBootstrap::parse(&fs::read(&organization)?)?;
            let mut files: Vec<(&Path, &str, u32)> = vec![
                (connector.as_path(), "bin/agentdesktop", 0o755),
                (organization.as_path(), "share/organization.json", 0o644),
            ];
            if let Some(control) = &control {
                files.push((control.as_path(), "bin/agentdesktop-install", 0o755));
            }
            install(
                &root,
                &files,
                &managed_user_systemd_unit(&root, &bootstrap),
                Some((
                    MACHINE_SYSTEMD_UNIT,
                    managed_machine_systemd_unit(&root, &bootstrap),
                )),
                "managed",
                &command_link,
            )
        }
        Command::Uninstall { root } => uninstall(&root),
        Command::Verify { root } => {
            validate_owned_tree(&root)?;
            println!("verified edge bundle at {}", root.display());
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
            Some(Path::new("agentdesktop.service")),
        )?;
    } else {
        run_systemctl(
            systemctl,
            ["disable", "--now"],
            Some(Path::new("agentdesktop.service")),
        )?;
    }
    println!(
        "{} user service",
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
    files: &[(&Path, &str, u32)],
    systemd_unit: &str,
    machine_systemd_unit: Option<(&str, String)>,
    deployment: &str,
    command_link: &Path,
) -> Result<()> {
    if !root.is_absolute() {
        bail!("install root must be absolute");
    }
    if !command_link.is_absolute() {
        bail!("command link must be absolute");
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
    remove_owned_tree_if_present(&staging, false)?;
    remove_owned_tree_if_present(&backup, false)?;
    fs::create_dir(&staging)?;

    let stage_result = (|| -> Result<()> {
        for (source, relative, mode) in files {
            let destination = staging.join(relative);
            fs::create_dir_all(destination.parent().expect("destination has parent"))?;
            fs::copy(source, &destination).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
            set_mode(&destination, *mode)?;
        }
        let unit = staging.join(SYSTEMD_UNIT);
        fs::create_dir_all(unit.parent().expect("systemd unit has parent"))?;
        fs::write(&unit, systemd_unit)?;
        set_mode(&unit, 0o644)?;
        if let Some((relative, contents)) = &machine_systemd_unit {
            let unit = staging.join(relative);
            fs::create_dir_all(unit.parent().expect("systemd unit has parent"))?;
            fs::write(&unit, contents)?;
            set_mode(&unit, 0o644)?;
        }

        let mut installed_files = files
            .iter()
            .map(|(_, relative, _)| *relative)
            .chain(std::iter::once(SYSTEMD_UNIT))
            .collect::<Vec<_>>();
        if let Some((relative, _)) = &machine_systemd_unit {
            installed_files.push(relative);
        }
        let files = installed_files
            .into_iter()
            .map(|relative| {
                let destination = staging.join(relative);
                Ok(InstalledFile {
                    path: relative.to_owned(),
                    sha256: file_sha256(&destination)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let manifest = InstallManifest {
            format_version: 3,
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
            files,
            command_link: Some(command_link.to_owned()),
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
    if let Err(error) = install_command_link(root, command_link) {
        let _ = fs::remove_dir_all(root);
        if backup.exists() {
            let _ = fs::rename(&backup, root);
        }
        return Err(error);
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    println!("installed {deployment} bundle at {}", root.display());
    Ok(())
}

fn standalone_systemd_unit(
    root: &Path,
    gateway_config: Option<&Path>,
    capture_enabled: bool,
) -> String {
    let connector = quote_systemd_arg(&root.join("bin/agentdesktop"));
    let agentgateway = quote_systemd_arg(&root.join("bin/agentgateway"));
    let config =
        quote_systemd_arg(gateway_config.unwrap_or(&root.join("share/examples/agentgateway.yaml")));
    let capture = if capture_enabled {
        " --capture-enabled"
    } else {
        ""
    };
    format!(
        "[Unit]\nDescription=Agent Desktop\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={connector} serve --mode standalone --upstream http://127.0.0.1:15008 --native-target native.agentdesktop.internal:4000 --gateway-binary {agentgateway} --gateway-config {config}{capture}\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectHome=read-only\n\n[Install]\nWantedBy=default.target\n"
    )
}

fn managed_user_systemd_unit(root: &Path, bootstrap: &OrganizationBootstrap) -> String {
    let connector = quote_systemd_arg(&root.join("bin/agentdesktop"));
    let upstream = quote_systemd_value(bootstrap.gateway.url.as_str());
    let issuer = quote_systemd_value(bootstrap.identity.issuer.as_str());
    let enrollment_url = quote_systemd_value(bootstrap.identity.enrollment_url.as_str());
    format!(
        "[Unit]\nDescription=Agent Desktop user session agent\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={connector} session-agent --mode managed --upstream {upstream} --identity-issuer {issuer} --enrollment-url {enrollment_url} --session-socket {SESSION_SOCKET}\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\n\n[Install]\nWantedBy=default.target\n"
    )
}

fn managed_machine_systemd_unit(root: &Path, bootstrap: &OrganizationBootstrap) -> String {
    let connector = quote_systemd_arg(&root.join("bin/agentdesktop"));
    let upstream = quote_systemd_value(bootstrap.gateway.url.as_str());
    let capture = if bootstrap.trust.is_some() {
        " --capture-enabled"
    } else {
        ""
    };
    format!(
        "[Unit]\nDescription=Agent Desktop machine forwarder\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={connector} serve --mode managed --upstream {upstream} --session-socket {SESSION_SOCKET}{capture}\nRestart=on-failure\nRestartSec=2\nRuntimeDirectory=agentdesktop\nRuntimeDirectoryMode=0755\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectHome=read-only\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn quote_systemd_arg(path: &Path) -> String {
    quote_systemd_value(&path.to_string_lossy())
}

fn quote_systemd_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn uninstall(root: &Path) -> Result<()> {
    let manifest = validate_owned_tree(root)?;
    if let Some(command_link) = manifest.command_link {
        fs::remove_file(command_link)?;
    }
    fs::remove_dir_all(root)?;
    println!("removed edge bundle from {}", root.display());
    Ok(())
}

fn validate_owned_tree(root: &Path) -> Result<InstallManifest> {
    validate_owned_tree_with_external(root, true)
}

fn validate_owned_tree_with_external(
    root: &Path,
    validate_external: bool,
) -> Result<InstallManifest> {
    let manifest = root.join(MANIFEST);
    let parsed: InstallManifest = serde_json::from_slice(
        &fs::read(&manifest)
            .with_context(|| format!("{} is not an edge bundle", root.display()))?,
    )?;
    if !matches!(parsed.format_version, 2 | 3) {
        bail!("unsupported install manifest format");
    }
    for installed in &parsed.files {
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
    if validate_external && let Some(command_link) = &parsed.command_link {
        validate_command_link(root, command_link)?;
    }
    Ok(parsed)
}

fn file_sha256(path: &Path) -> Result<String> {
    let digest = Sha256::digest(fs::read(path)?);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn remove_owned_tree_if_present(path: &Path, validate_external: bool) -> Result<()> {
    if path.exists() {
        validate_owned_tree_with_external(path, validate_external)?;
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn install_command_link(root: &Path, command_link: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let target = root.join("bin/agentdesktop");
    if let Ok(metadata) = fs::symlink_metadata(command_link) {
        if metadata.file_type().is_symlink() && fs::read_link(command_link)? == target {
            return Ok(());
        }
        bail!(
            "command path {} already exists and is not owned by this installation",
            command_link.display()
        );
    }
    fs::create_dir_all(
        command_link
            .parent()
            .context("command link has no parent")?,
    )?;
    symlink(target, command_link)?;
    Ok(())
}

#[cfg(not(unix))]
fn install_command_link(_root: &Path, _command_link: &Path) -> Result<()> {
    bail!("stable command links are not implemented on this platform")
}

fn validate_command_link(root: &Path, command_link: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(command_link)
        .with_context(|| format!("installed command {} is missing", command_link.display()))?;
    let expected = root.join("bin/agentdesktop");
    if !metadata.file_type().is_symlink() || fs::read_link(command_link)? != expected {
        bail!(
            "installed command {} has been modified",
            command_link.display()
        );
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
