//! Tests for the GitHub Copilot CLI provider.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs,
    net::SocketAddr,
};

use agentdesktop_core::config::{
    CopilotConfig, CopilotModel, CopilotProvider, LlmGatewayConfig, ProxyUnavailable,
};
use serde_json::{Value, json};

use super::discovery;
use super::reconcile::{managed_document, plan, providers_path};
use crate::reconcile::ReconcilePlan;

fn fixture() -> Value {
    serde_json::from_str(include_str!("testdata/providers.lab.json"))
        .expect("fixture is valid JSON")
}

fn sample_config() -> CopilotConfig {
    let mut models = BTreeMap::new();
    models.insert(
        "gpt-4.1".to_owned(),
        CopilotModel {
            provider: CopilotProvider::Agentdesktop,
            model_id: None,
            extra: BTreeMap::from([("wireModel".to_owned(), json!("gpt-4.1-mini"))]),
        },
    );
    models.insert(
        "claude-haiku-4.5".to_owned(),
        CopilotModel {
            provider: CopilotProvider::AgentdesktopAnthropic,
            model_id: None,
            extra: BTreeMap::from([("wireModel".to_owned(), json!("claude-haiku-4-5"))]),
        },
    );
    CopilotConfig {
        use_llm_gateway: true,
        models,
    }
}

fn sample_gateway() -> LlmGatewayConfig {
    LlmGatewayConfig {
        url: "https://gateway.example.com".parse().unwrap(),
        authentication: None,
        proxy_url: None,
        github_oauth: None,
        when_proxy_unavailable: Default::default(),
    }
}

const LISTEN: &str = "127.0.0.1:18095";
const PAIRING: &str = "PAIRING-FIXTURE";

fn listen_addr() -> SocketAddr {
    LISTEN.parse().unwrap()
}

fn find_provider<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["providers"]
        .as_array()
        .unwrap_or_else(|| panic!("providers must be an array in {document}"))
        .iter()
        .find(|provider| provider["name"] == name)
        .unwrap_or_else(|| panic!("provider {name} not found in {document}"))
}

fn assert_user_entries_present(document: &Value) {
    assert!(
        document["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["name"] == "my-own-openai"),
        "user provider missing from {document}"
    );
    assert!(
        document["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "gpt-mine"),
        "user model missing from {document}"
    );
}

fn our_provider_count(document: &Value) -> usize {
    document["providers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|provider| {
            matches!(
                provider["name"].as_str(),
                Some("agentdesktop") | Some("agentdesktop-anthropic")
            )
        })
        .count()
}

fn write_user_owned_document(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "name": "my-own-openai",
                "type": "openai",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "sk-user",
            }],
            "models": [{"provider": "my-own-openai", "id": "gpt-mine"}],
        }))
        .unwrap(),
    )
    .unwrap();
}

// --- Config-level model validation, at parse time ---------------------------

fn parse_programs(yaml: &str) -> anyhow::Result<agentdesktop_core::config::DaemonConfig> {
    agentdesktop_core::config::parse_daemon(yaml)
}

#[test]
fn model_documents_default_provider_and_model_id() {
    let config = parse_programs(
        "llmGateway:\n  url: https://gateway.example\nprograms:\n  copilot:\n    models:\n      gpt-4.1:\n        wireModel: gpt-4.1-mini\n",
    )
    .unwrap();
    let models = config.programs.copilot.unwrap().model_documents();
    assert_eq!(models[0]["id"], "gpt-4.1");
    assert_eq!(models[0]["provider"], "agentdesktop");
    assert_eq!(models[0]["modelId"], "gpt-4.1");
    assert_eq!(models[0]["wireModel"], "gpt-4.1-mini");
}

#[test]
fn config_rejects_an_unknown_provider() {
    let error = parse_programs(
        "programs:\n  copilot:\n    models:\n      x:\n        provider: not-ours\n",
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("provider"), "{error:#}");
}

