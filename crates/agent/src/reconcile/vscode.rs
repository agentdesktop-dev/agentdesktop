use std::{fs, path::Path};

use agentdesktop_core::config::VscodeConfig;
use anyhow::Context;
use jsonc_parser::{ParseOptions, cst::CstRootNode, json};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::secure_fs;

use super::{ReconcileMode, json_merge};

const CONFIG_PROGRAM: &str = "vscode";
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

pub fn apply(
    path: &Path,
    config: Option<&VscodeConfig>,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let state_path = json_merge::state_path(path);
    let Some(config) = config else {
        return remove(path, &state_path, mode);
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
    let existing = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {DISPLAY_NAME} from {}", path.display()));
        }
    };
    let text = existing
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .with_context(|| format!("decode {DISPLAY_NAME} from {}", path.display()))?
        .unwrap_or("{}\n");
    let root = match CstRootNode::parse(text, &ParseOptions::default()) {
        Ok(root) => root,
        Err(_) if mode.is_dry_run() => {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("parse {DISPLAY_NAME} at {}", path.display()));
        }
    };
    let Some(settings) = root.object_value_or_create() else {
        if mode.is_dry_run() {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        anyhow::bail!(
            "{DISPLAY_NAME} must contain a JSON object at {}",
            path.display()
        );
    };
    let previous = read_state(&state_path)?;
    let advanced_existed = settings.get(ADVANCED_KEY).is_some();
    let Some(advanced) = settings.object_value_or_create(ADVANCED_KEY) else {
        if mode.is_dry_run() {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        anyhow::bail!("{ADVANCED_KEY} must be an object in {}", path.display());
    };
    if let Some(previous) = previous.as_ref() {
        restore_if_managed(&advanced, PROXY_KEY, &previous.before, &previous.after);
        if let Some(capi_after) = previous.capi_after.as_deref() {
            restore_if_managed(&advanced, CAPI_KEY, &previous.capi_before, capi_after);
        }
    }
    let current = string_setting(&advanced, PROXY_KEY, path)
        .and_then(|before| Ok((before, string_setting(&advanced, CAPI_KEY, path)?)));
    let (before, capi_before) = match current {
        Ok(values) => values,
        Err(_) if mode.is_dry_run() => {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    set_setting(&advanced, PROXY_KEY, proxy_url);
    set_setting(&advanced, CAPI_KEY, capi_url);

    let contents = root.to_string().into_bytes();
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => "unchanged",
        Some(_) => "update",
        None => "create",
    };
    mode.record_diff(
        CONFIG_PROGRAM,
        "settings",
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
        .with_context(|| format!("create VS Code settings directory {}", directory.display()))?;
    if action != "unchanged" {
        secure_fs::atomic_write(path, &contents, 0o644)?;
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
    write_state(&state_path, &state)?;
    info!(program = CONFIG_PROGRAM, action, path = %path.display(), "merged user settings");
    Ok(())
}

fn remove(path: &Path, state_path: &Path, mode: ReconcileMode<'_>) -> anyhow::Result<()> {
    let Some(state) = read_state(state_path)? else {
        return Ok(());
    };
    let existing = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            mode.record(CONFIG_PROGRAM, "settings", "unchanged", path);
            if mode.writes() {
                remove_state(state_path)?;
            }
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {DISPLAY_NAME} from {}", path.display()));
        }
    };
    let text = std::str::from_utf8(&existing)
        .with_context(|| format!("decode {DISPLAY_NAME} from {}", path.display()))?;
    let root = match CstRootNode::parse(text, &ParseOptions::default()) {
        Ok(root) => root,
        Err(_) if mode.is_dry_run() => {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("parse {DISPLAY_NAME} at {}", path.display()));
        }
    };
    let Some(settings) = root.object_value() else {
        if mode.is_dry_run() {
            mode.record(CONFIG_PROGRAM, "settings", "conflict", path);
            return Ok(());
        }
        anyhow::bail!(
            "{DISPLAY_NAME} must contain a JSON object at {}",
            path.display()
        );
    };
    if let Some(advanced) = settings.object_value(ADVANCED_KEY) {
        restore_if_managed(&advanced, PROXY_KEY, &state.before, &state.after);
        if let Some(capi_after) = state.capi_after.as_deref() {
            restore_if_managed(&advanced, CAPI_KEY, &state.capi_before, capi_after);
        }
        if state.advanced_created && object_is_empty(&advanced) {
            settings
                .get(ADVANCED_KEY)
                .expect("advanced settings exist")
                .remove();
        }
    }

    let proposed = if state.created && object_is_empty(&settings) {
        None
    } else {
        Some(root.to_string().into_bytes())
    };
    let action = match proposed.as_deref() {
        None => "remove",
        Some(contents) if contents == existing => "unchanged",
        Some(_) => "update",
    };
    mode.record_diff(
        CONFIG_PROGRAM,
        "settings",
        action,
        path,
        Some(&existing),
        proposed.as_deref(),
    );
    if !mode.writes() {
        return Ok(());
    }
    match proposed {
        None => fs::remove_file(path)
            .with_context(|| format!("remove {DISPLAY_NAME} at {}", path.display()))?,
        Some(contents) if action == "update" => secure_fs::atomic_write(path, &contents, 0o644)?,
        Some(_) => {}
    }
    remove_state(state_path)?;
    info!(program = CONFIG_PROGRAM, action, path = %path.display(), "removed managed user setting");
    Ok(())
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
) {
    let PropertyValue::String(current) = property_value(advanced, key) else {
        return;
    };
    if current != after {
        return;
    }
    match before.as_deref() {
        Some(before) => set_setting(advanced, key, before),
        None => advanced
            .get(key)
            .expect("managed VS Code setting exists")
            .remove(),
    }
}

fn object_is_empty(object: &jsonc_parser::cst::CstObject) -> bool {
    object.properties().is_empty()
        && !object
            .children()
            .iter()
            .any(|child| child.as_comment().is_some())
}

fn read_state(path: &Path) -> anyhow::Result<Option<MergeState>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("parse VS Code merge state from {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read VS Code merge state from {}", path.display()))
        }
    }
}

