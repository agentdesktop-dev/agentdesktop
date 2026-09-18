use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use agentdesktop_client as client;
use agentdesktop_core::{
    config::{DaemonConfig, LlmGatewayAuthentication},
    model::LlmGatewayCredential,
};
use anyhow::{Context, bail};
use semver::Version;
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

use super::{CopilotCli, discovery};
use crate::{
    provider::shared::{CommandSpec, render_command, responses_base_url},
    secure_fs,
};

const DAEMON_TIMEOUT: Duration = Duration::from_secs(10);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_VERSION: Version = Version::new(1, 0, 84);
const EMPTY_REGISTRY: &[u8] = b"{\"providers\":[],\"models\":[]}\n";

/// The helper returns a raw, freshly requested credential. Errors deliberately
/// omit daemon response bodies: authentication failures may contain secrets.
pub(crate) async fn credential(socket: &Path) -> anyhow::Result<LlmGatewayCredential> {
    credential_with_timeout(socket, DAEMON_TIMEOUT).await
}

async fn credential_with_timeout(
    socket: &Path,
    deadline: Duration,
) -> anyhow::Result<LlmGatewayCredential> {
    let response: LlmGatewayCredential = timeout(
        deadline,
        client::get_bounded(
            socket,
            "/v1/llm-gateway/credential?client_id=copilot-cli",
            64 * 1024,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Copilot gateway credential request timed out; check daemon sign-in"))?
    .map_err(|_| anyhow::anyhow!(
        "Copilot gateway credential unavailable; check daemon sign-in and allowedClientIds for copilot-cli"
    ))?;
    if response.credential.is_empty()
        || response
            .credential
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("daemon returned an invalid Copilot gateway credential");
    }
    Ok(response)
}

pub(crate) async fn launch(socket: &Path, arguments: &[OsString]) -> anyhow::Result<ExitStatus> {
    // Native tools may change cwd before invoking helpers during a long session.
    let socket = std::path::absolute(socket).context("resolve the daemon socket path")?;
    let config: DaemonConfig = timeout(
        DAEMON_TIMEOUT,
        client::get_bounded(&socket, "/v1/effective-config", 4 * 1024 * 1024),
    )
    .await
    .map_err(|_| anyhow::anyhow!("reading the daemon's effective configuration timed out"))?
    .map_err(|_| {
        anyhow::anyhow!("cannot read effective configuration; check the daemon and --socket")
    })?;
    config
        .validate()
        .map_err(|_| anyhow::anyhow!("daemon returned invalid effective configuration"))?;
    let program = config.programs.copilot_cli.as_ref()
        .filter(|program| program.use_llm_gateway)
        .context("enable programs.copilotCli.useLlmGateway and set its model in the daemon configuration")?;
    let gateway = config
        .llm_gateway
        .as_ref()
        .context("Copilot CLI requires llmGateway")?;
    let authentication = gateway
        .authentication
        .as_ref()
        .context("Copilot CLI requires oidc or controllerJwt LLM gateway authentication")?;
    if let LlmGatewayAuthentication::ControllerJwt {
        allowed_client_ids, ..
    } = authentication
        && !allowed_client_ids.contains(CopilotCli::ID)
    {
        bail!(
            "llmGateway.authentication.allowedClientIds must explicitly include copilot-cli; existing policy was not changed"
        );
    }

    let executable = discovery::find_launcher()
        .context("GitHub Copilot CLI not found on PATH; install copilot 1.0.84 or newer")?;
    let version = match discovery::package_version(&executable, None) {
        Some(version) => version,
        None => binary_version(&executable).await?,
    };
    require_supported_version(&version)?;

    // Preflight is bounded and discarded, never installed in the child environment.
    drop(credential(&socket).await?);
    let helper = std::env::current_exe().context("locate the Agentdesktop credential helper")?;
    let registry = private_registry()?;
    let mut command = native_command(&executable, arguments)?;
    scrub_environment(&mut command, std::env::vars_os().map(|(key, _)| key));
    command
        .env("COPILOT_PROVIDER_BASE_URL", responses_base_url(gateway))
        .env("COPILOT_PROVIDER_TYPE", "openai")
        .env("COPILOT_MODEL", &program.model)
        .env("COPILOT_PROVIDER_WIRE_API", program.wire_api.as_str())
        .env("COPILOT_PROVIDER_TRANSPORT", "http")
        .env(
            "COPILOT_PROVIDER_API_KEY_COMMAND",
            credential_command(&helper, &socket)?,
        )
        .env(
            "COPILOT_PROVIDERS_CONFIG",
            registry.path().join("providers.json"),
        )
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let result = run_child(&mut command).await;
    // Keep the empty registry alive until native exit, without changing COPILOT_HOME.
    drop(registry);
    result
}

fn require_supported_version(version: &Version) -> anyhow::Result<()> {
    if version < &MIN_VERSION || !version.pre.is_empty() {
        bail!(
            "GitHub Copilot CLI {version} is unsupported; install a stable version 1.0.84 or newer for per-request gateway credentials"
        );
    }
    Ok(())
}

async fn binary_version(executable: &Path) -> anyhow::Result<Version> {
    let mut command = native_command(executable, &["--binary-version".into()])?;
    scrub_environment(&mut command, std::env::vars_os().map(|(key, _)| key));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("probe Copilot with --binary-version (requires CLI 1.0.84+)")?;
    let result = timeout(VERSION_TIMEOUT, async {
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .context("capture Copilot binary version")?
            .take(4097)
            .read_to_end(&mut output)
            .await?;
        if output.len() > 4096 {
            bail!("Copilot binary version response is too large");
        }
        if !child.wait().await?.success() {
            bail!("Copilot --binary-version failed; install CLI 1.0.84 or newer");
        }
        let text = std::str::from_utf8(&output).unwrap_or("").trim();
        // Do not include untrusted probe output in diagnostics.
        Version::parse(
            text.strip_prefix("Copilot binary version: ")
                .unwrap_or(text),
        )
        .map_err(|_| {
            anyhow::anyhow!("cannot determine Copilot binary version; install CLI 1.0.84 or newer")
        })
    })
    .await;
    result.map_err(|_| {
        anyhow::anyhow!("Copilot --binary-version timed out; install CLI 1.0.84 or newer")
    })?
}

fn private_registry() -> anyhow::Result<TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("agentdesktop-copilot-")
        .tempdir()
        .context("create temporary Copilot provider registry")?;
    secure_fs::ensure_private_dir(directory.path())?;
    secure_fs::atomic_write(
        &directory.path().join("providers.json"),
        EMPTY_REGISTRY,
        0o600,
    )?;
    Ok(directory)
}