#[test]
fn config_rejects_a_reserved_or_secret_pass_through_key() {
    for (key, needle) in [("id", "set by agentdesktop"), ("apiKey", "not allowed")] {
        let error = parse_programs(&format!(
            "programs:\n  copilot:\n    models:\n      x:\n        {key}: y\n"
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains(needle), "{key}: {error:#}");
    }
}

#[test]
fn config_requires_a_model_with_a_gateway() {
    let error =
        parse_programs("llmGateway:\n  url: https://gateway.example\nprograms:\n  copilot: {}\n")
            .unwrap_err();
    assert!(format!("{error:#}").contains("at least one"), "{error:#}");
    parse_programs("programs:\n  copilot: {}\n").expect("no gateway, no model needed");
}

#[test]
fn config_rejects_sandbox_with_copilot() {
    let error =
        parse_programs("sandbox:\n  filesystem: {}\nprograms:\n  copilot: {}\n").unwrap_err();
    assert!(format!("{error:#}").contains("sandbox"), "{error:#}");
}

// --- Version discovery ------------------------------------------------------

#[test]
fn npm_version_reads_the_package_manifest_next_to_the_launcher() {
    let prefix = tempfile::tempdir().unwrap();
    let package = prefix.path().join("lib/node_modules/@github/copilot");
    fs::create_dir_all(package.join("bin")).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"@github/copilot","version":"1.0.88"}"#,
    )
    .unwrap();
    fs::write(package.join("bin/copilot"), "#!/bin/sh\n").unwrap();
    fs::create_dir_all(prefix.path().join("bin")).unwrap();
    let launcher = prefix.path().join("bin/copilot");
    #[cfg(unix)]
    std::os::unix::fs::symlink(package.join("bin/copilot"), &launcher).unwrap();
    #[cfg(not(unix))]
    fs::copy(package.join("bin/copilot"), &launcher).unwrap();
    assert_eq!(discovery::npm_version(&launcher).as_deref(), Some("1.0.88"));
}

#[test]
fn npm_version_ignores_a_manifest_of_another_package() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("package.json"),
        r#"{"name":"@openai/codex","version":"9.9.9"}"#,
    )
    .unwrap();
    fs::write(dir.path().join("copilot"), "#!/bin/sh\n").unwrap();
    assert_eq!(discovery::npm_version(&dir.path().join("copilot")), None);
}

#[test]
fn npm_version_is_none_for_a_standalone_binary() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("copilot"), "#!/bin/sh\n").unwrap();
    assert_eq!(discovery::npm_version(&dir.path().join("copilot")), None);
}

// --- File location precedence -----------------------------------------------

#[test]
fn providers_path_prefers_explicit_config_env_var() {
    let mut vars = HashMap::new();
    vars.insert("COPILOT_PROVIDERS_CONFIG", "/explicit/providers.json");
    vars.insert("COPILOT_HOME", "/copilot-home");
    vars.insert("HOME", "/home/user");
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert_eq!(
        providers_path(&lookup).unwrap(),
        std::path::PathBuf::from("/explicit/providers.json")
    );
}

#[test]
fn providers_path_falls_back_to_copilot_home() {
    let mut vars = HashMap::new();
    vars.insert("COPILOT_HOME", "/copilot-home");
    vars.insert("HOME", "/home/user");
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert_eq!(
        providers_path(&lookup).unwrap(),
        std::path::PathBuf::from("/copilot-home/providers.json")
    );
}

#[test]
fn providers_path_falls_back_to_home_dot_copilot() {
    let mut vars = HashMap::new();
    vars.insert("HOME", "/home/user");
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert_eq!(
        providers_path(&lookup).unwrap(),
        std::path::PathBuf::from("/home/user/.copilot/providers.json")
    );
}

#[test]
fn providers_path_falls_back_to_userprofile_when_home_is_unset() {
    let mut vars = HashMap::new();
    vars.insert("USERPROFILE", r"C:\Users\user");
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert_eq!(
        providers_path(&lookup).unwrap(),
        std::path::PathBuf::from(r"C:\Users\user").join(".copilot/providers.json")
    );
}