fn write_state(path: &Path, state: &MergeState) -> anyhow::Result<()> {
    let mut contents = serde_json::to_vec_pretty(state).context("serialize VS Code merge state")?;
    contents.push(b'\n');
    secure_fs::atomic_write(path, &contents, 0o600)
}

fn remove_state(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove VS Code merge state at {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{ReconcileMode, apply, json_merge};
    use agentdesktop_core::config::parse_daemon;

    fn config() -> agentdesktop_core::config::DaemonConfig {
        parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4001
programs:
  vscode:
    copilotProxyUrl: http://127.0.0.1:4002/v1
"#,
        )
        .expect("valid VS Code gateway configuration")
    }

    #[test]
    fn preserves_comments_and_restores_the_previous_proxy() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-vscode-reconcile-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
    // Keep this comment.
    "editor.fontSize": 15,
    "github.copilot.advanced": {
        "authProvider": "github",
        "debug.overrideProxyUrl": "https://previous.example.com/v1",
    },
}
"#,
        )
        .unwrap();
        let config = config();

        apply(&path, config.programs.vscode.as_ref(), ReconcileMode::Apply)
            .expect("apply VS Code proxy");
        let settings = fs::read_to_string(&path).unwrap();
        assert!(settings.contains("// Keep this comment."));
        assert!(settings.contains("\"editor.fontSize\": 15"));
        assert!(settings.contains("\"authProvider\": \"github\""));
        assert!(settings.contains("http://127.0.0.1:4002/v1"));
        assert!(settings.contains("\"debug.overrideCapiUrl\": \"http://127.0.0.1:4002\""));

        apply(&path, None, ReconcileMode::Apply).expect("remove VS Code proxy");
        let settings = fs::read_to_string(&path).unwrap();
        assert!(settings.contains("// Keep this comment."));
        assert!(settings.contains("https://previous.example.com/v1"));
        assert!(!settings.contains("debug.overrideCapiUrl"));
        assert!(!json_merge::state_path(&path).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_a_settings_file_created_only_for_the_proxy() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-vscode-new-settings-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join("settings.json");
        let config = config();

        apply(&path, config.programs.vscode.as_ref(), ReconcileMode::Apply)
            .expect("create VS Code settings");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("debug.overrideProxyUrl")
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("debug.overrideCapiUrl")
        );

        apply(&path, None, ReconcileMode::Apply).expect("remove VS Code settings");
        assert!(!path.exists());
        assert!(!json_merge::state_path(&path).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrades_legacy_state_and_restores_existing_capi_url() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-vscode-legacy-state-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            r#"{
    "github.copilot.advanced": {
        "debug.overrideProxyUrl": "http://127.0.0.1:4002/v1",
        "debug.overrideCapiUrl": "https://previous-capi.example.com"
    }
}
"#,
        )
        .unwrap();
        fs::write(
            json_merge::state_path(&path),
            r#"{
  "created": false,
  "advanced_created": false,
  "before": "https://previous-proxy.example.com/v1",
  "after": "http://127.0.0.1:4002/v1"
}
"#,
        )
        .unwrap();

        let config = config();
        apply(&path, config.programs.vscode.as_ref(), ReconcileMode::Apply)
            .expect("upgrade VS Code settings state");
        let settings = fs::read_to_string(&path).unwrap();
        assert!(settings.contains("\"debug.overrideCapiUrl\": \"http://127.0.0.1:4002\""));

        apply(&path, None, ReconcileMode::Apply).expect("restore VS Code settings");
        let settings = fs::read_to_string(&path).unwrap();
        assert!(settings.contains("https://previous-proxy.example.com/v1"));
        assert!(settings.contains("https://previous-capi.example.com"));
        assert!(!json_merge::state_path(&path).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
