use super::{DISPLAY_NAME, PROVIDER_ANTHROPIC, PROVIDER_OPENAI};

use std::{ffi::OsString, net::SocketAddr, path::Path, path::PathBuf};

use agentdesktop_core::config::{CopilotConfig, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::warn;

use crate::provider::json_merge;
use crate::reconcile::ReconcilePlan;

/// What the plan report calls the managed part of the file.
const DESCRIPTION: &str = "providers";
/// The file carries the pairing value, so a file we write is owner-only.
const FILE_MODE: u32 = 0o600;

/// Resolves the `providers.json` path: `COPILOT_PROVIDERS_CONFIG` if set, else
/// `$COPILOT_HOME/providers.json` if `COPILOT_HOME` is set, else
/// `<home>/.copilot/providers.json` with home read from `HOME`, then
/// `USERPROFILE`. Empty values count as unset. Errors when no home resolves.
pub(crate) fn providers_path(env: &dyn Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    let get = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(explicit) = get("COPILOT_PROVIDERS_CONFIG") {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(copilot_home) = get("COPILOT_HOME") {
        return Ok(PathBuf::from(copilot_home).join("providers.json"));
    }
    let home = get("HOME")
        .or_else(|| get("USERPROFILE"))
        .context("the GitHub Copilot CLI providers file needs HOME, USERPROFILE or COPILOT_HOME")?;
    Ok(PathBuf::from(home).join(".copilot").join("providers.json"))
}

/// The JSON object merged into `providers.json`: two provider entries pointed
/// at the loopback proxy's `/copilot-cli` route with the pairing header, plus
/// one model entry per configured model. No `apiKey` is written: the proxy
/// adds the gateway credential itself.
pub(super) fn managed_document(
    config: &CopilotConfig,
    listen: SocketAddr,
    pairing: &str,
) -> anyhow::Result<Value> {
    let headers = json!({ crate::llm_proxy::PAIRING_HEADER: pairing });
    Ok(json!({
        "providers": [
            {
                "name": PROVIDER_OPENAI,
                "type": "openai",
                "baseUrl": format!("http://{listen}/copilot-cli/v1"),
                "headers": headers,
            },
            {
                "name": PROVIDER_ANTHROPIC,
                "type": "anthropic",
                "baseUrl": format!("http://{listen}/copilot-cli"),
                "headers": headers,
            },
        ],
        "models": config.model_documents(),
    }))
}

/// The two arrays agentdesktop manages by key: providers by `name`, models by
/// `provider` and `id` (the CLI addresses a model as `<provider>/<id>`).
const KEYED: &[json_merge::KeyedArray] = &[
    json_merge::KeyedArray {
        field: "providers",
        keys: &["name"],
    },
    json_merge::KeyedArray {
        field: "models",
        keys: &["provider", "id"],
    },
];

fn options() -> json_merge::MergeOptions {
    json_merge::MergeOptions {
        mode: FILE_MODE,
        keyed_arrays: KEYED,
        // The file holds the pairing and possibly the user's own API keys.
        redact_diff: true,
    }
}

/// Plans the create/update/remove/conflict for `providers.json`.
///
/// With the program absent, the gateway off for it (`useLlmGateway: false` or
/// no top-level `llmGateway`), or no loopback proxy, the managed entries are
/// removed and the user's own entries stay. Otherwise the managed document is
/// merged in. Entries under the managed keys (provider names `agentdesktop`
/// and `agentdesktop-anthropic`, models under those providers) belong to
/// agentdesktop once it has written the file; before that, a user entry under
/// one of those keys is a conflict, unless it carries a pairing header (then
/// it was written by an agentdesktop daemon whose sidecar is gone, and it is
/// replaced).
pub(super) fn plan(
    path: &Path,
    proxy: Option<(SocketAddr, &str)>,
    configured: Option<(&CopilotConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let state_path = json_merge::state_path(path);
    let pairing = proxy.map(|(_, pairing)| pairing);
    let Some((config, Some(gateway))) = configured else {
        remove(path, &state_path, pairing, plan)?;
        return Ok(());
    };
    let Some((listen, pairing)) = proxy else {
        if crate::reconcile::fail_closed_without_proxy(gateway, plan) {
            tracing::debug!(
                path = %path.display(),
                "programs.copilot is configured but the local LLM proxy is not available; whenProxyUnavailable: failClosed, so the managed Copilot CLI providers are left as they are"
            );
            return Ok(());
        }
        tracing::debug!(
            path = %path.display(),
            "programs.copilot is configured but the local LLM proxy is not available, so the Copilot CLI is not pointed at the gateway; the reason is llmProxy.error in daemon-info (or daemon.llmProxy.listen is unset); whenProxyUnavailable: failOpen, removing the managed Copilot CLI providers"
        );
        plan.inactive(crate::reconcile::PROXY_ABSENT_REASON);
        remove(path, &state_path, None, plan)?;
        return Ok(());
    };
    let managed = managed_document(config, listen, pairing)?;
    if let Some(reason) = foreign_managed_entry(path, &state_path, &managed, plan)? {
        warn!(path = %path.display(), reason, "refusing to change the Copilot CLI providers file");
        plan.record(
            DISPLAY_NAME,
            &format!("{DESCRIPTION} ({reason})"),
            "conflict",
            path,
        );
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && !parent.exists()
    {
        // A Copilot directory we create holds the pairing: owner-only, like the
        // file. An existing directory keeps the user's mode.
        plan.ensure_private_dir(parent);
    }
    json_merge::plan_merge_with(
        path,
        &state_path,
        managed,
        false,
        DESCRIPTION,
        DISPLAY_NAME,
        options(),
        plan,
    )
}

/// Managed entries are removed through the sidecar when there is one, and by
/// their pairing header when there is none.
fn remove(
    path: &Path,
    state_path: &Path,
    pairing: Option<&str>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    if !json_merge::plan_remove_with(path, state_path, DESCRIPTION, DISPLAY_NAME, options(), plan)?
        && let Some(pairing) = pairing
    {
        plan_remove_orphaned(path, pairing, plan)?;
    }
    Ok(())
}

/// Before agentdesktop has written the file (no sidecar), an entry under a
/// managed key is the user's: a provider with one of the managed names that
/// carries no pairing header, or a model under a managed provider name whose
/// provider entry was not written by an agentdesktop daemon (absent, or
/// without a pairing header). Once the sidecar exists, the managed keys are
/// agentdesktop's and their entries are replaced in place.
fn foreign_managed_entry(
    path: &Path,
    state_path: &Path,
    managed: &Value,
    plan: &ReconcilePlan,
) -> anyhow::Result<Option<String>> {
    if json_merge::managed_after(state_path, DISPLAY_NAME, plan)?.is_some() {
        return Ok(None);
    }
    let Some(current) = read_object(path, plan)? else {
        return Ok(None);
    };
    let managed_providers: Vec<&str> = managed["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|provider| provider["name"].as_str())
        .collect();
    let written_by_agentdesktop: Vec<&str> = providers(&current)
        .filter(|provider| !provider["headers"][crate::llm_proxy::PAIRING_HEADER].is_null())
        .filter_map(|provider| provider["name"].as_str())
        .collect();
    for provider in providers(&current) {
        let Some(name) = provider["name"].as_str() else {
            continue;
        };
        if managed_providers.contains(&name) && !written_by_agentdesktop.contains(&name) {
            return Ok(Some(format!(
                "provider {name} exists but was not written by agentdesktop"
            )));
        }
    }
    for model in current
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(provider), Some(id)) = (model["provider"].as_str(), model["id"].as_str())
            && managed_providers.contains(&provider)
            && !written_by_agentdesktop.contains(&provider)
        {
            return Ok(Some(format!(
                "model {provider}/{id} exists under a managed provider name that was not written by agentdesktop"
            )));
        }
    }
    Ok(None)
}

/// Removal without a sidecar: entries under the managed names that carry
/// this daemon's own pairing value were written for this daemon (before its
/// sidecar was deleted), so they and the models under them are taken out by
/// key; the file keeps its mode and is never deleted here. Entries with
/// another pairing value, or none, are left alone, and a file that cannot be
/// read is skipped with a warning rather than failing the whole apply.
fn plan_remove_orphaned(path: &Path, pairing: &str, plan: &ReconcilePlan) -> anyhow::Result<()> {
    let current = match read_object(path, plan) {
        Ok(Some(current)) => current,
        Ok(None) => return Ok(()),
        Err(error) => {
            warn!(error = %format!("{error:#}"), "skipping the Copilot CLI providers file");
            return Ok(());
        }
    };
    let orphaned: Vec<String> = providers(&current)
        .filter(|provider| provider["headers"][crate::llm_proxy::PAIRING_HEADER] == pairing)
        .filter_map(|provider| provider["name"].as_str().map(str::to_owned))
        .filter(|name| name == PROVIDER_OPENAI || name == PROVIDER_ANTHROPIC)
        .collect();
    if orphaned.is_empty() {
        return Ok(());
    }
    let mut remaining = Value::Object(current);
    // get_mut: indexing a mutable Value inserts a missing key as null.
    if let Some(entries) = remaining.get_mut("providers").and_then(Value::as_array_mut) {
        entries.retain(|provider| {
            !provider["name"]
                .as_str()
                .is_some_and(|name| orphaned.iter().any(|own| own == name))
        });
    }
    if let Some(entries) = remaining.get_mut("models").and_then(Value::as_array_mut) {
        entries.retain(|model| {
            !model["provider"]
                .as_str()
                .is_some_and(|name| orphaned.iter().any(|own| own == name))
        });
    }
    let existing = plan.read(path)?;
    let mut contents = serde_json::to_vec_pretty(&remaining)
        .with_context(|| format!("serialize {DISPLAY_NAME} providers"))?;
    contents.push(b'\n');
    if contents == existing {
        return Ok(());
    }
    plan.record_diff(DISPLAY_NAME, DESCRIPTION, "update", path, None, None);
    let mode = json_merge::current_mode(path).unwrap_or(FILE_MODE);
    plan.write_file(path, &contents, mode)
}

fn read_object(
    path: &Path,
    plan: &ReconcilePlan,
) -> anyhow::Result<Option<serde_json::Map<String, Value>>> {
    let existing = match plan.read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {DISPLAY_NAME} providers from {}", path.display()));
        }
    };
    // Anything that is not a JSON object is left to the merge, which reports it.
    Ok(match serde_json::from_slice::<Value>(&existing) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    })
}

fn providers(current: &serde_json::Map<String, Value>) -> impl Iterator<Item = &Value> {
    current
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}
