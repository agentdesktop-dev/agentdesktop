//! MXC engine: request generation for the Windows process container and,
//! on a POSIX host, the bubblewrap backend.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::Engine;
use crate::launch::plan::{ConsolePlan, LaunchPlan, PathStyle};
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

    fn box_relay(&self, style: PathStyle, host_relay: SocketAddr) -> SocketAddr {
        match style {
            // The process container shares host loopback.
            PathStyle::Windows => host_relay,
            // A bubblewrap box has a private network; the host is its gateway.
            PathStyle::Posix => SocketAddr::new(
                crate::llm_proxy::BUBBLEWRAP_HOST_GATEWAY
                    .parse()
                    .expect("the gateway constant is an IP address"),
                host_relay.port(),
            ),
        }
    }

    fn request(&self, plan: &LaunchPlan, launch_id: &str) -> Result<Vec<u8>> {
        match plan.style {
            PathStyle::Windows => serde_json::to_vec_pretty(&self.to_request(plan, launch_id)),
            PathStyle::Posix => {
                serde_json::to_vec_pretty(&self.to_bubblewrap_request(plan, launch_id)?)
            }
        }
        .context("encode MXC request")
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

impl MxcEngine {
    /// Wraps a resolved POSIX plan in the MXC request for the bubblewrap
    /// backend. The box gets a private network whose only destination is the
    /// relay, named as `runtimeConfig.networkProxy`; MXC points the box's
    /// proxy variables at it and drops everything else, so there is no
    /// "egress allow" form of this request.
    pub(crate) fn to_bubblewrap_request(
        &self,
        plan: &LaunchPlan,
        launch_id: &str,
    ) -> Result<BubblewrapRequest> {
        let Some(relay) = plan.host_proxy else {
            bail!("a bubblewrap launch needs the relay address");
        };
        if plan.egress == agentdesktop_core::config::ContainmentEgress::Allow {
            bail!(
                "daemon.containment.egress: allow is not available on this operating system: the box reaches the relay only"
            );
        }
        let render = |path: &std::path::PathBuf| plan.style.render(path);
        // `exec` so the program replaces the backend's `sh -c` shell and its
        // exit code is the box's.
        let mut command_line = format!("exec {}", crate::launch::sh_quote(&render(&plan.program)));
        for arg in &plan.args {
            command_line.push(' ');
            command_line.push_str(arg);
        }
        Ok(BubblewrapRequest {
            version: "1.0.0".into(),
            containment: "bubblewrap".into(),
            container_id: self.container_id(launch_id),
            process: MxcProcess {
                command_line,
                cwd: render(&plan.cwd),
                inherit_default_env: false,
                env: plan
                    .env
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
            },
            filesystem: MxcFilesystem {
                readwrite_paths: plan.readwrite.iter().map(render).collect(),
                readonly_paths: plan.readonly.iter().map(render).collect(),
                denied_paths: plan.denied.iter().map(render).collect(),
            },
            network: MxcNetwork {
                egress: MxcEgress {
                    default: "deny".into(),
                },
                ingress: MxcIngress {
                    default: "deny".into(),
                    host_loopback: "deny".into(),
                },
            },
            runtime_config: MxcRuntimeConfig {
                network_proxy: format!("http://{relay}"),
            },
        })
    }
}

/// Reads the executor's `--available-backends` answer: `Ok` when the
/// bubblewrap backend is there and can confine a box to its proxy
/// (`proxyEnforcement`); otherwise the reason, the backend's own warning when
/// it gives one (a missing `slirp4netns`, or user namespaces the host refuses).
pub(crate) fn bubblewrap_ready(available_backends: &[u8]) -> Result<(), String> {
    let backends: serde_json::Value = serde_json::from_slice(available_backends)
        .map_err(|error| format!("the executor's backend report is not JSON: {error}"))?;
    let Some(bubblewrap) = backends
        .as_array()
        .into_iter()
        .flatten()
        .find(|backend| backend["backend"] == "bubblewrap")
    else {
        return Err("the executor reports no bubblewrap backend on this host".to_owned());
    };
    let enforces = bubblewrap["capabilities"]
        .as_array()
        .is_some_and(|capabilities| capabilities.iter().any(|c| c == "proxyEnforcement"));
    if enforces {
        return Ok(());
    }
    let warnings: Vec<&str> = bubblewrap["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    Err(if warnings.is_empty() {
        "the bubblewrap backend cannot confine a box to the relay on this host".to_owned()
    } else {
        warnings.join("; ")
    })
}

// No Debug: the request's environment carries the session pairing.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BubblewrapRequest {
    pub version: String,
    pub containment: String,
    pub container_id: String,
    pub process: MxcProcess,
    pub filesystem: MxcFilesystem,
    pub network: MxcNetwork,
    pub runtime_config: MxcRuntimeConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MxcRuntimeConfig {
    /// The relay on host loopback: the box's only destination.
    pub network_proxy: String,
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
