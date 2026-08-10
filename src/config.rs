use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration from {}", path.display()))?;
    agent_core::serdes::yamlviajson::from_str(&contents)
        .with_context(|| format!("parse configuration from {}", path.display()))
}
