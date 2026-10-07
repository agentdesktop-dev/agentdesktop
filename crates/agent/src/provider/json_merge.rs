use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::reconcile::ReconcilePlan;

#[derive(Deserialize, Serialize)]
struct MergeState {
    created: bool,
    before: Value,
    after: Value,
}

pub(super) fn state_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    path.with_file_name(format!(".{name}.agentdesktop"))
}

/// An array in the managed document whose elements are identified by a key
/// (for example `providers[].name`), so a managed element replaces the element
/// with the same key rather than sitting next to it, and removal takes the
/// element out even if it was edited in place. `field: ""` names the document
/// root, for a file whose top level is an array.
#[derive(Clone, Copy)]
pub(super) struct KeyedArray {
    pub(super) field: &'static str,
    pub(super) keys: &'static [&'static str],
}

/// How a managed document is written into, and removed from, a user file.
#[derive(Clone, Copy)]
pub(super) struct MergeOptions {
    /// Mode of the file when the merge writes it (`plan_remove` keeps the
    /// file's current mode).
    pub(super) mode: u32,
    /// Arrays whose elements are identified by key. Elements of arrays not
    /// listed here are matched by full equality.
    pub(super) keyed_arrays: &'static [KeyedArray],
    /// Record actions without file content, for a file that holds secrets:
    /// the dry-run report then shows the action and the path only.
    pub(super) redact_diff: bool,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            mode: 0o644,
            keyed_arrays: &[],
            redact_diff: false,
        }
    }
}

pub(super) fn plan_merge(
    path: &Path,
    state_path: &Path,
    managed: Value,
    legacy_owned: bool,
    description: &str,
    display_name: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    plan_merge_with(
        path,
        state_path,
        managed,
        legacy_owned,
        description,
        display_name,
        MergeOptions::default(),
        plan,
    )
}

/// The managed values the previous merge left in the file (the sidecar's
/// `after` snapshot, the whole merged document as last written), so a
/// provider can tell whether an entry was written by it.
pub(super) fn managed_after(
    state_path: &Path,
    display_name: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<Option<Value>> {
    Ok(read_state(state_path, display_name, plan)?.map(|state| state.after))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_merge_with(
    path: &Path,
    state_path: &Path,
    managed: Value,
    legacy_owned: bool,
    description: &str,
    display_name: &str,
    options: MergeOptions,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let existing = match plan.read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {display_name} from {}", path.display()));
        }
    };
    let previous = read_state(state_path, display_name, plan)?;
    // A file that does not exist now is created by this merge, whatever an
    // older sidecar says: the user's earlier file is gone.
    let created = existing.is_none()
        || previous
            .as_ref()
            .map(|state| state.created)
            .unwrap_or(legacy_owned);

    // The file must have the managed document's shape at the root: an object,
    // or an array when the managed document is one.
    let empty_root = || {
        if managed.is_array() {
            json!([])
        } else {
            json!({})
        }
    };
    // An empty (or whitespace-only) file holds nothing to keep and is not a
    // conflict; the merge fills it. Comments or trailing commas are: the file
    // is rewritten as plain JSON, so they would be lost silently.
    let mut combined = match existing.as_deref() {
        Some(contents) if contents.iter().all(u8::is_ascii_whitespace) => empty_root(),
        Some(contents) => match serde_json::from_slice::<Value>(contents) {
            Ok(value)
                if value.is_object() == managed.is_object()
                    && value.is_array() == managed.is_array() =>
            {
                value
            }
            Ok(_) | Err(_) => {
                plan.record(display_name, description, "conflict", path);
                return Ok(());
            }
        },
        None => empty_root(),
    };

    if let Some(previous) = previous.as_ref() {
        combined = rollback_overlay(&combined, &previous.before, &previous.after);
    } else if legacy_owned {
        combined = empty_root();
    }
    // Keyed elements are owned by key: whatever the file holds under a managed
    // key (edited or not) gives way to the managed element, and an element the
    // previous merge added under a key that is no longer managed goes away.
    for keyed in options.keyed_arrays {
        if let Some(previous) = previous.as_ref() {
            let added = keyed_added(keyed, &previous.before, &previous.after);
            remove_keyed(&mut combined, keyed, &added);
        }
        remove_keyed(&mut combined, keyed, &managed);
    }
    let before = combined.clone();
    merge_overlay(&mut combined, managed);

    let mut contents = serde_json::to_vec_pretty(&combined)
        .with_context(|| format!("serialize merged {display_name}"))?;
    contents.push(b'\n');
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => "unchanged",
        Some(_) => "update",
        None => "create",
    };
    record(
        plan,
        display_name,
        description,
        action,
        path,
        existing.as_deref(),
        Some(&contents),
        options.redact_diff,
    );

    if action != "unchanged" {
        plan.write_file(path, &contents, options.mode)?;
    }
    let mut state = serde_json::to_vec_pretty(&MergeState {
        created,
        before,
        after: combined,
    })
    .with_context(|| format!("serialize {display_name} merge state"))?;
    state.push(b'\n');
    plan.write_file(state_path, &state, 0o600)?;
    debug!(provider = display_name, action, path = %path.display(), "planned user settings merge");
    Ok(())
}

