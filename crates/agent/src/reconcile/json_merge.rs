use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::ReconcileMode;

#[derive(Deserialize, Serialize)]
struct MergeState {
    created: bool,
    before: Value,
    after: Value,
}

pub fn state_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    path.with_file_name(format!(".{name}.agentdesktop"))
}

#[allow(clippy::too_many_arguments)]
pub fn apply(
    path: &Path,
    state_path: &Path,
    managed: Value,
    legacy_owned: bool,
    program: &str,
    description: &str,
    display_name: &str,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let existing = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {display_name} from {}", path.display()));
        }
    };
    let previous = read_state(state_path, display_name)?;
    let created = previous
        .as_ref()
        .map(|state| state.created)
        .unwrap_or(existing.is_none() || legacy_owned);

    let mut combined = match existing.as_deref() {
        Some(contents) => match serde_json::from_slice::<Value>(contents) {
            Ok(Value::Object(object)) => Value::Object(object),
            Ok(_) | Err(_) if mode.is_dry_run() => {
                mode.record(program, description, "conflict", path);
                return Ok(());
            }
            Ok(_) => anyhow::bail!("{display_name} must be a JSON object at {}", path.display()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("parse {display_name} from {}", path.display()));
            }
        },
        None => json!({}),
    };

    if let Some(previous) = previous.as_ref() {
        combined = rollback_overlay(&combined, &previous.before, &previous.after);
    } else if legacy_owned {
        combined = json!({});
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
    mode.record_diff(
        program,
        description,
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    if !mode.writes() {
        return Ok(());
    }

    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .with_context(|| format!("create {display_name} directory {}", directory.display()))?;
    if action != "unchanged" {
        secure_fs::atomic_write(path, &contents, 0o644)?;
    }
    let mut state = serde_json::to_vec_pretty(&MergeState {
        created,
        before,
        after: combined,
    })
    .with_context(|| format!("serialize {display_name} merge state"))?;
    state.push(b'\n');
    secure_fs::atomic_write(state_path, &state, 0o600)?;
    info!(program, action, path = %path.display(), "merged user settings");
    Ok(())
}

pub fn remove(
    path: &Path,
    state_path: &Path,
    program: &str,
    description: &str,
    display_name: &str,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<bool> {
    let Some(state) = read_state(state_path, display_name)? else {
        return Ok(false);
    };
    let existing = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            mode.record(program, description, "unchanged", path);
            if mode.writes() {
                remove_file(state_path, display_name)?;
            }
            return Ok(true);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {display_name} from {}", path.display()));
        }
    };
    let settings = match serde_json::from_slice::<Value>(&existing) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(_) | Err(_) if mode.is_dry_run() => {
            mode.record(program, description, "conflict", path);
            return Ok(true);
        }
        Ok(_) => anyhow::bail!("{display_name} must be a JSON object at {}", path.display()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("parse {display_name} from {}", path.display()));
        }
    };
    let settings = rollback_overlay(&settings, &state.before, &state.after);
    let empty = settings.as_object().is_some_and(serde_json::Map::is_empty);
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
    mode.record_diff(
        program,
        description,
        action,
        path,
        Some(&existing),
        proposed.as_deref(),
    );
    if !mode.writes() {
        return Ok(true);
    }
    if action == "remove" {
        fs::remove_file(path)
            .with_context(|| format!("remove {display_name} at {}", path.display()))?;
    } else if let Some(contents) = proposed
        && action == "update"
    {
        secure_fs::atomic_write(path, &contents, 0o644)?;
    }
    remove_file(state_path, display_name)?;
    info!(program, action, path = %path.display(), "removed managed values from user settings");
    Ok(true)
}

fn read_state(path: &Path, display_name: &str) -> anyhow::Result<Option<MergeState>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("parse {display_name} merge state from {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("read {display_name} merge state from {}", path.display())),
    }
}

fn remove_file(path: &Path, display_name: &str) -> anyhow::Result<()> {
    match fs::remove_file(path) {
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
