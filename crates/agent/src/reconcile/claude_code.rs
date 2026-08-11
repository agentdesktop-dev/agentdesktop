use std::{fs, io::Write, path::Path};

use anyhow::Context;

use agentdesktop_core::config::{
    ClaudeCodeConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use serde::Serialize;
use tracing::info;

const FILE_NAME: &str = "50-agentdesktop.json";

#[derive(Serialize)]
struct ManagedSettings<'a> {
    env: Environment<'a>,
    #[serde(rename = "apiKeyHelper", skip_serializing_if = "Option::is_none")]
    api_key_helper: Option<String>,
}

#[derive(Serialize)]
struct Environment<'a> {
    #[serde(rename = "ANTHROPIC_BASE_URL")]
    anthropic_base_url: &'a str,
    #[serde(
        rename = "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
        skip_serializing_if = "Option::is_none"
    )]
    api_key_helper_ttl_ms: Option<&'static str>,
}

pub fn apply(
    directory: &Path,
    credential_helper: &str,
    config: Option<(&ClaudeCodeConfig, &InferenceGatewayConfig)>,
) -> anyhow::Result<()> {
    let path = directory.join(FILE_NAME);
    let Some((config, gateway)) = config else {
        return remove(&path);
    };

    let uses_helper = matches!(
        gateway.authentication,
        Some(InferenceGatewayAuthentication::ControllerJwt { .. })
    );

    let settings = ManagedSettings {
        env: Environment {
            anthropic_base_url: gateway.url.as_str(),
            api_key_helper_ttl_ms: uses_helper.then_some("60000"),
        },
        api_key_helper: uses_helper
            .then(|| format!("{credential_helper} {}", config.inference_gateway)),
    };
    let mut contents =
        serde_json::to_vec_pretty(&settings).context("serialize Claude Code managed settings")?;
    contents.push(b'\n');
    let action = match fs::read(&path) {
        Ok(existing) if existing == contents => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already current"
            );
            return Ok(());
        }
        Ok(_) => "update",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "create",
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Claude Code managed settings from {}", path.display())
            });
        }
    };

    fs::create_dir_all(directory).with_context(|| {
        format!(
            "create Claude Code settings directory {}",
            directory.display()
        )
    })?;
    let temporary = directory.join(format!(".{FILE_NAME}.tmp"));
    write_file(&temporary, &contents)?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("install Claude Code managed settings at {}", path.display()))?;
    info!(
        program = "claude-code",
        action,
        path = %path.display(),
        "reconciled managed settings"
    );
    Ok(())
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            info!(
                program = "claude-code",
                action = "remove",
                path = %path.display(),
                "reconciled managed settings"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already absent"
            );
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("remove Claude Code managed settings at {}", path.display())),
    }
}

fn write_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("write Claude Code managed settings to {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write Claude Code managed settings to {}", path.display()))
}