#[test]
fn providers_path_treats_empty_values_as_unset() {
    let mut vars = HashMap::new();
    vars.insert("COPILOT_PROVIDERS_CONFIG", "");
    vars.insert("COPILOT_HOME", "");
    vars.insert("HOME", "");
    vars.insert("USERPROFILE", "/profile");
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert_eq!(
        providers_path(&lookup).unwrap(),
        std::path::PathBuf::from("/profile/.copilot/providers.json")
    );
}

#[test]
fn providers_path_errors_without_any_home() {
    let vars: HashMap<&str, &str> = HashMap::new();
    let lookup = |key: &str| vars.get(key).map(OsString::from);
    assert!(providers_path(&lookup).is_err());
}

// --- managed_document shape -------------------------------------------------

#[test]
fn managed_document_matches_the_lab_fixture_shape() {
    let config = sample_config();
    let document = managed_document(&config, listen_addr(), PAIRING).expect("managed document");
    let fixture = fixture();
    assert_eq!(document["providers"], fixture["providers"]);
    assert_eq!(document["models"], fixture["models"]);
}

// --- plan(): fresh create ---------------------------------------------------

#[test]
fn fresh_file_is_created_matching_the_fixture_and_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .expect("plan succeeds");
    assert!(!changes.has_conflicts());
    changes.apply().expect("apply succeeds");

    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let fixture = fixture();
    assert_eq!(written["providers"], fixture["providers"]);
    assert_eq!(written["models"], fixture["models"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a file we create must be owner-only"
        );
    }
    assert!(
        super::super::json_merge::state_path(&path).exists(),
        "a merge sidecar must be written alongside the created file"
    );
}

// A missing Copilot directory is created owner-only along with the file; an
// existing directory keeps its mode.
#[test]
fn fresh_directory_is_created_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".copilot").join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert!(path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let existing = tempfile::tempdir().unwrap();
        let open_dir = existing.path().join("open");
        fs::create_dir(&open_dir).unwrap();
        fs::set_permissions(&open_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = open_dir.join("providers.json");
        let changes = ReconcilePlan::default();
        plan(
            &path,
            Some((listen_addr(), PAIRING)),
            Some((&config, Some(&gateway))),
            &changes,
        )
        .unwrap();
        changes.apply().unwrap();
        assert_eq!(
            fs::metadata(&open_dir).unwrap().permissions().mode() & 0o777,
            0o755,
            "an existing directory keeps its mode"
        );
    }
}

// --- plan(): user entries survive, re-pairing replaces without duplicating ---

#[test]
fn user_provider_and_model_survive_reapply_and_repairing_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    // A user hand-adds their own BYOK provider and model.
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["providers"].as_array_mut().unwrap().push(json!({
        "name": "my-own-openai",
        "type": "openai",
        "baseUrl": "https://api.example.com/v1",
        "apiKey": "sk-user",
    }));
    document["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"provider": "my-own-openai", "id": "gpt-mine"}));
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    // Re-apply unchanged: both the user's and our entries stay, none duplicated.
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    let after_reapply: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_user_entries_present(&after_reapply);
    assert_eq!(our_provider_count(&after_reapply), 2);

    // Re-pairing (new listen address and pairing value, e.g. FRESH=1
    // re-enrollment): our entries are replaced in place, not duplicated; the
    // user's entries are untouched.
    let new_listen: SocketAddr = "127.0.0.1:18099".parse().unwrap();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((new_listen, "PAIRING-NEW")),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    let after_repair: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_user_entries_present(&after_repair);
    assert_eq!(
        our_provider_count(&after_repair),
        2,
        "re-pairing must replace our providers, not duplicate them"
    );
    let openai = find_provider(&after_repair, "agentdesktop");
    assert_eq!(openai["baseUrl"], "http://127.0.0.1:18099/copilot-cli/v1");
    assert_eq!(openai["headers"]["x-agentdesktop-pairing"], "PAIRING-NEW");
}

// --- plan(): removal --------------------------------------------------------

#[test]
fn removal_deletes_the_file_only_when_we_created_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert!(path.exists());

    // programs.copilot absent from the pushed config.
    let changes = ReconcilePlan::default();
    plan(&path, Some((listen_addr(), PAIRING)), None, &changes).unwrap();
    changes.apply().unwrap();

    assert!(
        !path.exists(),
        "a file we created must be removed once nothing else remains"
    );
    assert!(!super::super::json_merge::state_path(&path).exists());
}

