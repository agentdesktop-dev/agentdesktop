use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inference_gateways: BTreeMap<String, InferenceGatewayConfig>,
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InferenceGatewayConfig {
    pub url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<InferenceGatewayAuthentication>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum InferenceGatewayAuthentication {
    ControllerJwt { audience: String },
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
    pub inference_gateway: String,
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration from {}", path.display()))?;
    parse(&contents).with_context(|| format!("parse configuration from {}", path.display()))
}

pub fn parse(contents: &str) -> anyhow::Result<Config> {
    let config: Config =
        crate::serdes::yamlviajson::from_str(contents).context("parse configuration")?;
    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> anyhow::Result<()> {
        for (name, gateway) in &self.inference_gateways {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                anyhow::bail!(
                    "inference gateway name {name:?} must contain only letters, numbers, '.', '-', or '_'"
                );
            }
            if !matches!(gateway.url.scheme(), "http" | "https") {
                anyhow::bail!(
                    "inference gateway {name} URL must use HTTP or HTTPS, got {}",
                    gateway.url.scheme()
                );
            }
            if gateway.url.host().is_none() {
                anyhow::bail!("inference gateway {name} URL must include a host");
            }
            if !gateway.url.username().is_empty() || gateway.url.password().is_some() {
                anyhow::bail!("inference gateway {name} URL cannot include credentials");
            }
            if gateway.url.query().is_some() || gateway.url.fragment().is_some() {
                anyhow::bail!("inference gateway {name} URL cannot include a query or fragment");
            }
            if let Some(InferenceGatewayAuthentication::ControllerJwt { audience }) =
                &gateway.authentication
                && audience.trim().is_empty()
            {
                anyhow::bail!("inference gateway {name} JWT audience cannot be empty");
            }
        }

        if let Some(claude_code) = &self.programs.claude_code
            && !self
                .inference_gateways
                .contains_key(&claude_code.inference_gateway)
        {
            anyhow::bail!(
                "Claude Code references unknown inference gateway {}",
                claude_code.inference_gateway
            );
        }
        Ok(())
    }
}
