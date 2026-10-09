//! Tests for the VS Code Copilot Chat (own models) provider. Closely mirrors
//! `provider::copilot::tests`, which this feature builds on.

use std::{collections::BTreeMap, fs, net::SocketAddr, path::PathBuf};

use agentdesktop_core::config::{LlmGatewayConfig, ProxyUnavailable, VsCodeConfig, VsCodeModel};
use serde_json::{Value, json};

use super::reconcile::{chat_models_path, managed_document, plan};
use crate::reconcile::ReconcilePlan;

fn fixture() -> Value {
    serde_json::from_str(include_str!("testdata/chatLanguageModels.lab.json"))
        .expect("fixture is valid JSON")
}

fn sample_config() -> VsCodeConfig {
    let mut models = BTreeMap::new();
    models.insert(
        "gpt-4.1-mini".to_owned(),
        VsCodeModel {
            name: None,
            tool_calling: true,
            vision: false,
            max_input_tokens: Some(128_000),
            max_output_tokens: Some(16_000),
            extra: BTreeMap::new(),
        },
    );
    VsCodeConfig {
        use_llm_gateway: true,
        copilot_chat: Default::default(),
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

fn find_vendor<'a>(document: &'a Value, name: &str) -> &'a Value {
    document
        .as_array()
        .unwrap_or_else(|| panic!("chatLanguageModels.json root must be an array in {document}"))
        .iter()
        .find(|vendor| vendor["name"] == name)
        .unwrap_or_else(|| panic!("vendor {name} not found in {document}"))
}

fn our_vendor_count(document: &Value) -> usize {
    document
        .as_array()
        .unwrap()
        .iter()
        .filter(|vendor| vendor["name"] == VsCodeConfig::VENDOR_NAME)
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
        serde_json::to_vec_pretty(&json!([{
            "name": "my-own-vendor",
            "vendor": "customendpoint",
            "apiKey": "sk-user",
            "apiType": "chat-completions",
            "models": [{
                "id": "gpt-mine",
                "url": "https://example.com/v1/chat/completions",
            }],
        }]))
        .unwrap(),
    )
    .unwrap();
}

fn apply_managed(
    path: &std::path::Path,
    listen: SocketAddr,
    pairing: &str,
    config: &VsCodeConfig,
    gateway: &LlmGatewayConfig,
) {
    let changes = ReconcilePlan::default();
    plan(
        path,
        Some((listen, pairing)),
        Some((config, Some(gateway))),
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

fn parse_programs(yaml: &str) -> anyhow::Result<agentdesktop_core::config::DaemonConfig> {
    agentdesktop_core::config::parse_daemon(yaml)
}

// --- Config-level validation, at parse time ---------------------------------

#[test]
fn config_rejects_a_reserved_pass_through_key() {
    for key in ["id", "url", "requestHeaders"] {
        let error = parse_programs(&format!(
            "programs:\n  vscode:\n    models:\n      x:\n        {key}: y\n"
        ))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("set by agentdesktop"),
            "{key}: {error:#}"
        );
    }
}

#[test]
fn config_requires_a_model_with_a_gateway() {
    let error =
        parse_programs("llmGateway:\n  url: https://gateway.example\nprograms:\n  vscode: {}\n")
            .unwrap_err();
    assert!(format!("{error:#}").contains("at least one"), "{error:#}");
    parse_programs("programs:\n  vscode: {}\n").expect("no gateway, no model needed");
}

#[test]
fn config_rejects_sandbox_with_vscode() {
    let error =
        parse_programs("sandbox:\n  filesystem: {}\nprograms:\n  vscode: {}\n").unwrap_err();
    assert!(format!("{error:#}").contains("sandbox"), "{error:#}");
}

#[test]
fn daemon_vscode_config_overrides_the_default_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    fs::write(
        &path,
        "daemon:\n  user: true\n  vscode:\n    config: /custom/chatLanguageModels.json\n",
    )
    .unwrap();
    let config = agentdesktop_core::config::load_daemon(&path).unwrap();
    assert_eq!(
        config.daemon.unwrap().vscode.config,
        Some(PathBuf::from("/custom/chatLanguageModels.json"))
    );
}

