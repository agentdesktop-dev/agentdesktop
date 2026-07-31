use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    Verify {
        #[arg(long)]
        root: PathBuf,
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
        Command::Verify { root } => {
            validate_owned_tree(&root)?;
            println!("verified standalone bundle at {}", root.display());
            Ok(())
        }
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
        let files = files
            .iter()
            .map(|(_, relative, _)| {
                let destination = staging.join(relative);
                Ok(InstalledFile {
                    path: (*relative).to_owned(),
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
