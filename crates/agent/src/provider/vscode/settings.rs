//! VS Code user `settings.json` management for the `githubModels` variant of
//! `copilotChat`: points Copilot Chat's CAPI endpoint
//! (`github.copilot.advanced.debug.overrideCapiUrl`) at the loopback proxy's
//! `/vscode-copilot-capi/<pairing>` route, so VS Code keeps GitHub's own
//! models and its own Copilot token while the daemon adds the gateway
//! identity.
//!
//! The file is VS Code's JSONC, commonly commented and hand-edited, so it is
//! edited in place with a lossless syntax tree (`jsonc-parser`): only the
//! override and the two `settingsSync.ignoredSettings` entries change, and
//! comments, key order, indentation, line endings and a leading byte-order
//! mark stay as the user wrote them (a single-line object is expanded to one
//! property per line). The sidecar records only what removal must restore.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use agentdesktop_core::config::{LlmGatewayConfig, VsCodeConfig, VsCodeCopilotChat};
use anyhow::Context;
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use super::{VsCode, discovery};
use crate::provider::json_merge;
use crate::reconcile::ReconcilePlan;

/// The setting VS Code's Copilot Chat reads for its CAPI base URL (the legacy
/// name; `github.copilot.internal.capiUrl` is the newer alias).
pub(super) const OVERRIDE_KEY: &str = "github.copilot.advanced.debug.overrideCapiUrl";
const CAPI_ALIAS_KEY: &str = "github.copilot.internal.capiUrl";
const IGNORED_SETTINGS_KEY: &str = "settingsSync.ignoredSettings";
/// What the plan report calls the managed part of the file.
const DESCRIPTION: &str = "settings";
/// The file carries the pairing value, so a file we write is owner-only.
const FILE_MODE: u32 = 0o600;

/// The user's `settings.json` for the resolved home directory (per-OS VS Code
/// user profile root, shared with `chatLanguageModels.json` and MCP
/// discovery).
pub(super) fn settings_path(home: &Path) -> PathBuf {
    discovery::user_profile_root(home).join("settings.json")
}

/// The override URL for a listener and pairing: the pairing is the first path
/// segment of the `/vscode-copilot-capi` route, because VS Code cannot add a
/// header to these requests.
pub(super) fn override_url(listen: SocketAddr, pairing: &str) -> String {
    format!("http://{listen}{}/{pairing}", crate::llm_proxy::CAPI_ROUTE)
}

/// Why `edit_settings`/`remove_settings` refuse to touch the file:
/// anything beyond VS Code's own JSONC (comments, trailing commas), a
/// non-object root, `settingsSync.ignoredSettings` present and not an array,
/// or the override key or `settingsSync.ignoredSettings` appearing more than
/// once at the root (VS Code would read a different copy than the one
/// edited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsConflict {
    Parse,
    NotAnObject,
    IgnoredNotArray,
    DuplicateKey,
}

impl SettingsConflict {
    fn reason(self) -> &'static str {
        match self {
            Self::Parse => {
                "not valid VS Code JSONC (only comments and trailing commas are accepted beyond JSON)"
            }
            Self::NotAnObject => "the top level is not an object",
            Self::IgnoredNotArray => "settingsSync.ignoredSettings is not an array",
            Self::DuplicateKey => "a managed key appears more than once",
        }
    }
}

/// The sidecar: what removal must restore, without a copy of the user's
/// settings. `override_before` keeps an explicit JSON `null` as
/// `Some(Value::Null)`, distinct from the key being absent (`None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SettingsState {
    pub version: u32,
    pub created: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "some_or_explicit_null"
    )]
    pub override_before: Option<Value>,
    pub added_ignored: Vec<String>,
    pub ignored_created: bool,
}

/// A present-but-`null` field deserializes to `Some(Value::Null)`; an absent
/// field is left at its `#[serde(default)]` (`None`) by serde before this
/// function is ever called.
fn some_or_explicit_null<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// The sidecar version `edit_settings` writes; a sidecar without a `version`
/// (the whole-document `json_merge` state an earlier daemon wrote) is
/// upgraded by `read_state`.
pub(super) const SETTINGS_STATE_VERSION: u32 = 2;