// --- githubModels config validation -----------------------------------------

#[test]
fn config_rejects_github_models_with_a_non_empty_models_map() {
    let error = parse_programs(
        "programs:\n  vscode:\n    copilotChat: githubModels\n    models:\n      x: {}\n",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("not allowed when copilotChat is githubModels"),
        "{error:#}"
    );
    parse_programs("programs:\n  vscode:\n    copilotChat: githubModels\n")
        .expect("githubModels with no models is valid");
}

#[test]
fn config_rejects_github_models_without_proxy_url_only_when_the_gateway_is_used() {
    // The gateway is used (useLlmGateway defaults to true) and configured,
    // but llmGateway.proxyUrl is unset: rejected.
    let error = parse_programs(
        "llmGateway:\n  url: https://gateway.example\nprograms:\n  vscode:\n    copilotChat: githubModels\n",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("llmGateway.proxyUrl"),
        "{error:#}"
    );

    // useLlmGateway: false: the gateway is configured but not used by this
    // program, so no proxyUrl is required.
    parse_programs(
        "llmGateway:\n  url: https://gateway.example\nprograms:\n  vscode:\n    copilotChat: githubModels\n    useLlmGateway: false\n",
    )
    .expect("githubModels with useLlmGateway: false needs no proxyUrl");

    // No top-level llmGateway at all: same, no proxyUrl required.
    parse_programs("programs:\n  vscode:\n    copilotChat: githubModels\n")
        .expect("githubModels with no llmGateway needs no proxyUrl");
}

#[test]
fn config_accepts_github_models_when_both_the_gateway_and_proxy_url_are_set() {
    // Both values matched in validate_daemon: use_llm_gateway true, gateway
    // configured, and llmGateway.proxyUrl set.
    parse_programs(
        "llmGateway:\n  url: https://gateway.example\n  proxyUrl: https://gateway.example/copilot-proxy\nprograms:\n  vscode:\n    copilotChat: githubModels\n",
    )
    .expect("githubModels with llmGateway.proxyUrl set is valid");
}

// --- chat_models_path per OS ------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn chat_models_path_on_linux() {
    assert_eq!(
        chat_models_path(std::path::Path::new("/home/user")),
        PathBuf::from("/home/user/.config/Code/User/chatLanguageModels.json")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn chat_models_path_on_macos() {
    assert_eq!(
        chat_models_path(std::path::Path::new("/Users/user")),
        PathBuf::from("/Users/user/Library/Application Support/Code/User/chatLanguageModels.json")
    );
}

#[cfg(windows)]
#[test]
fn chat_models_path_on_windows() {
    assert_eq!(
        chat_models_path(std::path::Path::new(r"C:\Users\user")),
        PathBuf::from(r"C:\Users\user").join("AppData/Roaming/Code/User/chatLanguageModels.json")
    );
}

// --- managed_document shape -------------------------------------------------

#[test]
fn managed_document_matches_the_lab_fixture_shape() {
    let config = sample_config();
    let document = managed_document(&config, listen_addr(), PAIRING).expect("managed document");
    assert_eq!(document, fixture());
}

// --- plan(): fresh create ---------------------------------------------------

#[test]
fn fresh_file_is_created_matching_the_fixture_and_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
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

    let written = read(&path);
    assert_eq!(written, fixture());

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

// --- plan(): user entries survive, re-pairing replaces without duplicating ---

#[test]
fn user_vendor_entry_survives_reapply_and_repairing_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = sample_gateway();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // A user hand-adds their own custom-endpoint vendor entry.
    let mut document = read(&path);
    document.as_array_mut().unwrap().push(json!({
        "name": "my-own-vendor",
        "vendor": "customendpoint",
        "apiKey": "sk-user",
        "apiType": "chat-completions",
        "models": [{"id": "gpt-mine", "url": "https://example.com/v1/chat/completions"}],
    }));
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    // Re-apply unchanged: both the user's and our entry stay, ours not duplicated.
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let after_reapply = read(&path);
    assert!(
        after_reapply
            .as_array()
            .unwrap()
            .iter()
            .any(|vendor| vendor["name"] == "my-own-vendor"),
        "user vendor missing from {after_reapply}"
    );
    assert_eq!(our_vendor_count(&after_reapply), 1);

    // Re-pairing (new listen address and pairing value, e.g. FRESH=1
    // re-enrollment): our entry is replaced in place, not duplicated.
    let new_listen: SocketAddr = "127.0.0.1:18099".parse().unwrap();
    apply_managed(&path, new_listen, "PAIRING-NEW", &config, &gateway);
    let after_repair = read(&path);
    assert!(
        after_repair
            .as_array()
            .unwrap()
            .iter()
            .any(|vendor| vendor["name"] == "my-own-vendor")
    );
    assert_eq!(
        our_vendor_count(&after_repair),
        1,
        "re-pairing must replace our vendor entry, not duplicate it"
    );
    let ours = find_vendor(&after_repair, VsCodeConfig::VENDOR_NAME);
    assert_eq!(
        ours["models"][0]["url"],
        "http://127.0.0.1:18099/vscode-copilot/v1/chat/completions"
    );
    assert_eq!(
        ours["models"][0]["requestHeaders"]["x-agentdesktop-pairing"],
        "PAIRING-NEW"
    );
}

