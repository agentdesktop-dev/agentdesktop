use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const MANIFEST: &str = "agentgateway-edge-install.json";

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
        claude: PathBuf,
        #[arg(long)]
        agentgateway: PathBuf,
        #[arg(long)]
        starter_config: PathBuf,
    },
    Uninstall {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct InstallManifest {
    format_version: u32,
    connector_version: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install {
            root,
            connector,
            identity,
            claude,
            agentgateway,
            starter_config,
        } => install(
            &root,
            &[
                (&connector, "bin/agentgateway-edge-connector", true),
                (&identity, "bin/agentgateway-edge-identity", true),
                (&claude, "bin/agentgateway-edge-claude", true),
                (&agentgateway, "bin/agentgateway", true),
                (&starter_config, "share/examples/agentgateway.yaml", false),
            ],
        ),
        Command::Uninstall { root } => uninstall(&root),
    }
}

fn install(root: &Path, files: &[(&Path, &str, bool)]) -> Result<()> {
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
        let manifest = InstallManifest {
            format_version: 1,
            connector_version: env!("CARGO_PKG_VERSION").to_owned(),
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
    if parsed.format_version != 1 {
        bail!("unsupported install manifest format");
    }
    Ok(())
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
