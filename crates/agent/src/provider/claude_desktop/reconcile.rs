use super::ClaudeDesktop;

use std::path::Path;

use agentdesktop_core::config::{ClaudeDesktopConfig, LlmGatewayAuthentication, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::debug;

use crate::reconcile::ReconcilePlan;
#[cfg(any(windows, test))]
use crate::reconcile::registry::{RegistryKey, RegistryKeyPlan, RegistryValues};

#[cfg(any(not(windows), test))]
use crate::provider::shared::shell_quote;
use crate::provider::{json_merge, shared::deep_merge};
#[cfg(any(windows, test))]
use std::collections::{BTreeMap, BTreeSet};

const OWNER_MARKER: &[u8] = b"Agentdesktop\n";

/// The machine policy key Claude Desktop reads on Windows: flat `REG_SZ`
/// values directly under it (the app never reads subkeys). Any value
/// directly under the key — not the key itself — counts as machine policy
/// being present and makes the app ignore `HKCU` policy entirely.
#[cfg(any(windows, test))]
pub(super) const POLICY_KEY: &str = r"SOFTWARE\Policies\Claude";

/// The `REG_SZ` value under the policy key that marks what agentdesktop
/// manages, like the `.owner` sidecar next to a managed file: its data is a
/// JSON array of the value names this daemon wrote, so updates and removal
/// touch nothing else. To Claude Desktop the marker is a policy value like
/// any other — its presence alone switches the app to machine policy — so
/// it is never written on its own: an empty managed set plans removal
/// instead (see [`plan_registry_settings`]).
#[cfg(any(windows, test))]
const REGISTRY_OWNER_VALUE: &str = ".agentdesktop.owner";

pub(super) fn plan(
    desktop: &ClaudeDesktop,
    merge_existing: bool,
    credential_binary: &Path,
    socket: &Path,
    config: Option<(&ClaudeDesktopConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let settings_path = desktop.managed_settings_path.as_path();
    let helper_path = desktop.credential_helper_path.as_path();
    let settings_owner = owner_path(settings_path);
    let settings_state = json_merge::state_path(settings_path);
    let helper_owner = owner_path(helper_path);
    let Some((config, gateway)) = config else {
        if merge_existing
            && json_merge::plan_remove(
                settings_path,
                &settings_state,
                "managed settings",
                ClaudeDesktop::DISPLAY_NAME,
                plan,
            )?
        {
            remove_owner_marker(&settings_owner, plan)?;
        } else {
            #[cfg(windows)]
            remove_legacy_settings_file(settings_path, &settings_owner, plan)?;
            #[cfg(not(windows))]
            remove_owned(settings_path, &settings_owner, "managed settings", plan)?;
        }
        // On Windows the managed settings are machine policy registry
        // values; the file above is only what an agentdesktop without the
        // registry target wrote. A user daemon never wrote the policy key.
        #[cfg(windows)]
        if !merge_existing {
            let observed = crate::reconcile::registry::read_values(&desktop.policy_key)
                .with_context(|| format!("read {}", desktop.policy_key.display_path().display()))?;
            plan_registry_remove(&desktop.policy_key, observed, plan);
        }
        return remove_owned(helper_path, &helper_owner, "credential helper", plan);
    };

    let uses_credential_helper = gateway.is_some_and(|gateway| {
        gateway
            .authentication
            .as_ref()
            .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    });
    if uses_credential_helper {
        let script = credential_helper_contents(credential_binary, socket)?;
        write_owned(
            helper_path,
            &helper_owner,
            &script,
            0o755,
            "credential helper",
            plan,
        )?;
    } else {
        remove_owned(helper_path, &helper_owner, "credential helper", plan)?;
    }

    let settings = managed_settings(config, gateway, helper_path)?;
    if merge_existing {
        anyhow::bail!(
            "Claude Desktop settings cannot be applied to its user preferences; use system-managed settings"
        );
    }
    plan_managed_settings(desktop, &settings_owner, &settings, plan)
}

/// Plans the managed-settings target Claude Desktop reads on this platform:
/// the machine policy registry key on Windows, a managed file everywhere
/// else.
#[cfg(windows)]
fn plan_managed_settings(
    desktop: &ClaudeDesktop,
    settings_owner: &Path,
    settings: &Value,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    remove_legacy_settings_file(&desktop.managed_settings_path, settings_owner, plan)?;
    let observed = crate::reconcile::registry::read_values(&desktop.policy_key)
        .with_context(|| format!("read {}", desktop.policy_key.display_path().display()))?;
    plan_registry_settings(
        &desktop.policy_key,
        observed,
        &registry_values(settings)?,
        plan,
    );
    Ok(())
}

#[cfg(not(windows))]
fn plan_managed_settings(
    desktop: &ClaudeDesktop,
    settings_owner: &Path,
    settings: &Value,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let contents = serialize_managed_settings(settings)?;
    write_owned(
        &desktop.managed_settings_path,
        settings_owner,
        &contents,
        0o644,
        "managed settings",
        plan,
    )
}

/// Removes the managed-settings file an agentdesktop without the registry
/// target wrote at the path. Claude Desktop never read a file on Windows,
/// so when no owned file is there nothing is recorded — recording would
/// count the obsolete path as "unchanged" in every dry run.
#[cfg(windows)]
fn remove_legacy_settings_file(
    path: &Path,
    owner_path: &Path,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    if !is_owned(owner_path, plan)? {
        return Ok(());
    }
    remove_owned(path, owner_path, "managed settings", plan)
}

/// Serializes managed settings into the format Claude Desktop actually reads on this
/// platform. On macOS that's a property list — `CFPreferencesCopyAppValue` never consults
/// a JSON file — everywhere else it's pretty-printed JSON.
#[cfg(target_os = "macos")]
fn serialize_managed_settings(settings: &Value) -> anyhow::Result<Vec<u8>> {
    let mut contents = Vec::new();
    plist::to_writer_xml(&mut contents, settings)
        .context("serialize Claude Desktop managed settings as a property list")?;
    Ok(contents)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn serialize_managed_settings(settings: &Value) -> anyhow::Result<Vec<u8>> {
    let mut contents =
        serde_json::to_vec_pretty(settings).context("serialize Claude Desktop managed settings")?;
    contents.push(b'\n');
    Ok(contents)
}

/// Flattens managed settings into the `REG_SZ` data Claude Desktop reads on
/// Windows: a string value as it is, every other value as its JSON text —
/// the encoding the configuration reference specifies for MDM payloads.
#[cfg(any(windows, test))]
fn registry_values(settings: &Value) -> anyhow::Result<BTreeMap<String, String>> {
    let Value::Object(settings) = settings else {
        anyhow::bail!("Claude Desktop managed settings serialize to an object");
    };
    let mut values = BTreeMap::new();
    let mut folded: BTreeMap<String, &str> = BTreeMap::new();
    for (name, value) in settings {
        // Value names are case-insensitive to the registry, so every case
        // variant of the marker's name is reserved.
        if name.eq_ignore_ascii_case(REGISTRY_OWNER_VALUE) {
            anyhow::bail!(
                "Claude Desktop setting name {REGISTRY_OWNER_VALUE} is reserved for agentdesktop's ownership marker"
            );
        }
        // `RegSetValueExW` reads the name up to its first NUL: an embedded
        // NUL would write a shorter name than conflict checking compared and
        // the ownership marker records.
        if name.contains('\0') {
            anyhow::bail!("Claude Desktop setting name {name:?} contains an embedded NUL");
        }
        // The registry's maximum value-name length (one less than the
        // `read_values` name buffer). The registry refuses a longer name at
        // write time; refused here instead, before anything is written.
        if name.encode_utf16().count() > 16_383 {
            anyhow::bail!(
                "Claude Desktop setting name of {} characters is longer than a registry value name allows",
                name.chars().count()
            );
        }
        // Two names differing only in case address one registry value, so
        // every reconciliation would rewrite it with whichever came later.
        if let Some(previous) = folded.insert(name.to_ascii_lowercase(), name) {
            anyhow::bail!(
                "Claude Desktop setting names {previous} and {name} differ only in case and address the same registry value"
            );
        }
        let data = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        // `REG_SZ` data is NUL-terminated too; JSON-encoded data escapes
        // every NUL, only a pass-through string can carry one.
        if data.contains('\0') {
            anyhow::bail!("Claude Desktop setting {name} contains an embedded NUL");
        }
        values.insert(name.clone(), data);
    }
    Ok(values)
}

/// Looks `name` up among the observed values the way the registry itself
/// matches value names: case-insensitively. ASCII covers every name
/// agentdesktop writes; the registry's own folding is locale-invariant
/// Unicode, so a non-ASCII case variant is treated as a different name and
/// errs on the side of writing it.
#[cfg(any(windows, test))]
fn observed_value<'a>(observed: &'a RegistryValues, name: &str) -> Option<&'a Option<String>> {
    observed
        .iter()
        .find_map(|(existing, data)| existing.eq_ignore_ascii_case(name).then_some(data))
}

/// Whether a value with `name` exists under the key, case-insensitively.
#[cfg(any(windows, test))]
fn observes_name(observed: &RegistryValues, name: &str) -> bool {
    observed
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
}

/// The value names the ownership marker claims; empty when the marker is
/// missing or unreadable (nothing is owned, like a missing `.owner` sidecar).
#[cfg(any(windows, test))]
fn registry_owned_values(observed: &RegistryValues) -> BTreeSet<String> {
    observed_value(observed, REGISTRY_OWNER_VALUE)
        .and_then(|data| data.as_deref())
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default()
}

/// The named values that exist under the key, as the JSON object dry-run
/// diffs display; a value of a non-string registry type reads as `null`.
#[cfg(any(windows, test))]
fn registry_report<'a>(
    names: impl IntoIterator<Item = &'a String>,
    observed: &RegistryValues,
) -> Vec<u8> {
    let values: serde_json::Map<String, Value> = names
        .into_iter()
        .filter_map(|name| {
            let data = observed_value(observed, name)?;
            Some((
                name.clone(),
                data.clone().map_or(Value::Null, Value::String),
            ))
        })
        .collect();
    Value::Object(values).to_string().into_bytes()
}

