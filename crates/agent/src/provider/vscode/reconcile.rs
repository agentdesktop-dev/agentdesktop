use std::{collections::BTreeSet, path::Path};

use agentdesktop_core::config::VscodeConfig;
use anyhow::Context;
use jsonc_parser::{
    ParseOptions,
    cst::{CstNode, CstObjectProp, CstRootNode},
    json,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::VsCode;
use crate::{provider::json_merge, reconcile::ReconcilePlan};

const DISPLAY_NAME: &str = "VS Code user settings";
const ADVANCED_KEY: &str = "github.copilot.advanced";
const PROXY_KEY: &str = "debug.overrideProxyUrl";
const CAPI_KEY: &str = "debug.overrideCapiUrl";

#[derive(Deserialize, Serialize)]
struct MergeState {
    created: bool,
    advanced_created: bool,
    before: Option<String>,
    after: String,
    #[serde(default)]
    capi_before: Option<String>,
    #[serde(default)]
    capi_after: Option<String>,
}

enum PropertyValue {
    Missing,
    String(String),
    Other,
}

pub(super) fn plan(
    path: &Path,
    config: Option<&VscodeConfig>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let state_path = json_merge::state_path(path);
    let Some(config) = config else {
        return remove(path, &state_path, plan);
    };
    let proxy_url = config
        .copilot_proxy_url
        .as_ref()
        .context("VS Code Copilot proxy URL is not configured")?
        .as_str();
    let capi_url = proxy_url
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .context("VS Code Copilot proxy URL must end in /v1")?;
    let existing = match plan.read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {DISPLAY_NAME} from {}", path.display()));
        }
    };
    let Some(root) = parse_settings(existing.as_deref().unwrap_or(b"{}\n"), path, plan) else {
        return Ok(());
    };
    let Some(settings) = root.object_value_or_create() else {
        plan.record(VsCode::DISPLAY_NAME, "settings", "conflict", path);
        return Ok(());
    };
    let previous = read_state(&state_path, plan)?;
    if plan.has_conflicts() {
        return Ok(());
    }
    let advanced_existed = settings.get(ADVANCED_KEY).is_some();
    let Some(advanced) = settings.object_value_or_create(ADVANCED_KEY) else {
        plan.record(VsCode::DISPLAY_NAME, "settings", "conflict", path);
        return Ok(());
    };
    let current = string_setting(&advanced, PROXY_KEY, path)
        .and_then(|before| Ok((before, string_setting(&advanced, CAPI_KEY, path)?)));
    let (mut before, mut capi_before) = match current {
        Ok(values) => values,
        Err(_) => {
            plan.record(VsCode::DISPLAY_NAME, "settings", "conflict", path);
            return Ok(());
        }
    };
    // Recover the original values without removing/re-adding managed properties.
    // Repeated plans must not reorder settings or discard their comments.
    if let Some(previous) = previous.as_ref() {
        if before.as_deref() == Some(previous.after.as_str()) {
            before = previous.before.clone();
        }
        if previous.capi_after.is_some() && capi_before == previous.capi_after {
            capi_before = previous.capi_before.clone();
        }
    }
    set_setting(&advanced, PROXY_KEY, proxy_url);
    set_setting(&advanced, CAPI_KEY, capi_url);

    let contents = root.to_string().into_bytes();
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => "unchanged",
        Some(_) => "update",
        None => "create",
    };
    plan.record_diff(
        VsCode::DISPLAY_NAME,
        "settings",
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    if action != "unchanged" {
        plan.write_file(path, &contents, 0o644)?;
    }
    let state = MergeState {
        created: previous
            .as_ref()
            .map(|state| state.created)
            .unwrap_or(existing.is_none()),
        advanced_created: previous
            .as_ref()
            .map(|state| state.advanced_created)
            .unwrap_or(!advanced_existed),
        before,
        after: proxy_url.to_owned(),
        capi_before,
        capi_after: Some(capi_url.to_owned()),
    };
    write_state(&state_path, &state, plan)?;
    debug!(program = VsCode::ID, action, path = %path.display(), "planned user settings merge");
    Ok(())
}

fn remove(path: &Path, state_path: &Path, plan: &ReconcilePlan) -> anyhow::Result<()> {
    let Some(state) = read_state(state_path, plan)? else {
        return Ok(());
    };
    let existing = match plan.read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.record(VsCode::DISPLAY_NAME, "settings", "unchanged", path);
            remove_state(state_path, plan)?;
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {DISPLAY_NAME} from {}", path.display()));
        }
    };
    let Some(root) = parse_settings(&existing, path, plan) else {
        return Ok(());
    };
    let Some(settings) = root.object_value() else {
        plan.record(VsCode::DISPLAY_NAME, "settings", "conflict", path);
        return Ok(());
    };
    let mut removals = Vec::new();
    let mut remove_advanced = false;
    if let Some(advanced) = settings.object_value(ADVANCED_KEY) {
        restore_if_managed(
            &advanced,
            PROXY_KEY,
            &state.before,
            &state.after,
            &mut removals,
        );
        if let Some(capi_after) = state.capi_after.as_deref() {
            restore_if_managed(
                &advanced,
                CAPI_KEY,
                &state.capi_before,
                capi_after,
                &mut removals,
            );
        }
        if state.advanced_created
            && advanced.properties().len() == removals.len()
            && !has_comments(&advanced.into())
        {
            remove_advanced = true;
            removals.clear();
            removals.push(settings.get(ADVANCED_KEY).expect("advanced settings exist"));
        }
    }

    let proposed = if state.created
        && settings.properties().len() == usize::from(remove_advanced)
        && !has_comments(&root.clone().into())
    {
        None
    } else {
        Some(without_properties(&root, &removals).into_bytes())
    };
    let action = match proposed.as_deref() {
        None => "remove",
        Some(contents) if contents == existing => "unchanged",
        Some(_) => "update",
    };
    plan.record_diff(
        VsCode::DISPLAY_NAME,
        "settings",
        action,
        path,
        Some(&existing),
        proposed.as_deref(),
    );
    match proposed {
        None => plan
            .remove_file(path)
            .with_context(|| format!("remove {DISPLAY_NAME} at {}", path.display()))?,
        Some(contents) if action == "update" => plan.write_file(path, &contents, 0o644)?,
        Some(_) => {}
    }
    remove_state(state_path, plan)?;
    debug!(program = VsCode::ID, action, path = %path.display(), "planned removal of managed user settings");
    Ok(())
}

