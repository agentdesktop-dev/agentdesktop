//! MXC engine: Windows AppContainer request generation.

use anyhow::{Context, Result};
use serde::Serialize;

use super::Engine;
use crate::launch::plan::{ConsolePlan, LaunchPlan};
use crate::launch::win;

/// MXC sandbox engine.
pub(crate) struct MxcEngine;

impl Engine for MxcEngine {
    fn name(&self) -> &'static str {
        "mxc"
    }

    fn container_id(&self, launch_id: &str) -> String {
        format!("agentdesktop-{}", launch_id)
    }

    fn request(&self, plan: &LaunchPlan, launch_id: &str) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(&self.to_request(plan, launch_id)).context("encode MXC request")
    }
}

impl MxcEngine {
    /// Wraps a resolved plan in the MXC request envelope. Pure: the plan is
    /// final, nothing is read from the host.
    pub(crate) fn to_request(&self, plan: &LaunchPlan, launch_id: &str) -> MxcRequest {
        let cwd = win(&plan.cwd);
        let mut command_line = match plan.console {
            ConsolePlan::Window => format!(
                "cmd.exe /d /v:off /c start \"{}\" /WAIT /D \"{cwd}\" \"{}\"",
                plan.window_title,
                win(&plan.program)
            ),
            // Same-terminal hosting: no `start`, so the boxed program runs on
            // the executor's console (the client's) and cmd returns the
            // program's own exit code.
            ConsolePlan::Attach { .. } => {
                format!("cmd.exe /d /v:off /c \"\"{}\"", win(&plan.program))
            }
        };
        for arg in &plan.args {
            command_line.push(' ');
            command_line.push_str(arg);
        }
        if matches!(plan.console, ConsolePlan::Attach { .. }) {
            command_line.push('"');
        }
        MxcRequest {
            version: "1.0.0".into(),
            containment: "processcontainer".into(),
            container_id: self.container_id(launch_id),
            ui: MxcUi {
                disable: false,
                clipboard: "none".into(),
                injection: false,
            },
            process: MxcProcess {
                command_line,
                cwd,
                inherit_default_env: false,
                env: plan
                    .env
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
            },
            filesystem: MxcFilesystem {
                readwrite_paths: plan.readwrite.iter().map(|path| win(path)).collect(),
                readonly_paths: plan.readonly.iter().map(|path| win(path)).collect(),
                denied_paths: plan.denied.iter().map(|path| win(path)).collect(),
            },
            network: MxcNetwork {
                egress: MxcEgress {
                    default: match plan.egress {
                        agentdesktop_core::config::ContainmentEgress::Allow => "allow".into(),
                        agentdesktop_core::config::ContainmentEgress::Deny => "deny".into(),
                    },
                },
                ingress: MxcIngress {
                    default: "allow".into(),
                    host_loopback: "allow".into(),
                },
            },
        }
    }
}

// No Debug: the request's environment carries the session pairing.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MxcRequest {
    pub version: String,
    pub containment: String,
    pub container_id: String,
    pub ui: MxcUi,
    pub process: MxcProcess,
    pub filesystem: MxcFilesystem,
    pub network: MxcNetwork,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MxcUi {
    pub disable: bool,
    pub clipboard: String,
    pub injection: bool,
}

// No Debug: the env list carries the session pairing.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MxcProcess {
    pub command_line: String,
    pub cwd: String,
    pub inherit_default_env: bool,
    /// `KEY=VALUE` entries: MXC 1.0.0 rejects an object here (lab-verified).
    pub env: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct MxcFilesystem {
    #[serde(rename = "readwritePaths")]
    pub readwrite_paths: Vec<String>,
    #[serde(rename = "readonlyPaths")]
    pub readonly_paths: Vec<String>,
    #[serde(rename = "deniedPaths")]
    pub denied_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct MxcNetwork {
    pub egress: MxcEgress,
    pub ingress: MxcIngress,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct MxcEgress {
    pub default: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MxcIngress {
    pub default: String,
    pub host_loopback: String,
}

/// Builds the MXC request from a launch input: the resolved plan through the
/// MXC envelope as one call.
#[cfg(test)]
pub(crate) fn build_request(input: &crate::launch::LaunchInput) -> MxcRequest {
    MxcEngine.to_request(
        &crate::launch::plan(input, &crate::launch::profile::CLAUDE_CODE),
        &input.launch_id,
    )
}