/// `desired` as the JSON object dry-run diffs display.
#[cfg(any(windows, test))]
fn registry_report_desired(desired: &BTreeMap<String, String>) -> Vec<u8> {
    let values: serde_json::Map<String, Value> = desired
        .iter()
        .map(|(name, data)| (name.clone(), Value::String(data.clone())))
        .collect();
    Value::Object(values).to_string().into_bytes()
}

/// Plans the registry writes that bring the Windows policy key to `desired`,
/// with the ownership semantics of `write_owned`: a value whose data already
/// matches is claimed as ours, a differing value the marker does not claim
/// is a conflict, and names the marker claims but `desired` dropped are
/// deleted. The marker never claims anything the plan does not touch, and
/// it is written before the values it claims (see the comment at the marker
/// changes below). Value names match case-insensitively, as the registry
/// stores them.
///
/// An empty `desired` plans like removal: the ownership marker alone would
/// switch Claude Desktop to machine policy and away from the user's `HKCU`
/// policy, so with nothing to manage no key — not even a marker-only one —
/// is left behind.
#[cfg(any(windows, test))]
fn plan_registry_settings(
    key: &RegistryKey,
    observed: RegistryValues,
    desired: &BTreeMap<String, String>,
    plan: &ReconcilePlan,
) {
    if desired.is_empty() {
        return plan_registry_remove(key, observed, plan);
    }
    let path = key.display_path();
    let owned = registry_owned_values(&observed);
    let before = registry_report(desired.keys().chain(owned.iter()), &observed);
    let after = registry_report_desired(desired);
    let mut changes = Vec::new();
    for (name, data) in desired {
        match observed_value(&observed, name) {
            // Identical data is claimed as ours, like identical file bytes.
            Some(Some(existing)) if existing == data => {}
            Some(_)
                if !owned
                    .iter()
                    .any(|claimed| claimed.eq_ignore_ascii_case(name)) =>
            {
                plan.record_diff(
                    ClaudeDesktop::DISPLAY_NAME,
                    "managed settings",
                    "conflict",
                    &path,
                    Some(&before),
                    Some(&after),
                );
                return;
            }
            _ => changes.push((name.clone(), Some(data.clone()))),
        }
    }
    let stale: Vec<&String> = owned
        .iter()
        .filter(|name| !desired.keys().any(|kept| kept.eq_ignore_ascii_case(name)))
        .collect();
    for name in &stale {
        if observes_name(&observed, name) {
            changes.push(((*name).clone(), None));
        }
    }
    // The marker brackets the writes so a partial apply stays recoverable:
    // the first change claims every name this plan writes or deletes, so a
    // failure midway leaves everything already touched claimed — removal
    // still finds it and the next plan retries it. Once the stale names are
    // gone the claim shrinks to exactly `desired`'s names, written last.
    let full_claim = serde_json::to_string(
        &desired
            .keys()
            .chain(stale.iter().copied())
            .collect::<BTreeSet<_>>(),
    )
    .expect("value names serialize");
    let final_claim =
        serde_json::to_string(&desired.keys().collect::<Vec<_>>()).expect("value names serialize");
    if observed_value(&observed, REGISTRY_OWNER_VALUE).and_then(|data| data.as_deref())
        != Some(full_claim.as_str())
    {
        changes.insert(
            0,
            (REGISTRY_OWNER_VALUE.to_owned(), Some(full_claim.clone())),
        );
    }
    if final_claim != full_claim {
        changes.push((REGISTRY_OWNER_VALUE.to_owned(), Some(final_claim)));
    }
    let action = if changes.iter().any(|(name, _)| name != REGISTRY_OWNER_VALUE) {
        if observed.is_empty() {
            "create"
        } else {
            "update"
        }
    } else {
        "unchanged"
    };
    if !changes.is_empty() {
        plan.plan_registry_key(RegistryKeyPlan {
            key: key.clone(),
            observed,
            changes,
            delete_key_when_empty: false,
        });
    }
    debug!(program = ClaudeDesktop::ID, action, path = %path.display(), "planned managed settings");
    plan.record_diff(
        ClaudeDesktop::DISPLAY_NAME,
        "managed settings",
        action,
        &path,
        Some(&before),
        Some(&after),
    );
}