fn credential_command(helper: &Path, socket: &Path) -> anyhow::Result<String> {
    helper
        .to_str()
        .context("credential helper path must be valid UTF-8")?;
    let socket = socket
        .to_str()
        .context("daemon socket path must be valid UTF-8")?;
    Ok(render_command(&CommandSpec::new(
        helper,
        [
            "--socket",
            socket,
            "credential",
            "--client-id",
            CopilotCli::ID,
        ],
    )))
}

fn scrub_environment(command: &mut Command, keys: impl IntoIterator<Item = OsString>) {
    for key in keys {
        if overrides_routing_or_credentials(&key) {
            command.env_remove(key);
        }
    }
}

fn overrides_routing_or_credentials(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    key.starts_with("COPILOT_PROVIDER")
        || key.starts_with("COPILOT_MODEL")
        || key.starts_with("COPILOT_BYOK_")
        || key.starts_with("COPILOT_API_")
        || matches!(
            key.as_str(),
            "API_KEY"
                | "API_KEY_COMMAND"
                | "BEARER"
                | "AUTHORIZATION"
                | "HEADERS"
                | "GITHUB_COPILOT_API_TOKEN"
                | "OPENAI_API_KEY"
                | "OPENAI_API_KEY_COMMAND"
                | "OPENAI_BASE_URL"
                | "OPENAI_API_BASE"
                | "ANTHROPIC_API_KEY"
                | "ANTHROPIC_AUTH_TOKEN"
                | "ANTHROPIC_CUSTOM_HEADERS"
                | "ANTHROPIC_BASE_URL"
                | "AZURE_OPENAI_API_KEY"
                | "AZURE_OPENAI_ENDPOINT"
                | "COPILOT_BASE_URL"
                | "COPILOT_TRANSPORT"
        )
}

fn native_command(executable: &Path, arguments: &[OsString]) -> anyhow::Result<Command> {
    #[cfg(windows)]
    if executable.extension().is_some_and(|extension| {
        extension.to_str().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    }) {
        return npm_command(executable, arguments);
    }
    let mut command = Command::new(executable);
    command.args(arguments);
    Ok(command)
}

#[cfg(any(windows, test))]
fn npm_command(executable: &Path, arguments: &[OsString]) -> anyhow::Result<Command> {
    let script = discovery::npm_entrypoint(executable)
        .context("cannot resolve the Copilot npm shim; install the official @github/copilot package or native copilot.exe")?;
    let sibling_node = executable
        .parent()
        .context("npm shim has no parent")?
        .join("node.exe");
    let node = if sibling_node.is_file() {
        Some(sibling_node)
    } else {
        crate::provider::metadata::find_in_path("node.exe")
    }
    .context("node.exe is required for the Copilot npm launcher")?;
    let mut command = Command::new(node);
    command.arg(script).args(arguments);
    Ok(command)
}

async fn run_child(command: &mut Command) -> anyhow::Result<ExitStatus> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut child = command.spawn().context("launch GitHub Copilot CLI")?;
        loop {
            let signal = tokio::select! {
                result = child.wait() => return result.context("wait for GitHub Copilot CLI"),
                _ = interrupt.recv() => libc::SIGINT,
                _ = terminate.recv() => libc::SIGTERM,
                _ = hangup.recv() => libc::SIGHUP,
            };
            if let Some(pid) = child.id() {
                // SAFETY: the PID belongs to our live child. It retains the
                // foreground process group and inherited terminal streams.
                unsafe {
                    libc::kill(pid as libc::pid_t, signal);
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let mut child = command.spawn().context("launch GitHub Copilot CLI")?;
        loop {
            tokio::select! {
                result = child.wait() => return result.context("wait for GitHub Copilot CLI"),
                result = tokio::signal::ctrl_c() => {
                    result?;
                    // The native child shares the console and receives Ctrl-C too.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