#[test]
fn removal_keeps_a_preexisting_user_owned_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    write_user_owned_document(&path);

    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    let changes = ReconcilePlan::default();
    plan(&path, Some((listen_addr(), PAIRING)), None, &changes).unwrap();
    changes.apply().unwrap();

    assert!(
        path.exists(),
        "a file we did not create must not be deleted"
    );
    let remaining: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(remaining["providers"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["providers"][0]["name"], "my-own-openai");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "removal must not loosen the mode of a file that holds the user's keys"
        );
    }
}

#[test]
fn removal_when_use_llm_gateway_is_effectively_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    // `useLlmGateway: false`, or no top-level `llmGateway`: the caller passes
    // the program's config with the gateway filtered to `None`.
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, None)),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    assert!(!path.exists());
}

#[test]
fn fail_open_removes_the_entries_when_the_loopback_proxy_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let mut gateway = sample_gateway();
    gateway.when_proxy_unavailable = ProxyUnavailable::FailOpen;

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();

    // `ctx.llm_proxy` is `None` (bind failed, proxy off, no pairing) even
    // though the program is still configured with `useLlmGateway: true`.
    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    changes.apply().unwrap();

    assert!(
        !path.exists(),
        "our entries must be removed when the proxy is unavailable, even if programs.copilot is still configured"
    );
}

#[test]
fn fail_closed_keeps_the_entries_when_the_loopback_proxy_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();
    assert_eq!(
        gateway.when_proxy_unavailable,
        ProxyUnavailable::FailClosed,
        "the default"
    );
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    let state = super::super::json_merge::state_path(&path);
    let (before, state_before) = (fs::read(&path).unwrap(), fs::read(&state).unwrap());

    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_REASON)
    );
    changes.apply().unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "the providers stay, pointed at the loopback port"
    );
    assert_eq!(fs::read(&state).unwrap(), state_before);

    // Removing the program still removes, whatever the policy.
    let changes = ReconcilePlan::default();
    plan(&path, None, None, &changes).unwrap();
    changes.apply().unwrap();
    assert!(!path.exists());
}

// --- plan(): conflicts ------------------------------------------------------

#[test]
fn conflict_on_a_foreign_provider_named_agentdesktop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "name": "agentdesktop",
                "type": "openai",
                "baseUrl": "https://not-ours.example.com/v1",
                "apiKey": "sk-user",
            }],
            "models": [],
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&path).unwrap();

    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();

    assert!(changes.has_conflicts());
    assert!(changes.render().contains("CONFLICT"));
    assert!(changes.apply().is_err());
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "a conflict must write nothing"
    );
}