// --- plan(): hand edits and dropped models are undone -----------------------

#[test]
fn a_hand_edit_to_the_managed_vendor_entry_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let mut document = read(&path);
    for vendor in document.as_array_mut().unwrap() {
        if vendor["name"] == VsCodeConfig::VENDOR_NAME {
            vendor["models"][0]["name"] = json!("edited-by-hand");
            vendor["apiType"] = json!("edited");
        }
    }
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    // Re-apply with a new pairing: the whole vendor entry is replaced, not
    // merged field by field; the edit is gone and no duplicate appears.
    apply_managed(&path, listen_addr(), "PAIRING-NEW", &config, &gateway);
    let after = read(&path);
    assert_eq!(our_vendor_count(&after), 1);
    let ours = find_vendor(&after, VsCodeConfig::VENDOR_NAME);
    assert_eq!(ours["apiType"], "chat-completions");
    assert_eq!(ours["models"][0]["name"], "gpt-4.1-mini (agentdesktop)");
}

#[test]
fn a_dropped_model_disappears() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let mut config = sample_config();
    config.models.insert(
        "gpt-4.1".to_owned(),
        VsCodeModel {
            name: None,
            tool_calling: true,
            vision: false,
            max_input_tokens: None,
            max_output_tokens: None,
            extra: BTreeMap::new(),
        },
    );
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let first = read(&path);
    let ours = find_vendor(&first, VsCodeConfig::VENDOR_NAME);
    assert_eq!(ours["models"].as_array().unwrap().len(), 2);

    // The admin drops the extra model from the config.
    config.models.remove("gpt-4.1");
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let after = read(&path);
    let ours = find_vendor(&after, VsCodeConfig::VENDOR_NAME);
    let models = ours["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "{after}");
    assert!(!models.iter().any(|model| model["id"] == "gpt-4.1"));
}

// --- plan(): removal --------------------------------------------------------

#[test]
fn removal_deletes_the_file_only_when_we_created_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    assert!(path.exists());

    apply_removal(&path).apply().unwrap();

    assert!(
        !path.exists(),
        "a file we created must be removed once nothing else remains"
    );
    assert!(!super::super::json_merge::state_path(&path).exists());
}

#[test]
fn removal_keeps_a_preexisting_user_owned_file_and_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    write_user_owned_document(&path);

    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    apply_removal(&path).apply().unwrap();

    assert!(
        path.exists(),
        "a file we did not create must not be deleted"
    );
    let remaining = read(&path);
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    assert_eq!(remaining[0]["name"], "my-own-vendor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "removal must not loosen the mode of a file that may hold the user's own keys"
        );
    }
}

