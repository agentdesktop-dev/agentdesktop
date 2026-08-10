use std::{fs, io::Write, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub device_id: String,
    pub credential: String,
}

pub fn load(path: &Path) -> anyhow::Result<Option<Identity>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .context("parse device identity")
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read identity from {}", path.display())),
    }
}

pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create state directory {}", parent.display()))?;

    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, &serde_json::to_vec_pretty(identity)?)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("install identity at {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("write identity to {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write identity to {}", path.display()))
}
