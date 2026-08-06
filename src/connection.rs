use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::apps::claude::{ConnectionStatus, connect_installed, is_installed};
use crate::identity::enrollment::{
    EnrollmentClient, EnrollmentStatus, load_enrollment_for, save_enrollment_for,
};
use crate::identity::oauth::{LoginConfig, ManagedIdentity, load_session_for, login};
use crate::identity::storage::{CredentialStorageMode, CredentialStore, default_storage_root};
use crate::organization::OrganizationBootstrap;

pub async fn run(yes: bool) -> anyhow::Result<()> {
    if let Some((root, bootstrap)) = installed_managed_bootstrap()? {
        prepare_managed_connection(&root, &bootstrap)
            .await
            .with_context(|| {
                format!(
                    "managed setup could not finish; contact {}",
                    bootstrap.organization.support_url
                )
            })?;
    }
    if !is_installed()? {
        println!("No supported AI agents were found.");
        return Ok(());
    }
    if !yes && !confirm_claude_connection()? {
        println!("No agents were changed.");
        return Ok(());
    }
    match connect_installed()? {
        ConnectionStatus::Connected => println!("Claude Code connected."),
        ConnectionStatus::AlreadyConnected => println!("Claude Code is already connected."),
        ConnectionStatus::NotInstalled => println!("No supported AI agents were found."),
    }
    Ok(())
}

fn confirm_claude_connection() -> anyhow::Result<bool> {
    use std::io::Write;

    println!("Claude Code was found.");
    println!("This will update your Claude Code settings so requests use Agent Desktop.");
    print!("Connect Claude Code? [Y/n] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    let bytes_read = std::io::stdin().read_line(&mut answer)?;
    Ok(bytes_read > 0
        && matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "" | "y" | "yes"
        ))
}

fn installed_managed_bootstrap() -> anyhow::Result<Option<(PathBuf, OrganizationBootstrap)>> {
    let executable = std::env::current_exe()?;
    let Some(root) = executable.parent().and_then(Path::parent) else {
        return Ok(None);
    };
    let path = root.join("share/organization.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some((
        root.to_owned(),
        OrganizationBootstrap::parse(&std::fs::read(path)?)?,
    )))
}

async fn prepare_managed_connection(
    root: &Path,
    bootstrap: &OrganizationBootstrap,
) -> anyhow::Result<()> {
    let gateway_origin = bootstrap.gateway.url.clone();
    let storage_root = default_storage_root()?;
    let store = if storage_root.exists() {
        CredentialStore::load(&storage_root)?
    } else {
        CredentialStore::setup(CredentialStorageMode::Auto, &storage_root)?
    };
    let session = match load_session_for(&bootstrap.identity.issuer, &gateway_origin, &store) {
        Ok(session) => session,
        Err(_) => {
            println!(
                "Opening your {} sign-in in the browser...",
                bootstrap.organization.display_name
            );
            login(
                &LoginConfig {
                    issuer: bootstrap.identity.issuer.clone(),
                    client_id: bootstrap.identity.client_id.clone(),
                    audience: bootstrap.identity.audience.clone(),
                    scope: bootstrap.identity.scope.clone(),
                    gateway_origin: gateway_origin.clone(),
                },
                &store,
                crate::identity::oauth::open_authorization_url,
            )
            .await?
        }
    };
    let identity = ManagedIdentity::new(session, store.clone());
    let client = EnrollmentClient::new(&bootstrap.identity.enrollment_url)?;
    let mut enrollment =
        match load_enrollment_for(&bootstrap.identity.issuer, &gateway_origin, &store) {
            Ok(enrollment) => enrollment,
            Err(_) => {
                let enrollment = client.request(&identity).await?;
                save_enrollment_for(
                    &bootstrap.identity.issuer,
                    &gateway_origin,
                    &store,
                    &enrollment,
                )?;
                enrollment
            }
        };
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut announced = false;
    loop {
        enrollment = client.status(&identity, &enrollment).await?;
        save_enrollment_for(
            &bootstrap.identity.issuer,
            &gateway_origin,
            &store,
            &enrollment,
        )?;
        match enrollment.status {
            EnrollmentStatus::Approved => break,
            EnrollmentStatus::Pending | EnrollmentStatus::Issuing if Instant::now() < deadline => {
                if !announced {
                    println!("Waiting for your organization to approve this device...");
                    announced = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            EnrollmentStatus::Pending | EnrollmentStatus::Issuing => anyhow::bail!(
                "device approval did not complete; contact {}",
                bootstrap.organization.support_url
            ),
            EnrollmentStatus::Rejected => anyhow::bail!(
                "this device enrollment was rejected; contact {}",
                bootstrap.organization.support_url
            ),
        }
    }

    let status = Command::new(root.join("bin/agentdesktop-install"))
        .args(["service", "enable", "--root"])
        .arg(root)
        .status()?;
    if !status.success() {
        anyhow::bail!("could not start Agent Desktop");
    }
    wait_for_health().await?;
    println!("Agent Desktop is ready.");
    Ok(())
}

async fn wait_for_health() -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let client = reqwest::Client::new();
    while Instant::now() < deadline {
        if client
            .get("http://127.0.0.1:8081/_agentdesktop/healthz")
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Agent Desktop did not become ready")
}
