use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use agentdesktop_core::config::{LlmGatewayConfig, VsCodeConfig};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::warn;

use super::{VsCode, discovery};
use crate::provider::json_merge;
use crate::reconcile::ReconcilePlan;

/// What the plan report calls the managed part of the file.
const DESCRIPTION: &str = "chat language models";
/// The file carries the pairing value, so a file we write is owner-only.
const FILE_MODE: u32 = 0o600;

/// `chatLanguageModels.json` inside the VS Code user profile directory for
/// `home` (the per-OS root VS Code itself reads, shared with MCP discovery).
pub(super) fn chat_models_path(home: &Path) -> PathBuf {
    discovery::user_profile_root(home).join("chatLanguageModels.json")
}

/// The JSON root ARRAY merged into `chatLanguageModels.json`: one vendor
/// entry named `agentdesktop` (VS Code's "Custom Endpoint" Copilot Chat
/// provider) pointed at the loopback proxy's `/vscode-copilot` route, with
/// one model entry per configured model. No secret `apiKey` is written: VS
/// Code's schema requires the field, but the pairing travels in
/// `requestHeaders` instead.
pub(super) fn managed_document(
    config: &VsCodeConfig,
    listen: SocketAddr,
    pairing: &str,
) -> anyhow::Result<Value> {
    let models: Vec<Value> = config
        .model_documents()
        .into_iter()
        .map(|mut model| {
            if let Some(object) = model.as_object_mut() {
                object.insert(
                    "url".to_owned(),
                    Value::String(format!(
                        "http://{listen}/vscode-copilot/v1/chat/completions"
                    )),
                );
                object.insert(
                    "requestHeaders".to_owned(),
                    json!({ crate::llm_proxy::PAIRING_HEADER: pairing }),
                );
            }
            model
        })
        .collect();
    Ok(json!([{
        "name": VsCodeConfig::VENDOR_NAME,
        "vendor": "customendpoint",
        "apiKey": "unused",
        "apiType": "chat-completions",
        "models": models,
    }]))
}

/// The single array agentdesktop manages by key: vendor entries by `name`, at
/// the document root (`field: ""`). Unlike the Copilot CLI's `providers.json`,
/// the vendor's `models` sub-array is not separately keyed: the whole vendor
/// entry is owned and replaced wholesale on every apply.
const KEYED: &[json_merge::KeyedArray] = &[json_merge::KeyedArray {
    field: "",
    keys: &["name"],
}];

fn options() -> json_merge::MergeOptions {
    json_merge::MergeOptions {
        mode: FILE_MODE,
        keyed_arrays: KEYED,
        // The file holds the pairing.
        redact_diff: true,
    }
}