#[test]
fn conflict_on_a_foreign_provider_named_agentdesktop_that_carries_a_model() {
    // The user's own provider happens to be called "agentdesktop" (no pairing
    // header) and carries a model: both are the user's, so the merge is refused.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "providers": [{"name": "agentdesktop", "type": "openai", "baseUrl": "https://example.invalid/v1"}],
            "models": [{"provider": "agentdesktop", "id": "gpt-4.1"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    assert!(changes.has_conflicts());
    assert!(
        changes.render().contains("provider agentdesktop exists"),
        "{}",
        changes.render()
    );
    assert!(changes.apply().is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn conflict_on_a_non_object_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    fs::write(&path, b"[]\n").unwrap();

    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();

    assert!(changes.has_conflicts());
    assert!(changes.apply().is_err());
    assert_eq!(fs::read(&path).unwrap(), b"[]\n");
}

// --- Provider::plan: system-mode rejection ----------------------------------

#[test]
fn system_mode_rejects_a_configured_program_before_writing_anything() {
    use super::Copilot;
    use crate::provider::{Provider, ReconcileContext};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = parse_programs(
        "llmGateway:\n  url: https://gateway.example\nprograms:\n  copilot:\n    models:\n      gpt-4.1: {}\n",
    )
    .unwrap();
    let context = |merge_user_settings: bool| ReconcileContext {
        merge_user_settings,
        credential_helper: dir.path().join("agentdesktop"),
        socket: dir.path().join("agentdesktop.sock"),
        llm_proxy: None,
    };
    // System mode: no providers path, or a path but no user settings.
    for provider in [
        Copilot {
            providers_path: None,
        },
        Copilot {
            providers_path: Some(path.clone()),
        },
    ] {
        let error = match provider.plan(&context(false), &config) {
            Ok(_) => panic!("system mode with programs.copilot configured must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("--user"), "{error}");
        assert!(error.to_string().contains("GitHub Copilot CLI"), "{error}");
    }
    assert!(!path.exists(), "preflight failure must not write any files");
    // System mode with the program absent does nothing.
    let absent = parse_programs("llmGateway:\n  url: https://gateway.example\n").unwrap();
    let plan = Copilot {
        providers_path: None,
    }
    .plan(&context(false), &absent)
    .unwrap();
    assert!(
        plan.render().trim().ends_with("changed"),
        "{}",
        plan.render()
    );
    // User mode without the proxy: a removal is planned, nothing to remove here.
    Copilot {
        providers_path: Some(path.clone()),
    }
    .plan(&context(true), &config)
    .unwrap()
    .apply()
    .unwrap();
    assert!(!path.exists());
}

// --- keyed ownership, created flag, redaction ------------------------------

fn apply_managed(path: &std::path::Path, listen: SocketAddr, pairing: &str) {
    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        path,
        Some((listen, pairing)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    assert!(!changes.has_conflicts(), "{}", changes.render());
    changes.apply().unwrap();
}

fn apply_removal(path: &std::path::Path) -> ReconcilePlan {
    let changes = ReconcilePlan::default();
    plan(path, Some((listen_addr(), PAIRING)), None, &changes).unwrap();
    changes
}

fn read(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn an_edited_managed_entry_is_replaced_and_removed_by_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    apply_managed(&path, listen_addr(), PAIRING);
    // The user edits our model and our provider in place.
    let mut document = read(&path);
    for model in document["models"].as_array_mut().unwrap() {
        if model["id"] == "gpt-4.1" {
            model["wireModel"] = json!("gpt-4.1-edited");
        }
    }
    for provider in document["providers"].as_array_mut().unwrap() {
        if provider["name"] == "agentdesktop" {
            provider["baseUrl"] = json!("http://127.0.0.1:1/edited");
        }
    }
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    // Re-apply with a new pairing: ours are replaced, not duplicated, edits gone.
    apply_managed(&path, listen_addr(), "PAIRING-NEW");
    let after = read(&path);
    assert_eq!(our_provider_count(&after), 2);
    let openai = find_provider(&after, "agentdesktop");
    assert_eq!(openai["baseUrl"], "http://127.0.0.1:18095/copilot-cli/v1");
    assert_eq!(openai["headers"]["x-agentdesktop-pairing"], "PAIRING-NEW");
    let models = after["models"].as_array().unwrap();
    assert_eq!(models.iter().filter(|m| m["id"] == "gpt-4.1").count(), 1);
    assert_eq!(
        models.iter().find(|m| m["id"] == "gpt-4.1").unwrap()["wireModel"],
        "gpt-4.1-mini"
    );
    // Edit again, then remove the program: the edited entries go too.
    let mut document = read(&path);
    document["models"][0]["wireModel"] = json!("edited-again");
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    apply_removal(&path).apply().unwrap();
    assert!(!path.exists(), "nothing of ours may remain");
}

#[test]
fn a_user_model_with_a_managed_id_under_another_provider_is_not_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "providers": [{"name": "my-own", "type": "openai", "baseUrl": "https://example.invalid/v1"}],
            "models": [{"provider": "my-own", "id": "gpt-4.1"}],
        }))
        .unwrap(),
    )
    .unwrap();
    apply_managed(&path, listen_addr(), PAIRING);
    let after = read(&path);
    let gpt = after["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"] == "gpt-4.1")
        .count();
    assert_eq!(
        gpt, 2,
        "the user's my-own/gpt-4.1 and our agentdesktop/gpt-4.1 coexist"
    );
    // A user model under OUR provider with another id is kept across re-apply.
    let mut document = read(&path);
    document["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"provider": "agentdesktop", "id": "gpt-user"}));
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    apply_managed(&path, listen_addr(), PAIRING);
    assert!(
        read(&path)["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "gpt-user")
    );
}

#[test]
fn an_orphaned_managed_provider_with_a_pairing_header_is_replaced_not_a_conflict() {
    // The sidecar is gone (deleted, or the file came from another device), but
    // the entry carries a pairing header: an agentdesktop wrote it, so it is
    // replaced and owned again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    apply_managed(&path, listen_addr(), "PAIRING-OLD");
    fs::remove_file(super::super::json_merge::state_path(&path)).unwrap();
    apply_managed(&path, listen_addr(), PAIRING);
    let after = read(&path);
    assert_eq!(our_provider_count(&after), 2);
    assert_eq!(
        find_provider(&after, "agentdesktop")["headers"]["x-agentdesktop-pairing"],
        PAIRING
    );
    assert!(super::super::json_merge::state_path(&path).exists());
}

#[test]
fn orphan_removal_does_not_add_a_missing_models_key() {
    // No sidecar and no `models` key: removing our providers by their pairing
    // header must not invent `"models": null` in the user's file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let document = json!({
        "providers": [
            {"name": super::PROVIDER_OPENAI, "headers": {"x-agentdesktop-pairing": PAIRING}},
            {"name": "mine", "baseUrl": "https://example.com"}
        ]
    });
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    apply_removal(&path).apply().unwrap();
    assert_eq!(
        read(&path),
        json!({"providers": [{"name": "mine", "baseUrl": "https://example.com"}]})
    );
}