/// Plans the removal of the registry values the ownership marker claims and
/// of the marker itself; the key is deleted at apply time only when nothing
/// else remains under it. Without a marker nothing is owned and nothing is
/// touched.
#[cfg(any(windows, test))]
fn plan_registry_remove(key: &RegistryKey, observed: RegistryValues, plan: &ReconcilePlan) {
    let path = key.display_path();
    if !observes_name(&observed, REGISTRY_OWNER_VALUE) {
        plan.record_diff(
            ClaudeDesktop::DISPLAY_NAME,
            "managed settings",
            "unchanged",
            &path,
            None,
            None,
        );
        return;
    }
    let owned = registry_owned_values(&observed);
    let before = registry_report(owned.iter(), &observed);
    let mut changes: Vec<(String, Option<String>)> = owned
        .iter()
        .filter(|name| observes_name(&observed, name))
        .map(|name| (name.clone(), None))
        .collect();
    changes.push((REGISTRY_OWNER_VALUE.to_owned(), None));
    debug!(program = ClaudeDesktop::ID, action = "remove", path = %path.display(), "planned managed settings");
    plan.record_diff(
        ClaudeDesktop::DISPLAY_NAME,
        "managed settings",
        "remove",
        &path,
        Some(&before),
        None,
    );
    plan.plan_registry_key(RegistryKeyPlan {
        key: key.clone(),
        observed,
        changes,
        delete_key_when_empty: true,
    });
}