/// The outcome of `remove_settings`: nothing to change, rewritten text, or
/// the whole file going away (a file this daemon created that is empty once
/// the managed keys are taken out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Removal {
    Unchanged,
    Write(String),
    Delete,
}

/// VS Code's JSONC: comments and trailing commas, nothing else (VS Code
/// flags the other JSON5 forms as errors, so a file using them is mid-edit or
/// broken and is left alone).
fn vscode_jsonc() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

/// Parses the text and returns the root (which must outlive every node taken
/// from it) and its object, checking the shape the edit relies on.
fn parse_object(text: &str) -> Result<(CstRootNode, CstObject), SettingsConflict> {
    let root = CstRootNode::parse(text, &vscode_jsonc()).map_err(|_| SettingsConflict::Parse)?;
    let object = root
        .object_value_or_create()
        .ok_or(SettingsConflict::NotAnObject)?;
    for key in [OVERRIDE_KEY, IGNORED_SETTINGS_KEY] {
        let count = object
            .properties()
            .iter()
            .filter(|property| property.decoded_name().as_deref() == Some(key))
            .count();
        if count > 1 {
            return Err(SettingsConflict::DuplicateKey);
        }
    }
    if object.get(IGNORED_SETTINGS_KEY).is_some()
        && object.array_value(IGNORED_SETTINGS_KEY).is_none()
    {
        return Err(SettingsConflict::IgnoredNotArray);
    }
    Ok((root, object))
}

/// A `serde_json::Value` as a syntax-tree input value (`jsonc-parser` has no
/// conversion of its own); numbers keep their JSON text.
fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input_value).collect()),
        Value::Object(fields) => CstInputValue::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), input_value(value)))
                .collect(),
        ),
    }
}

fn is_string(node: &jsonc_parser::cst::CstNode, text: &str) -> bool {
    node.to_serde_value().as_ref().and_then(Value::as_str) == Some(text)
}