#[test]
fn a_file_recreated_by_the_daemon_after_the_user_deleted_theirs_is_removed_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    write_user_owned_document(&path);
    apply_managed(&path, listen_addr(), PAIRING);
    fs::remove_file(&path).unwrap();
    apply_managed(&path, listen_addr(), PAIRING);
    apply_removal(&path).apply().unwrap();
    assert!(
        !path.exists(),
        "the daemon created this file, so removal deletes it"
    );
}

#[test]
fn the_plan_report_never_shows_the_pairing_or_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    write_user_owned_document(&path);
    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    let report = changes.render();
    assert!(report.contains("UPDATE"), "{report}");
    assert!(
        !report.contains(PAIRING) && !report.contains("sk-user"),
        "{report}"
    );
    changes.apply().unwrap();
    let removal = apply_removal(&path);
    let report = removal.render();
    assert!(
        !report.contains(PAIRING) && !report.contains("sk-user"),
        "{report}"
    );
}

#[test]
fn a_managed_model_dropped_from_the_config_is_removed_even_if_edited() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    apply_managed(&path, listen_addr(), PAIRING);
    let mut document = read(&path);
    for model in document["models"].as_array_mut().unwrap() {
        if model["id"] == "claude-haiku-4.5" {
            model["wireModel"] = json!("edited");
        }
    }
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    // The admin drops the anthropic model from the config.
    let mut config = sample_config();
    config.models.remove("claude-haiku-4.5");
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    let after = read(&path);
    assert!(
        !after["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "claude-haiku-4.5"),
        "a previously managed model must not linger: {after}"
    );
    apply_removal(&path).apply().unwrap();
    assert!(!path.exists());
}