fn credential_helper_contents(credential_binary: &Path, socket: &Path) -> anyhow::Result<Vec<u8>> {
    #[cfg(windows)]
    return windows_credential_helper_contents(
        &credential_binary.to_string_lossy(),
        &socket.to_string_lossy(),
    );
    #[cfg(not(windows))]
    return Ok(posix_credential_helper_contents(
        &credential_binary.to_string_lossy(),
        &socket.to_string_lossy(),
    ));
}

#[cfg(any(not(windows), test))]
fn posix_credential_helper_contents(credential_binary: &str, socket: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nexec {} --socket {} credential --client-id claude-desktop\n",
        shell_quote(credential_binary),
        shell_quote(socket)
    )
    .into_bytes()
}

#[cfg(any(windows, test))]
fn windows_credential_helper_contents(
    credential_binary: &str,
    socket: &str,
) -> anyhow::Result<Vec<u8>> {
    Ok(format!(
        "@echo off\r\n{} --socket {} credential --client-id claude-desktop\r\n",
        batch_quote(credential_binary)?,
        batch_quote(socket)?
    )
    .into_bytes())
}

#[cfg(any(windows, test))]
fn batch_quote(value: &str) -> anyhow::Result<String> {
    if value.contains(['"', '\r', '\n']) {
        anyhow::bail!("Windows helper argument contains an unsupported quote or newline");
    }
    Ok(format!("\"{}\"", value.replace('%', "%%")))
}

fn managed_settings(
    config: &ClaudeDesktopConfig,
    gateway: Option<&LlmGatewayConfig>,
    helper_path: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.settings)
        .context("serialize Claude Desktop pass-through settings")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };
    let mut generated = json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": gateway.url.as_str(),
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    {
        generated["inferenceCredentialKind"] = json!("helper-script");
        generated["inferenceCredentialHelper"] = json!(helper_path);
        generated["inferenceCredentialHelperTtlSec"] = json!(60);
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn owner_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("managed");
    path.with_file_name(format!(".{name}.owner"))
}

