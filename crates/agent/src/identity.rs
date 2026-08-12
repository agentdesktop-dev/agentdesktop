use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::secure_fs;

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
    secure_fs::ensure_private_dir(parent)?;
    secure_fs::atomic_write(path, &serde_json::to_vec_pretty(identity)?, 0o600)
}