#[test]
fn a_user_model_under_a_managed_provider_name_without_that_provider_is_a_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "providers": [],
            "models": [{"provider": "agentdesktop", "id": "gpt-4.1"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    let config = sample_config();
    let gateway = sample_gateway();
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    assert!(changes.has_conflicts());
    assert!(
        changes.render().contains("model agentdesktop/gpt-4.1"),
        "{}",
        changes.render()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn removal_without_a_sidecar_strips_entries_that_carry_a_pairing_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    write_user_owned_document(&path);
    apply_managed(&path, listen_addr(), PAIRING);
    fs::remove_file(super::super::json_merge::state_path(&path)).unwrap();
    // Another device's (or a hand-written) entry with a different pairing is
    // not this daemon's and stays.
    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), "PAIRING-OTHER")),
        None,
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert_eq!(our_provider_count(&read(&path)), 2);
    let removal = apply_removal(&path);
    assert!(!removal.render().contains(PAIRING));
    removal.apply().unwrap();
    let after = read(&path);
    assert_eq!(our_provider_count(&after), 0, "{after}");
    assert!(
        after["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["provider"] == "my-own-openai")
    );
    assert_user_entries_present(&after);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn config_rejects_a_non_object_entry_and_names_the_entry() {
    let error =
        parse_programs("programs:\n  copilot:\n    models:\n      gpt-4.1: not-an-object\n")
            .unwrap_err();
    assert!(format!("{error:#}").contains("gpt-4.1"), "{error:#}");
    let error =
        parse_programs("programs:\n  copilot:\n    models:\n      gpt-4.1:\n        apiKey: x\n")
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("models.gpt-4.1.apiKey"),
        "{error:#}"
    );
}

// --- failClosed in place, failOpen reason, cleanup without a sidecar --------

fn fail_open_gateway() -> LlmGatewayConfig {
    let mut gateway = sample_gateway();
    gateway.when_proxy_unavailable = ProxyUnavailable::FailOpen;
    gateway
}

#[test]
fn fail_closed_first_apply_reports_not_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = sample_gateway();

    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_NOT_IN_PLACE_REASON)
    );
    changes.apply().unwrap();
    assert!(
        !path.exists(),
        "nothing is written when nothing is in place"
    );
    assert!(!super::super::json_merge::state_path(&path).exists());
}

#[test]
fn fail_open_reports_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = fail_open_gateway();
    apply_managed_with(&path, &config, &gateway);

    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_OPEN_REASON)
    );
}

fn apply_managed_with(path: &std::path::Path, config: &CopilotConfig, gateway: &LlmGatewayConfig) {
    let changes = ReconcilePlan::default();
    plan(
        path,
        Some((listen_addr(), PAIRING)),
        Some((config, Some(gateway))),
        &changes,
    )
    .unwrap();
    assert!(!changes.has_conflicts(), "{}", changes.render());
    changes.apply().unwrap();
}

/// A user-owned file with our entries applied, the sidecar gone and the mode
/// loosened to 0640.
fn paired_entries_without_a_sidecar(path: &std::path::Path, gateway: &LlmGatewayConfig) {
    write_user_owned_document(path);
    apply_managed_with(path, &sample_config(), gateway);
    fs::remove_file(super::super::json_merge::state_path(path)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    assert_eq!(our_provider_count(&read(path)), 2);
}

fn assert_paired_entries_gone_user_entries_kept(path: &std::path::Path) {
    let after = read(path);
    assert_eq!(our_provider_count(&after), 0, "{after}");
    assert!(
        after["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["provider"] == "my-own-openai"),
        "{after}"
    );
    assert_user_entries_present(&after);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640,
            "the file keeps its mode"
        );
    }
}

#[test]
fn fail_open_without_sidecar_removes_paired_entries_and_keeps_user_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    let config = sample_config();
    let gateway = fail_open_gateway();
    paired_entries_without_a_sidecar(&path, &gateway);

    // No proxy, so no pairing is known.
    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    changes.apply().unwrap();
    assert_paired_entries_gone_user_entries_kept(&path);
}

#[test]
fn removal_without_proxy_and_sidecar_removes_paired_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    paired_entries_without_a_sidecar(&path, &sample_gateway());

    let changes = ReconcilePlan::default();
    plan(&path, None, None, &changes).unwrap();
    changes.apply().unwrap();
    assert_paired_entries_gone_user_entries_kept(&path);
}

#[test]
fn known_pairing_keeps_entries_with_another_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("providers.json");
    paired_entries_without_a_sidecar(&path, &sample_gateway());

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), "PAIRING-OTHER")),
        None,
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert_eq!(our_provider_count(&read(&path)), 2);
}