fn write_owned(
    path: &Path,
    owner_path: &Path,
    contents: &[u8],
    permissions: u32,
    description: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let owned = is_owned(owner_path, plan)?;
    let existing = match plan.read(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let action = match existing.as_deref() {
        Some(existing)
            if existing == contents && !crate::reconcile::grants_beyond(path, permissions) =>
        {
            if !owned {
                plan.write_file(owner_path, OWNER_MARKER, 0o644)?;
            }
            "unchanged"
        }
        // Identical bytes at a mode looser than planned are rewritten, like
        // an owned file (the arm above claims identical bytes as ours).
        Some(existing) if owned || existing == contents => "update",
        Some(existing) => {
            plan.record_diff(
                ClaudeDesktop::DISPLAY_NAME,
                description,
                "conflict",
                path,
                Some(existing),
                Some(contents),
            );
            return Ok(());
        }
        None => "create",
    };
    if action != "unchanged" {
        plan.write_file(path, contents, permissions)?;
        plan.write_file(owner_path, OWNER_MARKER, 0o644)?;
    }
    debug!(program = ClaudeDesktop::ID, action, path = %path.display(), "planned {description}");
    plan.record_diff(
        ClaudeDesktop::DISPLAY_NAME,
        description,
        action,
        path,
        existing.as_deref(),
        Some(contents),
    );
    Ok(())
}

fn remove_owned(
    path: &Path,
    owner_path: &Path,
    description: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    if !is_owned(owner_path, plan)? {
        plan.record(ClaudeDesktop::DISPLAY_NAME, description, "unchanged", path);
        return Ok(());
    }
    let exists = match plan.read(path) {
        Ok(_) => {
            plan.remove_file(path)
                .with_context(|| format!("remove {}", path.display()))?;
            debug!(program = ClaudeDesktop::ID, action = "remove", path = %path.display(), "planned {description}");
            plan.record(ClaudeDesktop::DISPLAY_NAME, description, "remove", path);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if !exists {
        plan.record(ClaudeDesktop::DISPLAY_NAME, description, "unchanged", path);
    }
    match plan.remove_file(owner_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", owner_path.display())),
    }
}

fn is_owned(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<bool> {
    match plan.read(path) {
        Ok(contents) => Ok(contents == OWNER_MARKER),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn remove_owner_marker(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<()> {
    match plan.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    #[cfg(target_os = "macos")]
    use super::serialize_managed_settings;
    use super::{
        batch_quote, managed_settings, posix_credential_helper_contents,
        windows_credential_helper_contents,
    };
    use crate::reconcile::ReconcilePlan;
    use crate::reconcile::registry::{RegistryKey, RegistryValues};
    use agentdesktop_core::config::parse_daemon;

    #[test]
    fn generates_platform_specific_credential_helpers() {
        let binary = r"C:\Program Files\Agent Desktop\agentdesktop.exe";
        let socket = r"\\.\pipe\agentdesktop";

        let windows = windows_credential_helper_contents(binary, socket).expect("Windows helper");
        assert_eq!(
            String::from_utf8(windows).unwrap(),
            "@echo off\r\n\"C:\\Program Files\\Agent Desktop\\agentdesktop.exe\" --socket \"\\\\.\\pipe\\agentdesktop\" credential --client-id claude-desktop\r\n"
        );

        let posix = posix_credential_helper_contents(
            "/opt/Agent Desktop/agentdesktop",
            "/run/agentdesktop.sock",
        );
        assert_eq!(
            String::from_utf8(posix).unwrap(),
            "#!/bin/sh\nexec '/opt/Agent Desktop/agentdesktop' --socket '/run/agentdesktop.sock' credential --client-id claude-desktop\n"
        );
    }

    #[test]
    fn escapes_batch_expansion_and_rejects_unrepresentable_arguments() {
        assert_eq!(
            batch_quote(r"C:\Agent%TEMP%\agentdesktop.exe").unwrap(),
            r#""C:\Agent%%TEMP%%\agentdesktop.exe""#
        );
        assert!(batch_quote("bad\"path").is_err());
        assert!(batch_quote("bad\npath").is_err());
    }

    #[test]
    fn gateway_settings_override_pass_through_values() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-desktop]
programs:
  claudeDesktop:
    isLocalDevMcpEnabled: true
    inferenceProvider: ignored
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings =
            managed_settings(desktop, config.llm_gateway.as_ref(), Path::new("/helper")).unwrap();
        assert_eq!(settings["isLocalDevMcpEnabled"], true);
        assert_eq!(settings["inferenceProvider"], "gateway");
        assert_eq!(settings["inferenceCredentialHelper"], "/helper");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_serializes_managed_settings_as_a_property_list() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-desktop]
programs:
  claudeDesktop: {}
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings =
            managed_settings(desktop, config.llm_gateway.as_ref(), Path::new("/helper")).unwrap();

        let contents = serialize_managed_settings(&settings).expect("plist serializes");
        assert!(
            contents.starts_with(b"<?xml"),
            "expected XML property list, got {:?}",
            String::from_utf8_lossy(&contents)
        );

        let parsed: plist::Value = plist::from_bytes(&contents).expect("plist parses back");
        let dict = parsed.as_dictionary().expect("top-level dictionary");
        assert_eq!(
            dict.get("inferenceProvider")
                .and_then(plist::Value::as_string),
            Some("gateway")
        );
        assert_eq!(
            dict.get("inferenceGatewayBaseUrl")
                .and_then(plist::Value::as_string),
            Some("https://gateway.example.com/")
        );
    }

    #[cfg(unix)]
    fn mode_repair_fixture() -> (tempfile::TempDir, agentdesktop_core::config::DaemonConfig) {
        let config = parse_daemon(
            r#"
programs:
  claudeDesktop: {}
"#,
        )
        .expect("valid daemon configuration");
        (tempfile::tempdir().unwrap(), config)
    }

    #[test]
    #[cfg(unix)]
    fn identical_bytes_with_looser_mode_is_an_update_at_the_planned_mode() {
        let (dir, config) = mode_repair_fixture();
        let path = dir.path().join("managed-settings.json");
        let marker = super::owner_path(&path);
        let desktop = super::ClaudeDesktop {
            managed_settings_path: path.clone(),
            credential_helper_path: dir.path().join("helper.sh"),
        };
        // System mode: `merge_existing = false`.
        let plan_fn = |plan: &crate::reconcile::ReconcilePlan| {
            super::plan(
                &desktop,
                false,
                Path::new("agentdesktop"),
                Path::new("agentdesktop.sock"),
                Some((config.programs.claude_desktop.as_ref().unwrap(), None)),
                plan,
            )
        };
        crate::reconcile::plan::mode_repair::assert_identical_bytes_with_looser_mode_is_an_update(
            &[&path, &marker],
            0o644,
            &plan_fn,
        );
    }

    #[test]
    #[cfg(unix)]
    fn identical_bytes_within_mode_is_unchanged() {
        let (dir, config) = mode_repair_fixture();
        let path = dir.path().join("managed-settings.json");
        let marker = super::owner_path(&path);
        let desktop = super::ClaudeDesktop {
            managed_settings_path: path.clone(),
            credential_helper_path: dir.path().join("helper.sh"),
        };
        // System mode: `merge_existing = false`.
        let plan_fn = |plan: &crate::reconcile::ReconcilePlan| {
            super::plan(
                &desktop,
                false,
                Path::new("agentdesktop"),
                Path::new("agentdesktop.sock"),
                Some((config.programs.claude_desktop.as_ref().unwrap(), None)),
                plan,
            )
        };
        crate::reconcile::plan::mode_repair::assert_identical_bytes_within_mode_is_unchanged(
            &[&path, &marker],
            &plan_fn,
        );
    }

    // --- Windows policy registry target --------------------------------------

    #[test]
    fn registry_values_encode_strings_literally_and_the_rest_as_json() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-desktop]
programs:
  claudeDesktop:
    isLocalDevMcpEnabled: true
    coworkEgressAllowedHosts: ["example.com"]
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings =
            managed_settings(desktop, config.llm_gateway.as_ref(), Path::new("/helper")).unwrap();

        let values = super::registry_values(&settings).unwrap();
        assert_eq!(values["inferenceProvider"], "gateway");
        assert_eq!(
            values["inferenceGatewayBaseUrl"],
            "https://gateway.example.com/"
        );
        assert_eq!(values["isLocalDevMcpEnabled"], "true");
        assert_eq!(values["inferenceCredentialHelperTtlSec"], "60");
        assert_eq!(values["coworkEgressAllowedHosts"], r#"["example.com"]"#);
    }

    #[test]
    fn registry_values_reject_the_reserved_marker_name() {
        // Value names are case-insensitive to the registry, so a case
        // variant would overwrite the marker all the same.
        for name in [".agentdesktop.owner", ".AgentDesktop.Owner"] {
            let error = super::registry_values(&serde_json::json!({ name: "x" })).unwrap_err();
            assert!(error.to_string().contains("reserved"), "{name}: {error}");
        }
    }

    #[test]
    fn registry_values_reject_an_embedded_nul() {
        // `RegSetValueExW` would truncate the name at the NUL and overwrite
        // the value called `foreign`, which conflict checking compared
        // against the full name and removal could never find again.
        let error = super::registry_values(&serde_json::json!({ "foreign\0suffix": "x" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("embedded NUL"), "{error}");

        // `REG_SZ` data is NUL-terminated the same way.
        let error = super::registry_values(&serde_json::json!({ "name": "trun\0cated" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("embedded NUL"), "{error}");
    }

    #[test]
    fn registry_values_reject_a_name_past_the_registry_limit() {
        // 16,383 UTF-16 units is the registry's maximum value-name length;
        // one more would make `RegSetValueExW` refuse at apply time, after
        // earlier values of the plan were already written.
        let at_limit = "a".repeat(16_383);
        assert!(super::registry_values(&serde_json::json!({ at_limit: "x" })).is_ok());
        let past = "a".repeat(16_384);
        let error = super::registry_values(&serde_json::json!({ past: "x" }))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("longer than a registry value name"),
            "{error}"
        );
    }

    #[test]
    fn registry_values_reject_names_differing_only_in_case() {
        // Both names address the same registry value, so the reconciler
        // would rewrite it on every tick with whichever came later and
        // never converge. The realistic pair: a pass-through
        // `inferenceprovider` next to the generated `inferenceProvider`.
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-desktop]
programs:
  claudeDesktop:
    inferenceprovider: direct
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings =
            managed_settings(desktop, config.llm_gateway.as_ref(), Path::new("/helper")).unwrap();
        let error = super::registry_values(&settings).unwrap_err().to_string();
        assert!(error.contains("differ only in case"), "{error}");
        assert!(error.contains("inferenceprovider"), "{error}");
        assert!(error.contains("inferenceProvider"), "{error}");
    }

    #[test]
    fn registry_plan_retries_a_write_the_marker_claims_but_the_key_lacks() {
        // The state a partial apply leaves behind: the marker (written
        // first) claims a name whose value never landed. The next plan
        // writes the missing value without a conflict.
        let observed = RegistryValues::from([
            ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
            (
                ".agentdesktop.owner".to_owned(),
                Some(r#"["inferenceProvider","isLocalDevMcpEnabled"]"#.to_owned()),
            ),
        ]);
        let desired = BTreeMap::from([
            ("inferenceProvider".to_owned(), "gateway".to_owned()),
            ("isLocalDevMcpEnabled".to_owned(), "true".to_owned()),
        ]);
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, observed, &desired, &plan);

        assert!(!plan.has_conflicts());
        assert_eq!(
            plan.registry_key_plans()[0].changes,
            vec![("isLocalDevMcpEnabled".to_owned(), Some("true".to_owned()))]
        );
    }

    #[test]
    fn registry_plan_creates_values_and_the_ownership_marker() {
        let desired = BTreeMap::from([
            ("inferenceProvider".to_owned(), "gateway".to_owned()),
            ("isLocalDevMcpEnabled".to_owned(), "true".to_owned()),
        ]);
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, RegistryValues::new(), &desired, &plan);

        let plans = plan.registry_key_plans();
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].delete_key_when_empty);
        // The marker comes first: a create that fails midway leaves every
        // value already written claimed, so removal still finds it.
        assert_eq!(
            plans[0].changes,
            vec![
                (
                    ".agentdesktop.owner".to_owned(),
                    Some(r#"["inferenceProvider","isLocalDevMcpEnabled"]"#.to_owned())
                ),
                ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
                ("isLocalDevMcpEnabled".to_owned(), Some("true".to_owned())),
            ]
        );
        let rendered = plan.render();
        assert!(rendered.contains("CREATE  Claude Desktop managed settings"));
        assert!(rendered.contains(r"HKLM\SOFTWARE\Policies\Claude"));
    }

    #[test]
    fn registry_plan_updates_owned_and_deletes_stale_values() {
        let observed = RegistryValues::from([
            ("inferenceProvider".to_owned(), Some("direct".to_owned())),
            ("stale".to_owned(), Some("old".to_owned())),
            (
                ".agentdesktop.owner".to_owned(),
                Some(r#"["inferenceProvider","stale"]"#.to_owned()),
            ),
        ]);
        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, observed, &desired, &plan);

        assert!(!plan.has_conflicts());
        assert_eq!(
            plan.registry_key_plans()[0].changes,
            vec![
                ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
                ("stale".to_owned(), None),
                (
                    ".agentdesktop.owner".to_owned(),
                    Some(r#"["inferenceProvider"]"#.to_owned())
                ),
            ]
        );
        assert!(
            plan.render()
                .contains("UPDATE  Claude Desktop managed settings")
        );
    }

    #[test]
    fn registry_plan_adopts_identical_values_with_a_marker_write() {
        let observed =
            RegistryValues::from([("inferenceProvider".to_owned(), Some("gateway".to_owned()))]);
        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
        let key = RegistryKey::current_user(r"Software\AgentdesktopTest");
        assert!(
            key.display_path()
                .to_string_lossy()
                .starts_with(r"HKCU\Software")
        );
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, observed, &desired, &plan);

        assert_eq!(
            plan.registry_key_plans()[0].changes,
            vec![(
                ".agentdesktop.owner".to_owned(),
                Some(r#"["inferenceProvider"]"#.to_owned())
            )]
        );
        assert!(plan.render().contains("Summary: 0 changes, 1 unchanged"));
    }

    #[test]
    fn registry_plan_conflicts_on_a_foreign_value() {
        // A differing value the marker does not claim, and a value of a
        // non-string registry type, both refuse the plan. The name matches
        // case-insensitively, as the registry stores it: a case-variant
        // foreign value is the same value, not a writable second one.
        for name in ["inferenceProvider", "InferenceProvider"] {
            for foreign in [Some("direct".to_owned()), None] {
                let observed = RegistryValues::from([(name.to_owned(), foreign)]);
                let desired =
                    BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
                let key = RegistryKey::local_machine(super::POLICY_KEY);
                let plan = ReconcilePlan::default();
                super::plan_registry_settings(&key, observed, &desired, &plan);

                assert!(plan.has_conflicts(), "{name}");
                assert!(plan.registry_key_plans().is_empty(), "{name}");
            }
        }
    }

    #[test]
    fn registry_plan_matches_owned_names_case_insensitively() {
        // The registry kept another writer's casing for a name the marker
        // claims: still ours, so an update and its removal, not a conflict.
        let observed = || {
            RegistryValues::from([
                ("InferenceProvider".to_owned(), Some("direct".to_owned())),
                (
                    ".agentdesktop.owner".to_owned(),
                    Some(r#"["inferenceProvider"]"#.to_owned()),
                ),
            ])
        };
        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, observed(), &desired, &plan);
        assert!(!plan.has_conflicts());
        assert_eq!(
            plan.registry_key_plans()[0].changes,
            vec![("inferenceProvider".to_owned(), Some("gateway".to_owned()))]
        );

        let plan = ReconcilePlan::default();
        super::plan_registry_remove(&key, observed(), &plan);
        assert_eq!(
            plan.registry_key_plans()[0].changes,
            vec![
                ("inferenceProvider".to_owned(), None),
                (".agentdesktop.owner".to_owned(), None),
            ]
        );
    }

    #[test]
    fn registry_plan_with_nothing_to_manage_leaves_no_key() {
        // `claudeDesktop: {}` without a gateway: even a marker-only key
        // would count as machine policy to Claude Desktop and switch it
        // away from `HKCU` policy, so nothing is planned on a clean system.
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, RegistryValues::new(), &BTreeMap::new(), &plan);
        assert!(plan.registry_key_plans().is_empty());
        assert!(plan.render().contains("Summary: 0 changes, 1 unchanged"));

        // A previously managed key loses the owned values, the marker and —
        // once empty — itself, and the dry run reports the removal.
        let observed = RegistryValues::from([
            ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
            (
                ".agentdesktop.owner".to_owned(),
                Some(r#"["inferenceProvider"]"#.to_owned()),
            ),
        ]);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key, observed, &BTreeMap::new(), &plan);
        let plans = plan.registry_key_plans();
        assert!(plans[0].delete_key_when_empty);
        assert_eq!(
            plans[0].changes,
            vec![
                ("inferenceProvider".to_owned(), None),
                (".agentdesktop.owner".to_owned(), None),
            ]
        );
        assert!(
            plan.render()
                .contains("REMOVE  Claude Desktop managed settings")
        );
    }

    #[test]
    fn registry_remove_deletes_only_what_the_marker_claims() {
        let observed = RegistryValues::from([
            ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
            ("foreign".to_owned(), Some("keep".to_owned())),
            (
                ".agentdesktop.owner".to_owned(),
                Some(r#"["inferenceProvider"]"#.to_owned()),
            ),
        ]);
        let key = RegistryKey::local_machine(super::POLICY_KEY);
        let plan = ReconcilePlan::default();
        super::plan_registry_remove(&key, observed, &plan);

        let plans = plan.registry_key_plans();
        assert!(plans[0].delete_key_when_empty);
        assert_eq!(
            plans[0].changes,
            vec![
                ("inferenceProvider".to_owned(), None),
                (".agentdesktop.owner".to_owned(), None),
            ]
        );

        // Without the marker nothing is owned and nothing is planned.
        let plan = ReconcilePlan::default();
        super::plan_registry_remove(
            &key,
            RegistryValues::from([("foreign".to_owned(), Some("keep".to_owned()))]),
            &plan,
        );
        assert!(plan.registry_key_plans().is_empty());
        assert!(plan.render().contains("Summary: 0 changes, 1 unchanged"));
    }

    #[cfg(windows)]
    use crate::reconcile::registry::TestPolicyKey;

    #[test]
    #[cfg(windows)]
    fn registry_round_trip_applies_creates_updates_and_removal() {
        use crate::reconcile::registry::{key_exists, read_values};
        let key = TestPolicyKey::new();
        let desired = BTreeMap::from([
            ("inferenceProvider".to_owned(), "gateway".to_owned()),
            ("isLocalDevMcpEnabled".to_owned(), "true".to_owned()),
        ]);

        // Create: the values and the marker land under the key.
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key.0, read_values(&key.0).unwrap(), &desired, &plan);
        plan.apply().unwrap();
        let values = read_values(&key.0).unwrap();
        assert_eq!(values["inferenceProvider"].as_deref(), Some("gateway"));
        assert_eq!(values["isLocalDevMcpEnabled"].as_deref(), Some("true"));
        assert_eq!(
            values[".agentdesktop.owner"].as_deref(),
            Some(r#"["inferenceProvider","isLocalDevMcpEnabled"]"#)
        );

        // In place: the next plan has nothing to apply.
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key.0, read_values(&key.0).unwrap(), &desired, &plan);
        assert!(plan.registry_key_plans().is_empty());

        // Update: one value changes its data, the other is dropped.
        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "direct".to_owned())]);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key.0, read_values(&key.0).unwrap(), &desired, &plan);
        plan.apply().unwrap();
        let values = read_values(&key.0).unwrap();
        assert_eq!(values["inferenceProvider"].as_deref(), Some("direct"));
        assert!(!values.contains_key("isLocalDevMcpEnabled"));

        // Remove: the owned values go, and with them the empty key.
        let plan = ReconcilePlan::default();
        super::plan_registry_remove(&key.0, read_values(&key.0).unwrap(), &plan);
        plan.apply().unwrap();
        assert!(!key_exists(&key.0).unwrap());
    }

    #[test]
    #[cfg(windows)]
    fn registry_removal_keeps_foreign_values_and_the_key() {
        use crate::reconcile::registry::{RegistryKeyPlan, key_exists, read_values};
        let key = TestPolicyKey::new();
        crate::reconcile::registry::apply(&RegistryKeyPlan {
            key: key.0.clone(),
            observed: RegistryValues::new(),
            changes: vec![("foreign".to_owned(), Some("keep".to_owned()))],
            delete_key_when_empty: false,
        })
        .unwrap();

        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key.0, read_values(&key.0).unwrap(), &desired, &plan);
        plan.apply().unwrap();

        let plan = ReconcilePlan::default();
        super::plan_registry_remove(&key.0, read_values(&key.0).unwrap(), &plan);
        plan.apply().unwrap();

        let values = read_values(&key.0).unwrap();
        assert_eq!(values["foreign"].as_deref(), Some("keep"));
        assert!(!values.contains_key("inferenceProvider"));
        assert!(key_exists(&key.0).unwrap());
    }

    #[test]
    #[cfg(windows)]
    fn registry_apply_refuses_when_the_key_changed_after_planning() {
        use crate::reconcile::registry::{RegistryKeyPlan, read_values};
        let key = TestPolicyKey::new();
        let desired = BTreeMap::from([("inferenceProvider".to_owned(), "gateway".to_owned())]);
        let plan = ReconcilePlan::default();
        super::plan_registry_settings(&key.0, read_values(&key.0).unwrap(), &desired, &plan);

        // Another writer touches the key between planning and applying.
        crate::reconcile::registry::apply(&RegistryKeyPlan {
            key: key.0.clone(),
            observed: RegistryValues::new(),
            changes: vec![("foreign".to_owned(), Some("raced".to_owned()))],
            delete_key_when_empty: false,
        })
        .unwrap();

        let error = plan.apply().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed since reconciliation was planned")
        );
    }

    /// A registry apply that fails after some writes landed: the marker was
    /// written first and claims them, so removal cleans the key back up.
    /// Constructed directly — `registry_values` refuses such a name at
    /// planning, and the registry itself refuses it at write time
    /// (measured on Windows: 16,383-character names ok, 16,384+ returns
    /// rc 87 ERROR_INVALID_PARAMETER).
    #[test]
    #[cfg(windows)]
    fn registry_partial_apply_failure_stays_recoverable() {
        use crate::reconcile::registry::{RegistryKeyPlan, key_exists, read_values};
        let key = TestPolicyKey::new();
        let error = crate::reconcile::registry::apply(&RegistryKeyPlan {
            key: key.0.clone(),
            observed: RegistryValues::new(),
            changes: vec![
                (
                    ".agentdesktop.owner".to_owned(),
                    Some(r#"["inferenceProvider"]"#.to_owned()),
                ),
                ("inferenceProvider".to_owned(), Some("gateway".to_owned())),
                ("z".repeat(16_384), Some("boom".to_owned())),
            ],
            delete_key_when_empty: false,
        })
        .expect_err("the overlong name must fail the apply");
        assert!(error.to_string().contains("write"), "{error}");

        // The write before the failure landed, claimed by the marker.
        let values = read_values(&key.0).unwrap();
        assert_eq!(values["inferenceProvider"].as_deref(), Some("gateway"));
        assert!(values.contains_key(".agentdesktop.owner"));

        // Removal finds everything through the marker and leaves no key.
        let plan = ReconcilePlan::default();
        super::plan_registry_remove(&key.0, read_values(&key.0).unwrap(), &plan);
        plan.apply().unwrap();
        assert!(!key_exists(&key.0).unwrap());
    }
}
