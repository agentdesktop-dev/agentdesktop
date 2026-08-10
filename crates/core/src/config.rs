use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConfig>,
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerConfig {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_certificate_path: Option<PathBuf>,
    #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
    pub heartbeat_interval: Duration,
}

fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(30)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeConfig>,
}

impl ProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeCodeConfig {
    pub managed_settings: Map<String, Value>,
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration from {}", path.display()))?;
    crate::serdes::yamlviajson::from_str(&contents)
        .with_context(|| format!("parse configuration from {}", path.display()))
}

pub fn parse(contents: &str) -> anyhow::Result<Config> {
    crate::serdes::yamlviajson::from_str(contents).context("parse configuration")
}