/// In-place edit of `settings.json`: `text: None` is an absent file (created
/// as `{}` plus our keys); an empty or whitespace-only file is treated as
/// `{}`. Sets the override to `url` (in place when the key exists, appended
/// otherwise) and makes `settingsSync.ignoredSettings` hold both
/// `OVERRIDE_KEY` and `CAPI_ALIAS_KEY`, creating the array when absent and
/// appending only the missing entries. `state` is the previous sidecar
/// (`None` on a first apply or when there is none); the returned
/// state is what removal needs afterwards.
pub(super) fn edit_settings(
    text: Option<&str>,
    url: &str,
    state: Option<&SettingsState>,
) -> Result<(String, SettingsState), SettingsConflict> {
    // A UTF-8 byte-order mark (Windows PowerShell 5.1 writes one) is kept as
    // it is and edited around.
    let (bom, text) = match text {
        Some(text) => {
            let (bom, rest) = split_bom(text);
            (bom, Some(rest))
        }
        None => ("", None),
    };
    let source = match text {
        Some(text) if !text.trim().is_empty() => text,
        _ => "{}\n",
    };
    let (root, object) = parse_object(source)?;

    let existing = object.get(OVERRIDE_KEY);
    let existing_value = existing
        .as_ref()
        .and_then(|property| property.value())
        .and_then(|value| value.to_serde_value());
    // New properties go at the top of the object: appending after the last
    // one would add a comma to the user's last line, inserting first leaves
    // every line the user wrote as it was.
    let mut inserted = 0;
    match &existing {
        Some(property) => property.set_value(CstInputValue::String(url.to_owned())),
        None => {
            object.insert(
                inserted,
                OVERRIDE_KEY,
                CstInputValue::String(url.to_owned()),
            );
            inserted += 1;
        }
    }

    let ignored_created_now = object.get(IGNORED_SETTINGS_KEY).is_none();
    let mut appended = Vec::new();
    if ignored_created_now {
        object.insert(
            inserted,
            IGNORED_SETTINGS_KEY,
            CstInputValue::Array(
                [OVERRIDE_KEY, CAPI_ALIAS_KEY]
                    .map(|entry| CstInputValue::String(entry.to_owned()))
                    .to_vec(),
            ),
        );
        appended.extend([OVERRIDE_KEY, CAPI_ALIAS_KEY].map(str::to_owned));
    } else {
        // `parse_object` has already checked that the property is an array.
        let ignored = object
            .array_value(IGNORED_SETTINGS_KEY)
            .ok_or(SettingsConflict::IgnoredNotArray)?;
        for entry in [OVERRIDE_KEY, CAPI_ALIAS_KEY] {
            if !ignored.elements().iter().any(|node| is_string(node, entry)) {
                ignored.append(CstInputValue::String(entry.to_owned()));
                appended.push(entry.to_owned());
            }
        }
    }
    let only_ours = object
        .array_value(IGNORED_SETTINGS_KEY)
        .is_some_and(|ignored| {
            ignored
                .elements()
                .iter()
                .all(|node| is_string(node, OVERRIDE_KEY) || is_string(node, CAPI_ALIAS_KEY))
        });

    let new_state = match state {
        // The first apply's record stays: the value the user had, whether the
        // daemon created the file and the array. A hand edit of the override
        // since then is drift, never the user's value.
        Some(previous) => {
            let mut added = previous.added_ignored.clone();
            for entry in appended {
                if !added.contains(&entry) {
                    added.push(entry);
                }
            }
            SettingsState {
                version: SETTINGS_STATE_VERSION,
                // A file or list the user deleted and this apply recreated is
                // the daemon's too.
                created: previous.created || text.is_none(),
                override_before: previous.override_before.clone(),
                added_ignored: added,
                ignored_created: previous.ignored_created || ignored_created_now,
            }
        }
        None => {
            // Without a record, an override equal to our own URL or to another
            // agentdesktop loopback CAPI URL was written by a daemon: it is not the user's value, our entries
            // next to it are ours too, and an ignore list holding nothing but
            // them was created by it.
            let ours = existing_value
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|value| value == url || is_daemon_capi_url(value));
            let added_ignored = if ours {
                [OVERRIDE_KEY, CAPI_ALIAS_KEY].map(str::to_owned).to_vec()
            } else {
                appended
            };
            SettingsState {
                version: SETTINGS_STATE_VERSION,
                created: text.is_none(),
                override_before: if ours { None } else { existing_value },
                added_ignored,
                ignored_created: ignored_created_now || (ours && only_ours),
            }
        }
    };
    let edited = format!("{bom}{root}");
    Ok((edited, new_state))
}

fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    }
}

/// Whether an override value points at an agentdesktop loopback CAPI route
/// (`http://<loopback>:<port>/vscode-copilot-capi/<pairing>`): the daemon's
/// own URL, or one an earlier listen address or pairing left behind. Such a
/// value is never the user's own setting.
fn is_daemon_capi_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("http://") else {
        return false;
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let loopback = authority
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback())
        || authority
            .rsplit_once(':')
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost"));
    loopback
        && path
            .strip_prefix(crate::llm_proxy::CAPI_ROUTE)
            .is_some_and(|tail| tail.starts_with('/') && tail.len() > 1)
}