#[test]
fn removal_when_use_llm_gateway_is_effectively_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

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
fn fail_open_removes_the_entry_when_the_loopback_proxy_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let mut gateway = sample_gateway();
    gateway.when_proxy_unavailable = ProxyUnavailable::FailOpen;
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // `ctx.llm_proxy` is `None` (bind failed, proxy off, no pairing) even
    // though the program is still configured with `useLlmGateway: true`.
    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    changes.apply().unwrap();

    assert!(
        !path.exists(),
        "our vendor entry must be removed when the proxy is unavailable, even if programs.vscode is still configured"
    );
}

#[test]
fn fail_closed_keeps_the_entry_when_the_loopback_proxy_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let state = super::super::json_merge::state_path(&path);
    let (before, state_before) = (fs::read(&path).unwrap(), fs::read(&state).unwrap());

    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_REASON)
    );
    changes.apply().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&state).unwrap(), state_before);
}

#[test]
fn removal_without_a_sidecar_strips_entries_that_carry_a_pairing_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    write_user_owned_document(&path);
    let config = sample_config();
    let gateway = sample_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    fs::remove_file(super::super::json_merge::state_path(&path)).unwrap();

    let removal = apply_removal(&path);
    assert!(!removal.render().contains(PAIRING));
    removal.apply().unwrap();
    let after = read(&path);
    assert_eq!(our_vendor_count(&after), 0, "{after}");
    assert!(
        after
            .as_array()
            .unwrap()
            .iter()
            .all(|vendor| vendor["name"] == "my-own-vendor")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

// --- plan(): conflicts ------------------------------------------------------

#[test]
fn conflict_on_a_foreign_vendor_entry_named_agentdesktop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!([{
            "name": "agentdesktop",
            "vendor": "customendpoint",
            "apiKey": "sk-user",
            "apiType": "chat-completions",
            "models": [{"id": "not-ours", "url": "https://not-ours.example.com/v1/chat/completions"}],
        }]))
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
fn conflict_on_a_non_array_non_object_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    // The spec's conflict shape is neither array nor object; an object root
    // (`{}`) is deliberately not used here, since it is the merge's own
    // "nothing here yet" representation for an object-shaped managed
    // document and would not exercise the same code path.
    fs::write(&path, b"42\n").unwrap();

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
    assert_eq!(fs::read(&path).unwrap(), b"42\n");
}

// --- Provider::plan: system-mode rejection ----------------------------------