/// Plans the create/update/remove/conflict for `chatLanguageModels.json`.
///
/// With the program absent, the gateway off for it (`useLlmGateway: false` or
/// no top-level `llmGateway`), or no loopback proxy, the managed vendor entry
/// is removed and the user's own entries stay. Otherwise the managed document
/// is merged in. The vendor entry named `agentdesktop` belongs to agentdesktop
/// once it has written the file; before that, a user vendor entry under that
/// name is a conflict, unless it carries a pairing header on one of its
/// models (then it was written by an agentdesktop daemon whose sidecar is
/// gone, and it is replaced).
pub(super) fn plan(
    path: &Path,
    proxy: Option<(SocketAddr, &str)>,
    configured: Option<(&VsCodeConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let state_path = json_merge::state_path(path);
    let pairing = proxy.map(|(_, pairing)| pairing);
    let Some((config, Some(_gateway))) = configured else {
        remove(path, &state_path, pairing, plan)?;
        return Ok(());
    };
    let Some((listen, pairing)) = proxy else {
        warn!(
            path = %path.display(),
            "programs.vscode is configured but the local LLM proxy is not available, so VS Code is not pointed at the gateway; the reason is llmProxy.error in daemon-info (or daemon.llmProxy.listen is unset); removing the managed chat language models"
        );
        plan.inactive(crate::reconcile::PROXY_ABSENT_REASON);
        remove(path, &state_path, None, plan)?;
        return Ok(());
    };
    let managed = managed_document(config, listen, pairing)?;
    if let Some(reason) = foreign_managed_entry(path, &state_path, plan)? {
        warn!(path = %path.display(), reason, "refusing to change the VS Code chat language models file");
        plan.record(
            VsCode::DISPLAY_NAME,
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
        plan.ensure_private_dir(parent);
    }
    json_merge::plan_merge_with(
        path,
        &state_path,
        managed,
        false,
        DESCRIPTION,
        VsCode::DISPLAY_NAME,
        options(),
        plan,
    )
}

/// Managed entries are removed through the sidecar when there is one, and by
/// this daemon's pairing value when there is none.
fn remove(
    path: &Path,
    state_path: &Path,
    pairing: Option<&str>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    if !json_merge::plan_remove_with(
        path,
        state_path,
        DESCRIPTION,
        VsCode::DISPLAY_NAME,
        options(),
        plan,
    )? && let Some(pairing) = pairing
    {
        plan_remove_orphaned(path, pairing, plan)?;
    }
    Ok(())
}

/// Whether a vendor entry carries the pairing header on one of its models,
/// which only an agentdesktop daemon writes. With `Some(value)`, only that
/// pairing value counts.
fn carries_pairing(vendor: &Value, value: Option<&str>) -> bool {
    vendor["models"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|model| {
            let header = &model["requestHeaders"][crate::llm_proxy::PAIRING_HEADER];
            match value {
                Some(value) => header == value,
                None => !header.is_null(),
            }
        })
}

/// Before agentdesktop has written the file (no sidecar), a vendor entry named
/// `agentdesktop` without a pairing header on any model is the user's.
fn foreign_managed_entry(
    path: &Path,
    state_path: &Path,
    plan: &ReconcilePlan,
) -> anyhow::Result<Option<String>> {
    if json_merge::managed_after(state_path, VsCode::DISPLAY_NAME, plan)?.is_some() {
        return Ok(None);
    }
    let Some(current) = read_array(path, plan)? else {
        return Ok(None);
    };
    for vendor in &current {
        if vendor["name"] == VsCodeConfig::VENDOR_NAME && !carries_pairing(vendor, None) {
            return Ok(Some(format!(
                "vendor entry {} exists but was not written by agentdesktop",
                VsCodeConfig::VENDOR_NAME
            )));
        }
    }
    Ok(None)
}

/// Removal without a sidecar: the vendor entry named `agentdesktop` whose
/// models carry this daemon's own pairing value was written for this daemon,
/// so it is taken out; the file keeps its mode and is never deleted here. A
/// file that cannot be read is skipped with a warning.
fn plan_remove_orphaned(path: &Path, pairing: &str, plan: &ReconcilePlan) -> anyhow::Result<()> {
    let current = match read_array(path, plan) {
        Ok(Some(current)) => current,
        Ok(None) => return Ok(()),
        Err(error) => {
            warn!(error = %format!("{error:#}"), "skipping the VS Code chat language models file");
            return Ok(());
        }
    };
    let remaining: Vec<Value> = current
        .iter()
        .filter(|vendor| {
            !(vendor["name"] == VsCodeConfig::VENDOR_NAME && carries_pairing(vendor, Some(pairing)))
        })
        .cloned()
        .collect();
    if remaining.len() == current.len() {
        return Ok(());
    }
    let existing = plan.read(path)?;
    let mut contents = serde_json::to_vec_pretty(&Value::Array(remaining))
        .with_context(|| format!("serialize {} {DESCRIPTION}", VsCode::DISPLAY_NAME))?;
    contents.push(b'\n');
    if contents == existing {
        return Ok(());
    }
    plan.record_diff(
        VsCode::DISPLAY_NAME,
        DESCRIPTION,
        "update",
        path,
        None,
        None,
    );
    let mode = json_merge::current_mode(path).unwrap_or(FILE_MODE);
    plan.write_file(path, &contents, mode)
}

fn read_array(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<Option<Vec<Value>>> {
    let existing = match plan.read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read {} {DESCRIPTION} from {}",
                    VsCode::DISPLAY_NAME,
                    path.display()
                )
            });
        }
    };
    // Anything that is not a JSON array is left to the merge, which reports it.
    Ok(match serde_json::from_slice::<Value>(&existing) {
        Ok(Value::Array(array)) => Some(array),
        _ => None,
    })
}