/// In-place removal of the managed keys. With a sidecar: the override is
/// restored to `override_before` (in place, keeping a same-line comment;
/// appended when the user deleted the property meanwhile; removed when there
/// was none), `added_ignored` entries are taken out, and the array property
/// goes when the daemon created it and it is now empty. Without a sidecar,
/// an override equal to `own_url` or to any agentdesktop loopback CAPI URL,
/// and our two entries, are removed by value; an unparseable file is left
/// alone.
pub(super) fn remove_settings(
    text: &str,
    state: Option<&SettingsState>,
    own_url: Option<&str>,
) -> Result<Removal, SettingsConflict> {
    let original = text;
    let (bom, text) = split_bom(text);
    if text.trim().is_empty() {
        return Ok(match state {
            Some(state) if state.created => Removal::Delete,
            _ => Removal::Unchanged,
        });
    }
    let (root, object) = match parse_object(text) {
        Ok(parsed) => parsed,
        Err(conflict) if state.is_some() => return Err(conflict),
        Err(_) => {
            debug!(
                "VS Code settings file is not valid VS Code JSONC; leaving it alone (no sidecar, nothing known to remove)"
            );
            return Ok(Removal::Unchanged);
        }
    };

    // What the override goes back to (`None`: removed), which entries come
    // out of the ignore list, and whether an emptied list goes too.
    let (restore, entries, drop_empty_array) = match state {
        Some(state) => (
            state.override_before.clone(),
            state.added_ignored.clone(),
            state.ignored_created,
        ),
        None => {
            let current = object
                .get(OVERRIDE_KEY)
                .and_then(|property| property.value())
                .and_then(|value| value.to_serde_value());
            let ours = current
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|value| own_url == Some(value) || is_daemon_capi_url(value));
            if !ours {
                return Ok(Removal::Unchanged);
            }
            (
                None,
                [OVERRIDE_KEY, CAPI_ALIAS_KEY].map(str::to_owned).to_vec(),
                true,
            )
        }
    };

    match (object.get(OVERRIDE_KEY), restore) {
        (Some(property), Some(value)) => property.set_value(input_value(&value)),
        (Some(property), None) => property.remove(),
        (None, Some(value)) => {
            object.append(OVERRIDE_KEY, input_value(&value));
        }
        (None, None) => {}
    }
    if let Some(ignored) = object.array_value(IGNORED_SETTINGS_KEY) {
        for node in ignored.elements() {
            if entries.iter().any(|entry| is_string(&node, entry)) {
                node.remove();
            }
        }
        if drop_empty_array
            && ignored.elements().is_empty()
            && let Some(property) = object.get(IGNORED_SETTINGS_KEY)
        {
            property.remove();
        }
    }

    let body = root.to_string();
    let remaining = format!("{bom}{body}");
    if state.is_some_and(|state| state.created)
        && body
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            == "{}"
    {
        return Ok(Removal::Delete);
    }
    Ok(if remaining == original {
        Removal::Unchanged
    } else {
        Removal::Write(remaining)
    })
}

/// The whole-document state an earlier daemon wrote through `json_merge`.
#[derive(Deserialize)]
struct LegacyMergeState {
    created: bool,
    before: Value,
}

/// Reads a sidecar: the current form, or the earlier whole-document form
/// upgraded; `None` when the bytes are neither (the caller fails the plan).
pub(super) fn read_state(bytes: &[u8]) -> Option<SettingsState> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version").is_some() {
        return serde_json::from_value::<SettingsState>(value)
            .ok()
            .filter(|state| state.version == SETTINGS_STATE_VERSION);
    }
    let legacy: LegacyMergeState = serde_json::from_value(value).ok()?;
    let before_ignored = legacy.before.get(IGNORED_SETTINGS_KEY);
    let held = |entry: &str| {
        before_ignored
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|value| value == entry))
    };
    Some(SettingsState {
        version: SETTINGS_STATE_VERSION,
        created: legacy.created,
        override_before: legacy.before.get(OVERRIDE_KEY).cloned(),
        added_ignored: [OVERRIDE_KEY, CAPI_ALIAS_KEY]
            .into_iter()
            .filter(|entry| !held(entry))
            .map(str::to_owned)
            .collect(),
        ignored_created: before_ignored.is_none(),
    })
}