#[test]
fn system_mode_rejects_a_configured_program_before_writing_anything() {
    use super::VsCode;
    use crate::provider::{Provider, ReconcileContext};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    let config = parse_programs(
        "llmGateway:\n  url: https://gateway.example\nprograms:\n  vscode:\n    models:\n      gpt-4.1-mini: {}\n",
    )
    .unwrap();
    let context = |merge_user_settings: bool| ReconcileContext {
        merge_user_settings,
        credential_helper: dir.path().join("agentdesktop"),
        socket: dir.path().join("agentdesktop.sock"),
        llm_proxy: None,
    };
    // System mode: no chat models path, or a path but no user settings.
    for provider in [
        VsCode {
            chat_models_path: None,
            settings_path: None,
        },
        VsCode {
            chat_models_path: Some(path.clone()),
            settings_path: Some(path.with_file_name("settings.json")),
        },
    ] {
        let error = match provider.plan(&context(false), &config) {
            Ok(_) => panic!("system mode with programs.vscode configured must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("--user"), "{error}");
        assert!(error.to_string().contains("VS Code"), "{error}");
    }
    assert!(!path.exists(), "preflight failure must not write any files");
}

#[test]
fn config_rejects_reserved_and_secret_keys_in_any_case() {
    for key in ["ID", "Url", "RequestHeaders", "apikey", "APIKEY", "ApiKey"] {
        let error = parse_programs(&format!(
            "programs:\n  vscode:\n    models:\n      x:\n        {key}: y\n"
        ))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("models.x."),
            "{key}: {error:#}"
        );
    }
    parse_programs("programs:\n  vscode:\n    models:\n      x:\n        apiKeyHint: y\n")
        .expect("a key that merely starts like a reserved one passes");
}

#[test]
fn github_models_through_the_provider_removes_the_chat_models_entry_and_writes_settings() {
    use super::VsCode;
    use crate::provider::{Provider, ReconcileContext};
    let dir = tempfile::tempdir().unwrap();
    let chat_models = dir.path().join("chatLanguageModels.json");
    let settings = dir.path().join("settings.json");
    let pairing: std::sync::Arc<str> = "PAIRING-FIXTURE".into();
    let context = ReconcileContext {
        merge_user_settings: true,
        credential_helper: dir.path().join("agentdesktop"),
        socket: dir.path().join("agentdesktop.sock"),
        llm_proxy: Some(crate::llm_proxy::LlmProxyContext {
            address: "127.0.0.1:4000".parse().unwrap(),
            pairing: pairing.clone(),
        }),
    };
    let provider = VsCode {
        chat_models_path: Some(chat_models.clone()),
        settings_path: Some(settings.clone()),
    };
    // First `ownModels`: the custom-models entry exists.
    let own = parse_programs(
        "llmGateway:\n  url: https://gateway.example\n  proxyUrl: https://gateway.example/copilot-proxy\nprograms:\n  vscode:\n    models:\n      gpt-4.1-mini: {}\n",
    )
    .unwrap();
    provider.plan(&context, &own).unwrap().apply().unwrap();
    assert!(chat_models.exists());
    assert!(!settings.exists());
    // Then `githubModels`: the entry goes (the file was ours, so it is deleted)
    // and the settings override arrives.
    let github = parse_programs(
        "llmGateway:\n  url: https://gateway.example\n  proxyUrl: https://gateway.example/copilot-proxy\nprograms:\n  vscode:\n    copilotChat: githubModels\n",
    )
    .unwrap();
    provider.plan(&context, &github).unwrap().apply().unwrap();
    assert!(
        !chat_models.exists(),
        "the daemon-created custom-models file is removed under githubModels"
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        written["github.copilot.advanced.debug.overrideCapiUrl"],
        "http://127.0.0.1:4000/vscode-copilot-capi/PAIRING-FIXTURE"
    );
    // And back to `ownModels`: the override goes, the entry returns.
    provider.plan(&context, &own).unwrap().apply().unwrap();
    assert!(chat_models.exists());
    assert!(
        !settings.exists(),
        "a settings file the daemon created and emptied is deleted"
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
    let path = dir.path().join("chatLanguageModels.json");
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
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = fail_open_gateway();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_OPEN_REASON)
    );
}

/// A user-owned file with our entry applied, the sidecar gone and the mode
/// loosened to 0640.
fn paired_entry_without_a_sidecar(path: &std::path::Path, gateway: &LlmGatewayConfig) {
    write_user_owned_document(path);
    apply_managed(path, listen_addr(), PAIRING, &sample_config(), gateway);
    fs::remove_file(super::super::json_merge::state_path(path)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    assert_eq!(our_vendor_count(&read(path)), 1);
}

fn assert_paired_entry_gone_user_entries_kept(path: &std::path::Path) {
    let after = read(path);
    assert_eq!(our_vendor_count(&after), 0, "{after}");
    assert!(
        after
            .as_array()
            .unwrap()
            .iter()
            .any(|vendor| vendor["name"] == "my-own-vendor"),
        "{after}"
    );
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
    let path = dir.path().join("chatLanguageModels.json");
    let config = sample_config();
    let gateway = fail_open_gateway();
    paired_entry_without_a_sidecar(&path, &gateway);

    // No proxy, so no pairing is known.
    let changes = ReconcilePlan::default();
    plan(&path, None, Some((&config, Some(&gateway))), &changes).unwrap();
    changes.apply().unwrap();
    assert_paired_entry_gone_user_entries_kept(&path);
}

#[test]
fn removal_without_proxy_and_sidecar_removes_paired_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    paired_entry_without_a_sidecar(&path, &sample_gateway());

    let changes = ReconcilePlan::default();
    plan(&path, None, None, &changes).unwrap();
    changes.apply().unwrap();
    assert_paired_entry_gone_user_entries_kept(&path);
}

#[test]
fn known_pairing_keeps_entries_with_another_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chatLanguageModels.json");
    paired_entry_without_a_sidecar(&path, &sample_gateway());

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), "PAIRING-OTHER")),
        None,
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert_eq!(our_vendor_count(&read(&path)), 1);
}
