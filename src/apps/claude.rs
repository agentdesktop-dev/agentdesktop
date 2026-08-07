use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

const CONNECTOR_BASE_URL: &str = "http://127.0.0.1:8080";
const PLACEHOLDER_CREDENTIAL: &str = "local-gateway-placeholder";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionStatus {
    Connected,
    AlreadyConnected,
    NotInstalled,
}

pub fn connect_installed() -> Result<ConnectionStatus> {
    if !is_installed()? {
        return Ok(ConnectionStatus::NotInstalled);
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    connect_settings(&PathBuf::from(home).join(".claude/settings.json"))
}

pub fn is_installed() -> Result<bool> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    if home.join(".local/bin/claude").is_file() {
        return Ok(true);
    }
    Ok(env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| directory.join("claude").is_file())
    }))
}

pub fn ensure_capture_routing_is_clear() -> Result<()> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    ensure_capture_routing_is_clear_for(
        env::var_os("ANTHROPIC_BASE_URL").as_deref(),
        &PathBuf::from(home).join(".claude/settings.json"),
    )
}

fn ensure_capture_routing_is_clear_for(
    base_url: Option<&OsStr>,
    settings_path: &Path,
) -> Result<()> {
    if base_url.is_some() {
        bail!(
            "Claude capture cannot start while ANTHROPIC_BASE_URL is set; run Claude normally for configured routing, or unset it before using transparent capture"
        );
    } 
    if !settings_path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(settings_path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "Claude Code settings {} is not a regular file",
            settings_path.display()
        );
    }
    let settings =
        serde_json::from_slice::<Value>(&fs::read(settings_path)?).with_context(|| {
            format!(
                "Claude Code settings {} is not valid JSON",
                settings_path.display()
            )
        })?;
    let root = settings.as_object().with_context(|| {
        format!(
            "Claude Code settings {} must contain a JSON object",
            settings_path.display()
        )
    })?;
    if let Some(environment) = root.get("env") {
        let environment = environment.as_object().with_context(|| {
            format!(
                "Claude Code settings {} has a non-object env setting",
                settings_path.display()
            )
        })?;
        if environment.contains_key("ANTHROPIC_BASE_URL") {
            bail!(
                "Claude capture cannot start while {} configures ANTHROPIC_BASE_URL; run Claude normally for configured routing, or remove that setting before using transparent capture",
                settings_path.display()
            );
        }
    }
    Ok(())
}

fn connect_settings(path: &Path) -> Result<ConnectionStatus> {
    let mut settings = if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!(
                "Claude Code settings {} is not a regular file",
                path.display()
            );
        }
        serde_json::from_slice::<Value>(&fs::read(path)?)
            .with_context(|| format!("Claude Code settings {} is not valid JSON", path.display()))?
    } else {
        Value::Object(Map::new())
    };
    let root = settings.as_object_mut().with_context(|| {
        format!(
            "Claude Code settings {} must contain a JSON object",
            path.display()
        )
    })?;
    let environment = root
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .with_context(|| {
            format!(
                "Claude Code settings {} has a non-object env setting",
                path.display()
            )
        })?;

    let desired = [
        ("ANTHROPIC_BASE_URL", CONNECTOR_BASE_URL),
        ("ANTHROPIC_API_KEY", PLACEHOLDER_CREDENTIAL),
    ];
    if desired
        .iter()
        .all(|(name, value)| environment.get(*name).and_then(Value::as_str) == Some(*value))
    {
        return Ok(ConnectionStatus::AlreadyConnected);
    }
    for (name, value) in desired {
        if let Some(existing) = environment.get(name)
            && existing.as_str() != Some(value)
        {
            bail!(
                "Claude Code already configures {name}; disconnect its existing provider or gateway before connecting Agent Desktop"
            );
        }
        environment.insert(name.to_owned(), Value::String(value.to_owned()));
    }

    let parent = path
        .parent()
        .context("Claude Code settings has no parent")?;
    fs::create_dir_all(parent)?;
    let mut encoded = serde_json::to_vec_pretty(&settings)?;
    encoded.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temporary.write_all(&encoded)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to update Claude Code settings {}", path.display()))?;
    Ok(ConnectionStatus::Connected)
}

#[cfg(test)]
mod tests {
    use super::{
        CONNECTOR_BASE_URL, ConnectionStatus, PLACEHOLDER_CREDENTIAL, connect_settings,
        ensure_capture_routing_is_clear_for,
    };
    use std::ffi::OsStr;
    use std::fs;

    use serde_json::{Value, json};

    #[test]
    fn connects_claude_without_replacing_other_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            serde_json::to_vec(&json!({"theme": "light"})).unwrap(),
        )
        .unwrap();

        assert_eq!(
            connect_settings(&settings).unwrap(),
            ConnectionStatus::Connected
        );
        assert_eq!(
            connect_settings(&settings).unwrap(),
            ConnectionStatus::AlreadyConnected
        );
        let saved: Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
        assert_eq!(saved["theme"], "light");
        assert_eq!(saved["env"]["ANTHROPIC_BASE_URL"], CONNECTOR_BASE_URL);
        assert_eq!(saved["env"]["ANTHROPIC_API_KEY"], PLACEHOLDER_CREDENTIAL);
    }

    #[test]
    fn refuses_to_replace_existing_claude_provider() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join("settings.json");
        fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "env": {"ANTHROPIC_BASE_URL": "https://existing.example"}
            }))
            .unwrap(),
        )
        .unwrap();

        let error = connect_settings(&settings).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already configures ANTHROPIC_BASE_URL")
        );
        let saved: Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
        assert_eq!(
            saved["env"]["ANTHROPIC_BASE_URL"],
            "https://existing.example"
        );
        assert!(saved["env"].get("ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn capture_rejects_environment_routing() {
        let temporary = tempfile::tempdir().unwrap();
        let error = ensure_capture_routing_is_clear_for(
            Some(OsStr::new("http://127.0.0.1:8080")),
            &temporary.path().join("settings.json"),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("ANTHROPIC_BASE_URL is set"));
    }

    #[test]
    fn capture_rejects_persistent_routing() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join("settings.json");
        fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "env": {"ANTHROPIC_BASE_URL": CONNECTOR_BASE_URL}
            }))
            .unwrap(),
        )
        .unwrap();

        let error = ensure_capture_routing_is_clear_for(None, &settings)
            .unwrap_err()
            .to_string();

        assert!(error.contains("configures ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn capture_accepts_unrelated_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join("settings.json");
        fs::write(
            &settings,
            serde_json::to_vec(&json!({"theme": "light"})).unwrap(),
        )
        .unwrap();

        ensure_capture_routing_is_clear_for(None, &settings).unwrap();
    }

    #[test]
    fn capture_rejects_unverifiable_settings_shape() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join("settings.json");
        fs::write(&settings, serde_json::to_vec(&json!({"env": []})).unwrap()).unwrap();

        let error = ensure_capture_routing_is_clear_for(None, &settings)
            .unwrap_err()
            .to_string();

        assert!(error.contains("non-object env setting"));
    }
}