/// Plans the create/update/remove/conflict for the user `settings.json`.
///
/// Edits only when the program is set with `copilotChat: githubModels`,
/// uses the gateway, the gateway has `proxyUrl` and the loopback proxy is
/// available; removes the managed keys otherwise, the same decision shape as
/// `reconcile::plan`. The one exception is the proxy being the missing part:
/// then `llmGateway.whenProxyUnavailable` decides, and `failClosed` (the default) leaves
/// the file as it is, so VS Code stays pointed at the loopback port
/// instead of reaching GitHub past the gateway.
pub(super) fn plan(
    path: &Path,
    proxy: Option<(SocketAddr, &str)>,
    configured: Option<(&VsCodeConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let state_path = json_merge::state_path(path);
    let active = matches!(
        configured,
        Some((config, Some(gateway)))
            if config.copilot_chat == VsCodeCopilotChat::GithubModels && gateway.proxy_url.is_some()
    );
    if !active {
        return remove(path, &state_path, proxy, plan);
    }
    let Some((listen, pairing)) = proxy else {
        if let Some((_, Some(gateway))) = configured
            && crate::reconcile::fail_closed_without_proxy(gateway, plan, || {
                override_in_place(path, plan)
            })
        {
            debug!(
                path = %path.display(),
                "programs.vscode uses copilotChat: githubModels but the local LLM proxy is not available; whenProxyUnavailable: failClosed, so settings.json is left as it is"
            );
            return Ok(());
        }
        tracing::debug!(
            path = %path.display(),
            "programs.vscode uses copilotChat: githubModels but the local LLM proxy is not available, so VS Code is not pointed at the gateway; the reason is llmProxy.error in daemon-info (or daemon.llmProxy.listen is unset); whenProxyUnavailable: failOpen, removing the managed settings"
        );
        plan.inactive(crate::reconcile::FAIL_OPEN_REASON);
        return remove(path, &state_path, None, plan);
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && !parent.exists()
    {
        plan.ensure_private_dir(parent);
    }

    let existing = read_optional(path, plan)?;
    let (sidecar, state) = read_sidecar(&state_path, plan)?;
    let text = match existing.as_deref().map(std::str::from_utf8) {
        Some(Ok(text)) => Some(text),
        Some(Err(_)) => return conflict(path, SettingsConflict::Parse, plan),
        None => None,
    };
    let (edited, new_state) =
        match edit_settings(text, &override_url(listen, pairing), state.as_ref()) {
            Ok(result) => result,
            Err(kind) => return conflict(path, kind, plan),
        };

    // An unchanged file is not rewritten, unless its mode lets others read
    // it (VS Code saves it 664): then the same bytes are written 0600.
    let looser = crate::reconcile::grants_beyond(path, FILE_MODE);
    let action = match text {
        None => "create",
        Some(text) if text == edited && !looser => "unchanged",
        Some(_) => "update",
    };
    // The sidecar is written first: if the apply stops between the two
    // writes, the record of the user's own value already exists.
    let mut state_bytes = serde_json::to_vec_pretty(&new_state)
        .with_context(|| format!("serialize {} {DESCRIPTION} state", VsCode::DISPLAY_NAME))?;
    state_bytes.push(b'\n');
    // An identical sidecar is rewritten only when its mode lets others read it.
    let looser_sidecar = crate::reconcile::grants_beyond(&state_path, FILE_MODE);
    if sidecar.as_deref() != Some(state_bytes.as_slice()) || looser_sidecar {
        plan.write_file(&state_path, &state_bytes, FILE_MODE)?;
    }
    if action == "unchanged" {
        plan.record(VsCode::DISPLAY_NAME, DESCRIPTION, action, path);
    } else {
        // Redacted: the file holds the pairing and possibly the user's own
        // secrets.
        plan.record_diff(VsCode::DISPLAY_NAME, DESCRIPTION, action, path, None, None);
        plan.write_file(path, edited.as_bytes(), FILE_MODE)?;
    }

    debug!(provider = VsCode::DISPLAY_NAME, action, path = %path.display(), "planned VS Code settings edit");
    Ok(())
}

/// Removes the managed keys: through the sidecar when there is one, by value
/// (an override pointing at an agentdesktop loopback CAPI route) when there
/// is none. The file keeps its mode. Like the other managed files, `unchanged`
/// is recorded only when there was something of ours to remove (a sidecar).
fn remove(
    path: &Path,
    state_path: &Path,
    proxy: Option<(SocketAddr, &str)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let (sidecar, state) = read_sidecar(state_path, plan)?;
    let existing = match read_optional(path, plan) {
        Ok(existing) => existing,
        Err(error) if state.is_none() => {
            warn!(error = %format!("{error:#}"), "skipping the VS Code settings file");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let own_url = proxy.map(|(listen, pairing)| override_url(listen, pairing));
    if let Some(existing) = existing {
        let removal = match std::str::from_utf8(&existing) {
            Ok(text) => remove_settings(text, state.as_ref(), own_url.as_deref()),
            Err(_) if state.is_some() => Err(SettingsConflict::Parse),
            Err(_) => {
                debug!(
                    path = %path.display(),
                    "VS Code settings file is not UTF-8; leaving it alone (no sidecar, nothing known to remove)"
                );
                Ok(Removal::Unchanged)
            }
        };
        match removal {
            Ok(Removal::Unchanged) => {
                if sidecar.is_some() {
                    plan.record(VsCode::DISPLAY_NAME, DESCRIPTION, "unchanged", path);
                }
            }
            Ok(Removal::Write(text)) => {
                plan.record_diff(
                    VsCode::DISPLAY_NAME,
                    DESCRIPTION,
                    "update",
                    path,
                    None,
                    None,
                );
                let mode = json_merge::current_mode(path).unwrap_or(FILE_MODE);
                plan.write_file(path, text.as_bytes(), mode)?;
            }
            Ok(Removal::Delete) => {
                plan.record(VsCode::DISPLAY_NAME, DESCRIPTION, "remove", path);
                plan.remove_file(path)
                    .with_context(|| format!("remove {}", path.display()))?;
            }
            Err(kind) => return conflict(path, kind, plan),
        }
    } else if sidecar.is_some() {
        plan.record(VsCode::DISPLAY_NAME, DESCRIPTION, "unchanged", path);
    }
    if sidecar.is_some() {
        plan.remove_file(state_path)
            .with_context(|| format!("remove {}", state_path.display()))?;
    }
    Ok(())
}

/// Whether the override points at an agentdesktop loopback CAPI route, so
/// VS Code goes through the daemon (and fails while the proxy is down). A
/// missing file is not in place; one that cannot be read or parsed is logged
/// and counts as not in place.
fn override_in_place(path: &Path, plan: &ReconcilePlan) -> bool {
    let bytes = match read_optional(path, plan) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return false,
        Err(error) => {
            warn!(error = %format!("{error:#}"), "reading the VS Code settings file");
            return false;
        }
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    let Ok((_root, object)) = parse_object(split_bom(text).1) else {
        return false;
    };
    object
        .get(OVERRIDE_KEY)
        .and_then(|property| property.value())
        .and_then(|value| value.to_serde_value())
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(is_daemon_capi_url)
}

fn conflict(path: &Path, kind: SettingsConflict, plan: &ReconcilePlan) -> anyhow::Result<()> {
    warn!(path = %path.display(), reason = kind.reason(), "refusing to change the VS Code settings file");
    plan.record(VsCode::DISPLAY_NAME, DESCRIPTION, "conflict", path);
    Ok(())
}

fn read_optional(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<Option<Vec<u8>>> {
    match plan.read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// The sidecar's bytes and its parsed state. A sidecar that is present but
/// not a settings state (after a leading byte-order mark, which a hand edit
/// on Windows can add) is an error, as is one that cannot be read at all
/// (permissions), as for any managed file.
fn read_sidecar(
    state_path: &Path,
    plan: &ReconcilePlan,
) -> anyhow::Result<(Option<Vec<u8>>, Option<SettingsState>)> {
    let bytes = read_optional(state_path, plan)?;
    let state = match bytes.as_deref() {
        Some(bytes) => {
            let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
            Some(read_state(bytes).ok_or_else(|| unreadable_sidecar(state_path, bytes))?)
        }
        None => None,
    };
    Ok((bytes, state))
}

/// A sidecar that is present but not a settings state fails the plan, as the
/// json_merge sidecars do: without it the user's own override cannot be
/// restored. The message names the position of a syntax error only, never a
/// value: the sidecar can hold the user's own override URL.
fn unreadable_sidecar(state_path: &Path, bytes: &[u8]) -> anyhow::Error {
    let position = match serde_json::from_slice::<Value>(bytes) {
        Err(error) => format!(" (line {}, column {})", error.line(), error.column()),
        Ok(_) => String::new(),
    };
    anyhow::anyhow!(
        "parse {} {DESCRIPTION} state from {}: not a valid settings state{position}; fix it, or delete it (the override's earlier value is then not restored)",
        VsCode::DISPLAY_NAME,
        state_path.display()
    )
}
