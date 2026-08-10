use std::{fs, io::Write, path::Path};

use anyhow::Context;

use crate::config::ClaudeCodeConfig;

const FILE_NAME: &str = "50-agentplane.json";

pub fn apply(directory: &Path, config: Option<&ClaudeCodeConfig>) -> anyhow::Result<()> {
    let path = directory.join(FILE_NAME);
    let Some(config) = config else {
        return remove(&path);
    };

    let mut contents = serde_json::to_vec_pretty(&config.managed_settings)
        .context("serialize Claude Code managed settings")?;
    contents.push(b'\n');
    let action = match fs::read(&path) {
        Ok(existing) if existing == contents => {
            eprintln!(
                "claude-code: managed settings already current at {}",
                path.display()
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
    eprintln!(
        "claude-code: {action} managed settings at {}",
        path.display()
    );
    Ok(())
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            eprintln!("claude-code: remove managed settings at {}", path.display());
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "claude-code: managed settings already absent at {}",
                path.display()
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