pub(super) fn plan_remove(
    path: &Path,
    state_path: &Path,
    description: &str,
    display_name: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<bool> {
    plan_remove_with(
        path,
        state_path,
        description,
        display_name,
        MergeOptions::default(),
        plan,
    )
}

/// `plan_remove` with keyed arrays and diff redaction; the file keeps its
/// current mode when it is rewritten.
pub(super) fn plan_remove_with(
    path: &Path,
    state_path: &Path,
    description: &str,
    display_name: &str,
    options: MergeOptions,
    plan: &ReconcilePlan,
) -> anyhow::Result<bool> {
    let Some(state) = read_state(state_path, display_name, plan)? else {
        return Ok(false);
    };
    let existing = match plan.read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.record(display_name, description, "unchanged", path);
            remove_file(state_path, display_name, plan)?;
            return Ok(true);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {display_name} from {}", path.display()));
        }
    };
    // The file must still have the shape the merge left (object or array); an
    // emptied file holds nothing of ours any more.
    if existing.iter().all(u8::is_ascii_whitespace) {
        if state.created {
            plan.record(display_name, description, "remove", path);
            plan.remove_file(path)
                .with_context(|| format!("remove {display_name} at {}", path.display()))?;
        } else {
            plan.record(display_name, description, "unchanged", path);
        }
        remove_file(state_path, display_name, plan)?;
        return Ok(true);
    }
    let settings = match serde_json::from_slice::<Value>(&existing) {
        Ok(value)
            if value.is_object() == state.after.is_object()
                && value.is_array() == state.after.is_array() =>
        {
            value
        }
        Ok(_) | Err(_) => {
            plan.record(display_name, description, "conflict", path);
            return Ok(true);
        }
    };
    let mut settings = rollback_overlay(&settings, &state.before, &state.after);
    // Keyed elements the last merge added are removed by key, so an element
    // edited in place after the merge goes too.
    for keyed in options.keyed_arrays {
        let added = keyed_added(keyed, &state.before, &state.after);
        remove_keyed(&mut settings, keyed, &added);
        // An array the merge introduced (absent or empty before) and that is
        // now empty goes with it.
        if !keyed.field.is_empty()
            && state
                .before
                .get(keyed.field)
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
            && settings
                .get(keyed.field)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            && let Some(object) = settings.as_object_mut()
        {
            object.remove(keyed.field);
        }
    }
    let empty = settings.as_object().is_some_and(serde_json::Map::is_empty)
        || settings.as_array().is_some_and(Vec::is_empty);
    let proposed = if state.created && empty {
        None
    } else {
        let mut contents = serde_json::to_vec_pretty(&settings)
            .with_context(|| format!("serialize {display_name} after removing managed values"))?;
        contents.push(b'\n');
        Some(contents)
    };
    let action = match proposed.as_deref() {
        None => "remove",
        Some(contents) if contents == existing => "unchanged",
        Some(_) => "update",
    };
    record(
        plan,
        display_name,
        description,
        action,
        path,
        Some(&existing),
        proposed.as_deref(),
        options.redact_diff,
    );
    if action == "remove" {
        plan.remove_file(path)
            .with_context(|| format!("remove {display_name} at {}", path.display()))?;
    } else if let Some(contents) = proposed
        && action == "update"
    {
        // The user's remaining content keeps the file's current mode: a file
        // that was owner-only (it may hold the user's own keys) stays so.
        plan.write_file(path, &contents, current_mode(path).unwrap_or(0o644))?;
    }
    remove_file(state_path, display_name, plan)?;
    debug!(provider = display_name, action, path = %path.display(), "planned removal of managed values from user settings");
    Ok(true)
}

