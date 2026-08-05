use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};
use clap::ValueEnum;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Action {
    Install,
    Remove,
}

pub fn run(action: Action) -> anyhow::Result<()> {
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
    let contents = std::fs::read(&certificate).with_context(|| {
        format!(
            "local inspection CA was not found at {}; reinstall Agent Desktop first",
            certificate.display()
        )
    })?;
    let fingerprint = Sha256::digest(&contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("Local Agent Gateway inspection CA");
    println!("SHA-256: {fingerprint}");
    println!(
        "Action: {} system trust for apps explicitly launched through Agent Desktop",
        match action {
            Action::Install => "install",
            Action::Remove => "remove",
        }
    );
    let helper_action = match action {
        Action::Install => "trust-install",
        Action::Remove => "trust-remove",
    };
    let status = Command::new("pkexec")
        .arg("/usr/libexec/agentdesktop-capture-setup")
        .arg(helper_action)
        .arg("--certificate")
        .arg(&certificate)
        .status()
        .context("authorize inspection trust change")?;
    if !status.success() {
        bail!("inspection trust change failed with {status}");
    }
    Ok(())
}