fn parse_settings(contents: &[u8], path: &Path, plan: &ReconcilePlan) -> Option<CstRootNode> {
    match std::str::from_utf8(contents)
        .ok()
        .and_then(|text| CstRootNode::parse(text, &ParseOptions::default()).ok())
    {
        Some(root) => Some(root),
        None => {
            plan.record(VsCode::DISPLAY_NAME, "settings", "conflict", path);
            None
        }
    }
}

fn property_value(advanced: &jsonc_parser::cst::CstObject, key: &str) -> PropertyValue {
    let Some(value) = advanced.get(key).and_then(|property| property.value()) else {
        return PropertyValue::Missing;
    };
    value
        .as_string_lit()
        .and_then(|value| value.decoded_value().ok())
        .map(PropertyValue::String)
        .unwrap_or(PropertyValue::Other)
}

fn string_setting(
    advanced: &jsonc_parser::cst::CstObject,
    key: &str,
    path: &Path,
) -> anyhow::Result<Option<String>> {
    match property_value(advanced, key) {
        PropertyValue::Missing => Ok(None),
        PropertyValue::String(value) => Ok(Some(value)),
        PropertyValue::Other => anyhow::bail!(
            "{ADVANCED_KEY}.{key} must be a string in {}",
            path.display()
        ),
    }
}

fn set_setting(advanced: &jsonc_parser::cst::CstObject, key: &str, value: &str) {
    if matches!(property_value(advanced, key), PropertyValue::String(current) if current == value) {
        return;
    }
    if let Some(property) = advanced.get(key) {
        property.set_value(json!(value));
    } else {
        advanced.append(key, json!(value));
    }
}

fn restore_if_managed(
    advanced: &jsonc_parser::cst::CstObject,
    key: &str,
    before: &Option<String>,
    after: &str,
    removals: &mut Vec<CstObjectProp>,
) {
    let PropertyValue::String(current) = property_value(advanced, key) else {
        return;
    };
    if current != after {
        return;
    }
    match before.as_deref() {
        Some(before) => set_setting(advanced, key, before),
        None => removals.push(advanced.get(key).expect("managed VS Code setting exists")),
    }
}

fn has_comments(node: &CstNode) -> bool {
    node.as_comment().is_some() || node.children().iter().any(has_comments)
}

/// CST property removal also discards adjacent comments. Remove only the
/// property tokens and separator instead, leaving comments in their original
/// object, including comments between a property's name and value.
fn without_properties(root: &CstRootNode, properties: &[CstObjectProp]) -> String {
    let mut edits = Vec::new();
    let mut commas = BTreeSet::new();
    for property in properties {
        let node: CstNode = property.clone().into();
        let start = node_offset(&node);
        let trivia = node
            .children()
            .iter()
            .filter(|child| child.is_trivia())
            .map(ToString::to_string)
            .collect::<String>();
        edits.push((start..start + node.to_string().len(), trivia));
        let comma = property
            .trailing_comma()
            .map(CstNode::from)
            .or_else(|| property.previous_siblings().find(CstNode::is_comma));
        if let Some(comma) = comma {
            commas.insert(node_offset(&comma));
        }
    }
    // Adjacent removals can share the same comma; remove each separator once.
    edits.extend(
        commas
            .into_iter()
            .map(|start| (start..start + 1, String::new())),
    );
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0.start));
    let mut contents = root.to_string();
    for (range, replacement) in edits {
        contents.replace_range(range, &replacement);
    }
    contents
}

fn node_offset(node: &CstNode) -> usize {
    let preceding: usize = node
        .previous_siblings()
        .map(|node| node.to_string().len())
        .sum();
    preceding
        + node
            .parent()
            .map_or(0, |parent| node_offset(&parent.into()))
}

fn read_state(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<Option<MergeState>> {
    match plan.read(path) {
        Ok(contents) => match serde_json::from_slice(&contents) {
            Ok(state) => Ok(Some(state)),
            Err(_) => {
                // An invalid ownership record must not be treated as an unowned file.
                plan.record(VsCode::DISPLAY_NAME, "merge state", "conflict", path);
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read VS Code merge state from {}", path.display()))
        }
    }
}

fn write_state(path: &Path, state: &MergeState, plan: &ReconcilePlan) -> anyhow::Result<()> {
    let mut contents = serde_json::to_vec_pretty(state).context("serialize VS Code merge state")?;
    contents.push(b'\n');
    plan.write_file(path, &contents, 0o600)
}

fn remove_state(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<()> {
    match plan.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove VS Code merge state at {}", path.display()))
        }
    }
}