fn read_state(
    path: &Path,
    display_name: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<Option<MergeState>> {
    match plan.read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("parse {display_name} merge state from {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("read {display_name} merge state from {}", path.display())),
    }
}

fn remove_file(path: &Path, display_name: &str, plan: &ReconcilePlan) -> anyhow::Result<()> {
    match plan.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("remove {display_name} merge state at {}", path.display())),
    }
}

fn merge_overlay(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge_overlay(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (Value::Array(base), Value::Array(overlay)) => {
            for value in overlay {
                if !base.contains(&value) {
                    base.push(value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn rollback_overlay(current: &Value, before: &Value, after: &Value) -> Value {
    rollback_value(Some(current), Some(before), Some(after)).unwrap_or_else(|| json!({}))
}

fn rollback_value(
    current: Option<&Value>,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Option<Value> {
    if current == after {
        return before.cloned();
    }
    if before == after {
        return current.cloned();
    }
    let current = current?;

    if let Some(current_object) = current.as_object()
        && before.is_none_or(Value::is_object)
        && after.is_none_or(Value::is_object)
    {
        let mut result = current_object.clone();
        let before_object = before.and_then(Value::as_object);
        let after_object = after.and_then(Value::as_object);
        let mut keys = BTreeSet::new();
        keys.extend(result.keys().cloned());
        keys.extend(
            before_object
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .cloned(),
        );
        keys.extend(
            after_object
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .cloned(),
        );
        for key in keys {
            match rollback_value(
                current_object.get(&key),
                before_object.and_then(|object| object.get(&key)),
                after_object.and_then(|object| object.get(&key)),
            ) {
                Some(value) => {
                    result.insert(key, value);
                }
                None => {
                    result.remove(&key);
                }
            }
        }
        return Some(Value::Object(result));
    }

    if let Some(current_array) = current.as_array()
        && before.is_none_or(Value::is_array)
        && after.is_none_or(Value::is_array)
    {
        let before = before
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let after = after
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let mut result = current_array.clone();
        for added in after.iter().filter(|value| !before.contains(value)) {
            if let Some(index) = result.iter().position(|value| value == added) {
                result.remove(index);
            }
        }
        return Some(Value::Array(result));
    }

    Some(current.clone())
}

/// The mode bits of an existing file, on platforms that have them.
pub(super) fn current_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    plan: &ReconcilePlan,
    display_name: &str,
    description: &str,
    action: &str,
    path: &Path,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    redact: bool,
) {
    if redact {
        plan.record_diff(display_name, description, action, path, None, None);
    } else {
        plan.record_diff(display_name, description, action, path, before, after);
    }
}

/// The key tuple of an array element, `None` when a key field is missing or
/// not a string (such an element is never matched by key).
fn element_key(keyed: &KeyedArray, element: &Value) -> Option<Vec<String>> {
    keyed
        .keys
        .iter()
        .map(|key| element.get(*key)?.as_str().map(str::to_owned))
        .collect()
}

/// The keyed array inside `document`: the field, or the document itself for
/// `field: ""`.
fn keyed_array<'a>(document: &'a Value, keyed: &KeyedArray) -> Option<&'a Vec<Value>> {
    if keyed.field.is_empty() {
        document.as_array()
    } else {
        document.get(keyed.field).and_then(Value::as_array)
    }
}

fn keyed_array_mut<'a>(document: &'a mut Value, keyed: &KeyedArray) -> Option<&'a mut Vec<Value>> {
    if keyed.field.is_empty() {
        document.as_array_mut()
    } else {
        document.get_mut(keyed.field).and_then(Value::as_array_mut)
    }
}

/// Remove from the keyed array of `document` every element whose key equals
/// the key of an element in the keyed array of `managed`.
fn remove_keyed(document: &mut Value, keyed: &KeyedArray, managed: &Value) {
    let managed_keys: Vec<Vec<String>> = keyed_array(managed, keyed)
        .into_iter()
        .flatten()
        .filter_map(|element| element_key(keyed, element))
        .collect();
    if managed_keys.is_empty() {
        return;
    }
    if let Some(elements) = keyed_array_mut(document, keyed) {
        elements.retain(|element| {
            element_key(keyed, element).is_none_or(|key| !managed_keys.contains(&key))
        });
    }
}

/// The document holding only the keyed elements the last merge added
/// (`after` minus `before`, by key).
fn keyed_added(keyed: &KeyedArray, before: &Value, after: &Value) -> Value {
    let before_keys: Vec<Vec<String>> = keyed_array(before, keyed)
        .into_iter()
        .flatten()
        .filter_map(|element| element_key(keyed, element))
        .collect();
    let added: Vec<Value> = keyed_array(after, keyed)
        .into_iter()
        .flatten()
        .filter(|element| {
            element_key(keyed, element).is_some_and(|key| !before_keys.contains(&key))
        })
        .cloned()
        .collect();
    if keyed.field.is_empty() {
        Value::Array(added)
    } else {
        json!({ keyed.field: added })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed() -> Value {
        json!({"env": {"MANAGED": "1"}})
    }

    #[test]
    fn an_empty_file_is_filled_not_a_conflict_and_a_comment_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        for contents in ["", "  \n"] {
            let path = dir.path().join("empty.json");
            let state = state_path(&path);
            std::fs::write(&path, contents).unwrap();
            let plan = ReconcilePlan::default();
            plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
            assert!(!plan.has_conflicts(), "{}", plan.render());
            plan.apply().unwrap();
            let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(written, managed());
            // Emptied by hand afterwards: removal drops the sidecar and leaves
            // the user's (pre-existing) file.
            std::fs::write(&path, "").unwrap();
            let plan = ReconcilePlan::default();
            assert!(plan_remove(&path, &state, "settings", "Test", &plan).unwrap());
            plan.apply().unwrap();
            assert!(!state.exists());
            assert!(path.exists());
            std::fs::remove_file(&path).unwrap();
        }
        // A file the merge created and the user then emptied is removed whole.
        let path = dir.path().join("created.json");
        let state = state_path(&path);
        let plan = ReconcilePlan::default();
        plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
        plan.apply().unwrap();
        std::fs::write(&path, "").unwrap();
        let plan = ReconcilePlan::default();
        assert!(plan_remove(&path, &state, "settings", "Test", &plan).unwrap());
        plan.apply().unwrap();
        assert!(!path.exists() && !state.exists());
        let path = dir.path().join("commented.json");
        std::fs::write(&path, "{\n  // a comment\n  \"env\": {}\n}\n").unwrap();
        let plan = ReconcilePlan::default();
        plan_merge(
            &path,
            &state_path(&path),
            managed(),
            false,
            "settings",
            "Test",
            &plan,
        )
        .unwrap();
        assert!(plan.has_conflicts(), "comments are not rewritten silently");
    }

    #[test]
    fn removal_refuses_a_file_whose_shape_changed_since_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        for (managed, replaced) in [
            (json!({"env": {"MANAGED": "1"}}), "[]\n"),
            (json!([{"name": "agentdesktop"}]), "{}\n"),
        ] {
            let path = dir.path().join("shape.json");
            let state = state_path(&path);
            let plan = ReconcilePlan::default();
            let options = MergeOptions {
                keyed_arrays: &[KeyedArray {
                    field: "",
                    keys: &["name"],
                }],
                ..MergeOptions::default()
            };
            plan_merge_with(&path, &state, managed, false, "x", "Test", options, &plan).unwrap();
            plan.apply().unwrap();
            std::fs::write(&path, replaced).unwrap();
            let plan = ReconcilePlan::default();
            assert!(plan_remove_with(&path, &state, "x", "Test", options, &plan).unwrap());
            assert!(plan.has_conflicts(), "{}", plan.render());
            assert!(plan.apply().is_err());
            assert!(state.exists(), "a conflict keeps the sidecar");
            std::fs::remove_file(&path).unwrap();
            std::fs::remove_file(&state).unwrap();
        }
    }

    // Default options, as Claude Code and Claude Desktop use them.
    #[test]
    fn removal_keeps_the_file_mode_and_a_recreated_file_counts_as_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let state = state_path(&path);
        // A user file that is owner-only stays owner-only after the managed
        // values are removed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&path, b"{\"env\":{\"USER\":\"keep\"}}\n").unwrap();
            let plan = ReconcilePlan::default();
            plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
            plan.apply().unwrap();
            // The user tightens the mode after the merge (the default merge
            // writes 0644, as before); removal keeps what it finds.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let plan = ReconcilePlan::default();
            assert!(plan_remove(&path, &state, "settings", "Test", &plan).unwrap());
            plan.apply().unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let remaining: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(remaining, json!({"env": {"USER": "keep"}}));
        }
        // The user's file existed at the first merge (not created by us); it
        // is then deleted and the next merge recreates it: removal deletes it.
        std::fs::write(&path, b"{\"env\":{\"USER\":\"keep\"}}\n").unwrap();
        let plan = ReconcilePlan::default();
        plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
        plan.apply().unwrap();
        std::fs::remove_file(&path).unwrap();
        let plan = ReconcilePlan::default();
        plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
        plan.apply().unwrap();
        let plan = ReconcilePlan::default();
        assert!(plan_remove(&path, &state, "settings", "Test", &plan).unwrap());
        plan.apply().unwrap();
        assert!(
            !path.exists(),
            "a file the merge recreated is removed whole"
        );
    }

    // --- Root-array support -------------------------------------------------
    //
    // A managed document that is itself a top-level array, keyed by `name` at
    // the document root (`field: ""`), used by the VS Code provider's
    // `chatLanguageModels.json`. Object-root callers (above) must keep
    // working unchanged.

    const ROOT_KEYED: &[KeyedArray] = &[KeyedArray {
        field: "",
        keys: &["name"],
    }];

    fn root_array_options() -> MergeOptions {
        MergeOptions {
            mode: 0o600,
            keyed_arrays: ROOT_KEYED,
            redact_diff: true,
        }
    }

    #[test]
    fn root_array_merge_keeps_user_entries_and_replaces_managed_entry_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatLanguageModels.json");
        let state = state_path(&path);
        let options = root_array_options();

        let managed = json!([{"name": "agentdesktop", "vendor": "customendpoint"}]);
        let plan = ReconcilePlan::default();
        plan_merge_with(
            &path,
            &state,
            managed,
            false,
            "chat models",
            "Test",
            options,
            &plan,
        )
        .unwrap();
        plan.apply().unwrap();

        let mut document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(document.is_array(), "managed document must stay an array");
        // The user hand-adds their own vendor entry.
        document
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "user-vendor"}));
        std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        // Re-apply with an edited managed entry: replaced by key, not
        // duplicated; the user's entry survives.
        let managed = json!([{"name": "agentdesktop", "vendor": "customendpoint", "apiType": "chat-completions"}]);
        let plan = ReconcilePlan::default();
        plan_merge_with(
            &path,
            &state,
            managed,
            false,
            "chat models",
            "Test",
            options,
            &plan,
        )
        .unwrap();
        plan.apply().unwrap();

        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let array = after.as_array().unwrap();
        assert_eq!(array.len(), 2, "{after}");
        assert!(array.iter().any(|entry| entry["name"] == "user-vendor"));
        let ours = array
            .iter()
            .find(|entry| entry["name"] == "agentdesktop")
            .unwrap();
        assert_eq!(ours["apiType"], "chat-completions");
    }

    #[test]
    fn root_array_removal_strips_the_managed_entry_by_key_and_deletes_when_empty_and_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatLanguageModels.json");
        let state = state_path(&path);
        let options = root_array_options();

        let managed = json!([{"name": "agentdesktop"}]);
        let plan = ReconcilePlan::default();
        plan_merge_with(
            &path,
            &state,
            managed,
            false,
            "chat models",
            "Test",
            options,
            &plan,
        )
        .unwrap();
        plan.apply().unwrap();
        assert!(path.exists());

        let plan = ReconcilePlan::default();
        assert!(plan_remove_with(&path, &state, "chat models", "Test", options, &plan).unwrap());
        plan.apply().unwrap();
        assert!(
            !path.exists(),
            "an empty root array we created must be deleted"
        );
    }

    #[test]
    fn root_array_removal_keeps_a_user_entry_and_the_files_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatLanguageModels.json");
        let state = state_path(&path);
        let options = root_array_options();

        std::fs::write(&path, b"[{\"name\":\"user-vendor\"}]\n").unwrap();
        let plan = ReconcilePlan::default();
        plan_merge_with(
            &path,
            &state,
            json!([{"name": "agentdesktop"}]),
            false,
            "chat models",
            "Test",
            options,
            &plan,
        )
        .unwrap();
        plan.apply().unwrap();

        let plan = ReconcilePlan::default();
        assert!(plan_remove_with(&path, &state, "chat models", "Test", options, &plan).unwrap());
        plan.apply().unwrap();

        assert!(
            path.exists(),
            "a file we did not create must not be deleted"
        );
        let remaining: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(remaining, json!([{"name": "user-vendor"}]));
    }

    #[test]
    fn object_root_callers_are_unaffected_by_root_array_support() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let state = state_path(&path);

        let plan = ReconcilePlan::default();
        plan_merge(&path, &state, managed(), false, "settings", "Test", &plan).unwrap();
        plan.apply().unwrap();
        let document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(document.is_object(), "{document}");
        assert_eq!(document, json!({"env": {"MANAGED": "1"}}));

        let plan = ReconcilePlan::default();
        assert!(plan_remove(&path, &state, "settings", "Test", &plan).unwrap());
        plan.apply().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_non_array_file_is_a_conflict_when_the_managed_document_is_an_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chatLanguageModels.json");
        let state = state_path(&path);
        std::fs::write(&path, b"42\n").unwrap();

        let plan = ReconcilePlan::default();
        plan_merge_with(
            &path,
            &state,
            json!([{"name": "agentdesktop"}]),
            false,
            "chat models",
            "Test",
            root_array_options(),
            &plan,
        )
        .unwrap();
        assert!(plan.has_conflicts(), "{}", plan.render());
        assert!(plan.apply().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"42\n");
    }
}
