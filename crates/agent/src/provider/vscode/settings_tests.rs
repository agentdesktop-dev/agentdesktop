//! Tests for VS Code's user `settings.json` management under the
//! `githubModels` variant of `copilotChat`, mirroring `provider::vscode::tests`
//! (`chatLanguageModels.json`) wherever the shape is shared.
//! `github_models_removes_the_managed_chat_language_models_entry_through_reconcile_plan`
//! exercises `reconcile::plan` with a `githubModels` config, which must plan a
//! removal of the custom-models entry.

use std::{collections::BTreeMap, fs, net::SocketAddr, path::PathBuf};

use agentdesktop_core::config::{
    LlmGatewayConfig, ProxyUnavailable, VsCodeConfig, VsCodeCopilotChat, VsCodeModel,
};
use jsonc_parser::{ParseOptions, cst::CstRootNode};
use serde_json::{Value, json};

use super::settings::{
    Removal, SETTINGS_STATE_VERSION, SettingsConflict, SettingsState, edit_settings, plan,
    read_state, remove_settings, settings_path,
};
use crate::provider::json_merge;
use crate::reconcile::ReconcilePlan;

/// The two keys the daemon owns in `settings.json`.
const OVERRIDE_KEY: &str = "github.copilot.advanced.debug.overrideCapiUrl";
const CAPI_ALIAS_KEY: &str = "github.copilot.internal.capiUrl";
const IGNORED_SETTINGS_KEY: &str = "settingsSync.ignoredSettings";

const LISTEN: &str = "127.0.0.1:18095";
const PAIRING: &str = "PAIRING-FIXTURE";

fn listen_addr() -> SocketAddr {
    LISTEN.parse().unwrap()
}

fn override_url(listen: SocketAddr, pairing: &str) -> String {
    format!("http://{listen}/vscode-copilot-capi/{pairing}")
}

fn github_models_config() -> VsCodeConfig {
    VsCodeConfig {
        use_llm_gateway: true,
        copilot_chat: VsCodeCopilotChat::GithubModels,
        models: BTreeMap::new(),
    }
}

fn own_models_config() -> VsCodeConfig {
    let mut models = BTreeMap::new();
    models.insert("gpt-4.1-mini".to_owned(), VsCodeModel::default());
    VsCodeConfig {
        use_llm_gateway: true,
        copilot_chat: VsCodeCopilotChat::OwnModels,
        models,
    }
}

fn gateway_with_proxy() -> LlmGatewayConfig {
    LlmGatewayConfig {
        url: "https://gateway.example.com".parse().unwrap(),
        authentication: None,
        proxy_url: Some("https://gateway.example.com/copilot-proxy".parse().unwrap()),
        github_oauth: None,
        when_proxy_unavailable: Default::default(),
    }
}

fn gateway_without_proxy_url() -> LlmGatewayConfig {
    LlmGatewayConfig {
        proxy_url: None,
        ..gateway_with_proxy()
    }
}

fn read(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn ignored_settings(document: &Value) -> Vec<String> {
    document[IGNORED_SETTINGS_KEY]
        .as_array()
        .unwrap_or_else(|| panic!("{IGNORED_SETTINGS_KEY} missing or not an array in {document}"))
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
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

fn apply_removal(
    path: &std::path::Path,
    proxy: Option<(SocketAddr, &str)>,
    configured: Option<(&VsCodeConfig, Option<&LlmGatewayConfig>)>,
) -> ReconcilePlan {
    let changes = ReconcilePlan::default();
    plan(path, proxy, configured, &changes).unwrap();
    changes
}

fn write_user_settings(path: &std::path::Path, document: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(document).unwrap()).unwrap();
}

// --- Helpers for the pure-function (`edit_settings`/`remove_settings`)
// tests: the strict VS Code JSONC grammar (comments and trailing commas,
// nothing else of JSON5's permissive defaults) and a comment-tolerant way to
// check semantic content without caring about formatting.

fn vscode_parse_options() -> ParseOptions {
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

/// The semantic content of a (possibly commented) `settings.json` text, for
/// assertions that do not care about comments or formatting.
fn parse_jsonc(text: &str) -> Value {
    CstRootNode::parse(text, &vscode_parse_options())
        .unwrap_or_else(|error| panic!("valid VS Code JSONC: {error}\n{text}"))
        .to_serde_value()
        .unwrap_or_else(|| panic!("object or array root:\n{text}"))
}

/// Every line of `original` still appears, in the same relative order
/// (insertions between them are allowed), in `edited`: the
/// byte-identical-preservation check for a fixture with comments and a
/// trailing comma, where the exact bytes of the *inserted* lines are an
/// implementation detail. Splitting on `'\n'` (not `str::lines`, which
/// strips a trailing `\r`) keeps a CRLF line ending part of what must match.
fn assert_original_lines_preserved_in_order(original: &str, edited: &str) {
    let mut edited_lines = edited.split('\n');
    for line in original.split('\n') {
        assert!(
            edited_lines.any(|candidate| candidate == line),
            "line {line:?} missing or reordered after the edit:\n--- original ---\n{original}\n--- edited ---\n{edited}"
        );
    }
}

const MANAGED_URL: &str = "http://127.0.0.1:18095/vscode-copilot-capi/PAIRING-FIXTURE";
const OTHER_URL: &str = "http://127.0.0.1:18099/vscode-copilot-capi/PAIRING-OTHER";

// --- Fresh apply -------------------------------------------------------

#[test]
fn fresh_file_gets_both_managed_keys_and_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let written = read(&path);
    assert_eq!(written[OVERRIDE_KEY], override_url(listen_addr(), PAIRING));
    let ignored = ignored_settings(&written);
    assert!(ignored.contains(&OVERRIDE_KEY.to_owned()), "{written}");
    assert!(ignored.contains(&CAPI_ALIAS_KEY.to_owned()), "{written}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "settings.json carries the pairing and must be owner-only"
        );
    }
    assert!(
        super::super::json_merge::state_path(&path).exists(),
        "a merge sidecar must be written alongside the created file"
    );
}

// --- User content survives re-apply and re-pairing --------------------

#[test]
fn user_keys_and_a_user_ignored_settings_entry_survive_reapply_and_repairing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(
        &path,
        &json!({
            "editor.fontSize": 14,
            IGNORED_SETTINGS_KEY: ["some.other.userSetting"],
        }),
    );
    let config = github_models_config();
    let gateway = gateway_with_proxy();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let after_first = read(&path);
    assert_eq!(after_first["editor.fontSize"], 14);
    let ignored = ignored_settings(&after_first);
    assert!(ignored.contains(&"some.other.userSetting".to_owned()));
    assert!(ignored.contains(&OVERRIDE_KEY.to_owned()));
    assert!(ignored.contains(&CAPI_ALIAS_KEY.to_owned()));

    // Re-apply unchanged: nothing lost, nothing duplicated.
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let after_reapply = read(&path);
    assert_eq!(after_reapply["editor.fontSize"], 14);
    let ignored = ignored_settings(&after_reapply);
    assert_eq!(
        ignored
            .iter()
            .filter(|entry| *entry == &"some.other.userSetting".to_owned())
            .count(),
        1
    );
    assert_eq!(
        ignored
            .iter()
            .filter(|entry| *entry == &OVERRIDE_KEY.to_owned())
            .count(),
        1
    );

    // Re-pairing (new listen address and pairing value, e.g. FRESH=1
    // re-enrollment): the URL changes, everything else is unchanged.
    let new_listen: SocketAddr = "127.0.0.1:18099".parse().unwrap();
    apply_managed(&path, new_listen, "PAIRING-NEW", &config, &gateway);
    let after_repair = read(&path);
    assert_eq!(after_repair["editor.fontSize"], 14);
    assert_eq!(
        after_repair[OVERRIDE_KEY],
        override_url(new_listen, "PAIRING-NEW")
    );
    let ignored = ignored_settings(&after_repair);
    assert!(ignored.contains(&"some.other.userSetting".to_owned()));
}

// --- Removal: one test per trigger, each keeping the user's keys -----

#[test]
fn removal_program_absent_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // `programs.vscode` removed entirely.
    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn removal_own_models_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let github_models = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &github_models, &gateway);

    // `copilotChat` switched back to `ownModels`: settings.json has nothing
    // of this variant's to keep, even though the program is still configured
    // and the gateway is still on.
    let own_models = own_models_config();
    apply_removal(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&own_models, Some(&gateway))),
    )
    .apply()
    .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn removal_use_llm_gateway_false_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // `useLlmGateway: false`: the caller passes the program's config with the
    // gateway filtered to `None`.
    apply_removal(&path, Some((listen_addr(), PAIRING)), Some((&config, None)))
        .apply()
        .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn removal_no_top_level_gateway_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // No top-level `llmGateway` at all: same call shape as `useLlmGateway:
    // false` (the caller filters either way), a distinct real-world trigger.
    apply_removal(&path, Some((listen_addr(), PAIRING)), Some((&config, None)))
        .apply()
        .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn removal_no_proxy_url_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // `llmGateway.proxyUrl` unset: the gateway is still configured and used,
    // but this route has nowhere to forward to.
    let gateway_without_proxy = gateway_without_proxy_url();
    apply_removal(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway_without_proxy))),
    )
    .apply()
    .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn fail_open_removal_no_proxy_available_keeps_user_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let mut gateway = gateway_with_proxy();
    gateway.when_proxy_unavailable = ProxyUnavailable::FailOpen;
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    // `ctx.llm_proxy` is `None` (bind failed, proxy off) even though the
    // program is still configured with a usable gateway.
    apply_removal(&path, None, Some((&config, Some(&gateway))))
        .apply()
        .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
}

#[test]
fn fail_closed_leaves_the_file_and_sidecar_until_the_proxy_is_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let state = json_merge::state_path(&path);
    let (before, state_before) = (fs::read(&path).unwrap(), fs::read(&state).unwrap());

    let changes = apply_removal(&path, None, Some((&config, Some(&gateway))));
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_REASON)
    );
    changes.apply().unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "the override stays, pointed at the loopback port"
    );
    assert_eq!(fs::read(&state).unwrap(), state_before);

    // The proxy back, on another port: the current URL is written.
    let other: SocketAddr = "127.0.0.1:18199".parse().unwrap();
    apply_managed(&path, other, PAIRING, &config, &gateway);
    assert_eq!(read(&path)[OVERRIDE_KEY], override_url(other, PAIRING));

    // Removing the program still removes, whatever the policy.
    apply_removal(&path, None, None).apply().unwrap();
    let after = read(&path);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
    assert_eq!(after["editor.fontSize"], 14);
}

#[test]
fn fail_closed_does_not_create_a_file_without_the_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let changes = apply_removal(
        &path,
        None,
        Some((&github_models_config(), Some(&gateway_with_proxy()))),
    );
    changes.apply().unwrap();
    assert!(!path.exists());
}

#[test]
fn when_proxy_unavailable_defaults_to_fail_closed_and_parses_fail_open() {
    let parse = |extra: Value| {
        let mut document = json!({"url": "https://gateway.example.com"});
        document
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<LlmGatewayConfig>(document)
    };
    let default = parse(json!({})).unwrap();
    assert_eq!(default.when_proxy_unavailable, ProxyUnavailable::FailClosed);
    assert!(
        serde_json::to_value(&default)
            .unwrap()
            .get("whenProxyUnavailable")
            .is_none()
    );
    assert_eq!(
        parse(json!({"whenProxyUnavailable": "failOpen"}))
            .unwrap()
            .when_proxy_unavailable,
        ProxyUnavailable::FailOpen
    );
    assert!(parse(json!({"whenProxyUnavailable": "failSoft"})).is_err());
}

#[test]
fn removal_of_a_created_and_emptied_file_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    assert!(path.exists());

    // Emptied by hand afterwards: a file the merge created is removed whole,
    // not left as an empty object.
    fs::write(&path, "").unwrap();
    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();

    assert!(
        !path.exists(),
        "a settings.json we created and the user then emptied must be removed"
    );
    assert!(!super::super::json_merge::state_path(&path).exists());
}

// --- File mode ---------------------------------------------------------

#[cfg(unix)]
#[test]
fn mode_0644_becomes_0600_on_merge_and_removal_keeps_the_mode_it_finds() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "an existing 0644 settings.json must be tightened on merge"
    );

    // The user (or another tool) sets an unusual mode after the merge;
    // removal must not loosen or tighten it further.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640,
        "removal must keep the mode it finds, not the merge's own 0600"
    );
}

// --- Redaction ---------------------------------------------------------

#[test]
fn redaction_keeps_the_pairing_out_of_render() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();

    let changes = ReconcilePlan::default();
    plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .unwrap();
    assert!(
        !changes.render().contains(PAIRING),
        "the plan report must not leak the pairing value: {}",
        changes.render()
    );
}

// --- Pre-existing user override (json_merge rollback) -----------------

#[test]
fn preexisting_user_override_capi_url_is_overwritten_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(
        &path,
        &json!({ OVERRIDE_KEY: "http://someone-else.example:9999/other" }),
    );
    let config = github_models_config();
    let gateway = gateway_with_proxy();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let after_merge = read(&path);
    assert_eq!(
        after_merge[OVERRIDE_KEY],
        override_url(listen_addr(), PAIRING)
    );

    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();
    let after_removal = read(&path);
    assert_eq!(
        after_removal[OVERRIDE_KEY], "http://someone-else.example:9999/other",
        "the user's own overrideCapiUrl, set before agentdesktop ever wrote the file, must be restored"
    );
}

// --- Comments and trailing commas are preserved, not a conflict ------
//
// The in-place edit keeps comments and trailing commas, so applying a file
// that has them succeeds.

#[test]
fn comments_and_trailing_commas_are_preserved() {
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    for (name, contents) in [
        (
            "comment",
            "{\n  // a user comment\n  \"editor.fontSize\": 14\n}\n",
        ),
        ("trailing comma", "{\n  \"editor.fontSize\": 14,\n}\n"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, contents).unwrap();

        let changes = ReconcilePlan::default();
        plan(
            &path,
            Some((listen_addr(), PAIRING)),
            Some((&config, Some(&gateway))),
            &changes,
        )
        .unwrap();
        assert!(!changes.has_conflicts(), "{name}: {}", changes.render());
        changes.apply().unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_original_lines_preserved_in_order(contents, &written);
        let document = parse_jsonc(&written);
        assert_eq!(
            document[OVERRIDE_KEY],
            override_url(listen_addr(), PAIRING),
            "{name}: {document}"
        );
    }
}

// --- settings_path per OS ----------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn settings_path_on_linux() {
    assert_eq!(
        settings_path(std::path::Path::new("/home/user")),
        PathBuf::from("/home/user/.config/Code/User/settings.json")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn settings_path_on_macos() {
    assert_eq!(
        settings_path(std::path::Path::new("/Users/user")),
        PathBuf::from("/Users/user/Library/Application Support/Code/User/settings.json")
    );
}

#[cfg(windows)]
#[test]
fn settings_path_on_windows() {
    assert_eq!(
        settings_path(std::path::Path::new(r"C:\Users\user")),
        PathBuf::from(r"C:\Users\user").join("AppData/Roaming/Code/User/settings.json")
    );
}

// --- githubModels removes the managed chatLanguageModels.json entry, through
// the existing reconcile::plan. -----------------------------------------

#[test]
fn github_models_removes_the_managed_chat_language_models_entry_through_reconcile_plan() {
    use super::reconcile::{chat_models_path, plan as chat_models_plan};

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let path = chat_models_path(home);
    let mut own_models = own_models_config();
    own_models.copilot_chat = VsCodeCopilotChat::OwnModels;
    let gateway = gateway_with_proxy();

    // The `ownModels` entry exists (a previous revision had it configured).
    let changes = ReconcilePlan::default();
    chat_models_plan(
        &path,
        Some((listen_addr(), PAIRING)),
        Some((&own_models, Some(&gateway))),
        &changes,
    )
    .unwrap();
    changes.apply().unwrap();
    assert!(path.exists());

    // `copilotChat` switches to `githubModels`: `reconcile::plan` is called
    // with `configured = None`, since this file has nothing to manage under
    // that variant. It must plan (and apply) a removal.
    let changes = ReconcilePlan::default();
    chat_models_plan(&path, Some((listen_addr(), PAIRING)), None, &changes).unwrap();
    assert!(!changes.has_conflicts(), "{}", changes.render());
    changes.apply().unwrap();

    assert!(
        !path.exists(),
        "chatLanguageModels.json must be cleaned up once githubModels is active"
    );
}

// Owned override: these exercise `edit_settings`/`remove_settings` directly,
// chained the way `plan` chains them across a re-apply.

// A hand edit of the managed URL after the first apply is drift, not the
// user's value; a re-apply replaces it and removal must not put it back.
#[test]
fn hand_edited_override_is_replaced_on_reapply_and_not_restored_on_removal() {
    let fixture = "{\n  \"editor.fontSize\": 14\n}\n";

    let (text1, state1) = edit_settings(Some(fixture), MANAGED_URL, None).unwrap();
    assert_eq!(state1.override_before, None, "no user override existed");

    // Hand edit of our URL, then a daemon restart (re-apply against the
    // sidecar from the first apply).
    let text2 = text1.replacen(MANAGED_URL, OTHER_URL, 1);
    assert_ne!(text2, text1, "the fixture must contain MANAGED_URL once");
    let (text3, state2) = edit_settings(Some(&text2), MANAGED_URL, Some(&state1)).unwrap();
    assert_eq!(
        parse_jsonc(&text3)[OVERRIDE_KEY],
        MANAGED_URL,
        "hand edit replaced: {text3}"
    );
    assert_eq!(
        state2.override_before, None,
        "still no user override to restore"
    );

    // Removal: the key goes entirely; the hand-edited value was never "the
    // user's" to restore.
    let removed = remove_settings(&text3, Some(&state2), None).unwrap();
    let Removal::Write(remaining_text) = removed else {
        panic!("editor.fontSize must survive: {removed:?}");
    };
    let remaining = parse_jsonc(&remaining_text);
    assert!(
        remaining.get(OVERRIDE_KEY).is_none(),
        "stale override restored: {remaining}"
    );
    assert!(
        remaining.get(IGNORED_SETTINGS_KEY).is_none(),
        "the ignore array we created must go too: {remaining}"
    );
    assert_eq!(remaining["editor.fontSize"], 14);
}

// The user's own override from before the first apply survives a hand edit of
// the managed URL and comes back on removal.
#[test]
fn user_override_from_before_the_first_apply_survives_a_hand_edit_and_returns_on_removal() {
    let users_own = "https://capi.example.invalid";
    let fixture = format!("{{\n  \"{OVERRIDE_KEY}\": \"{users_own}\"\n}}\n");

    let (text1, state1) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    assert_eq!(
        state1.override_before,
        Some(Value::String(users_own.to_owned())),
        "the pre-existing user override must be captured"
    );
    assert_eq!(parse_jsonc(&text1)[OVERRIDE_KEY], MANAGED_URL);

    let text2 = text1.replacen(MANAGED_URL, OTHER_URL, 1);
    let (text3, state2) = edit_settings(Some(&text2), MANAGED_URL, Some(&state1)).unwrap();
    assert_eq!(
        parse_jsonc(&text3)[OVERRIDE_KEY],
        MANAGED_URL,
        "hand edit replaced: {text3}"
    );
    assert_eq!(
        state2.override_before,
        Some(Value::String(users_own.to_owned())),
        "overrideBefore is kept across the re-apply, not recomputed from the hand edit"
    );

    let removed = remove_settings(&text3, Some(&state2), None).unwrap();
    let Removal::Write(remaining_text) = removed else {
        panic!("the user's own override must survive: {removed:?}");
    };
    assert_eq!(
        parse_jsonc(&remaining_text)[OVERRIDE_KEY],
        users_own,
        "the user's own value comes back: {remaining_text}"
    );
}

// =========================================================================
// Pure-function tests on `edit_settings`/`remove_settings`/`read_state`, no
// filesystem.
// =========================================================================

/// Comments (line and block), a trailing comma, CRLF, and a user key on
/// either side of where the override will be appended (no override or
/// `settingsSync.ignoredSettings` yet).
const FIXTURE_NO_OVERRIDE: &str = "{\r\n  // top comment\r\n  \"a.userSetting\": 1,\r\n  /* block comment */\r\n  \"z.userSetting\": true,\r\n}\r\n";

/// A pre-existing override, with a same-line comment, sandwiched between two
/// user keys.
const FIXTURE_WITH_OVERRIDE: &str = "{\n  \"a.userSetting\": 1,\n  \"github.copilot.advanced.debug.overrideCapiUrl\": \"https://old.example/capi\", // old\n  \"z.userSetting\": true\n}\n";
const FIXTURE_WITH_OVERRIDE_VALUE: &str = "https://old.example/capi";

// --- edit_settings: fresh apply ----------------------------------------

#[test]
fn an_absent_file_is_created_as_the_exact_golden_text() {
    let (edited, state) = edit_settings(None, MANAGED_URL, None).unwrap();
    assert_eq!(
        edited,
        format!(
            "{{\n  \"{OVERRIDE_KEY}\": \"{MANAGED_URL}\",\n  \"{IGNORED_SETTINGS_KEY}\": [\"{OVERRIDE_KEY}\", \"{CAPI_ALIAS_KEY}\"]\n}}\n"
        ),
        "the golden text is fixed exactly"
    );
    assert!(state.created, "the file did not exist before this apply");
    assert!(state.ignored_created);
    assert_eq!(
        state.added_ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );
    assert_eq!(state.override_before, None);
    assert_eq!(state.version, SETTINGS_STATE_VERSION);
}

#[test]
fn an_empty_or_whitespace_only_existing_file_is_filled_like_absent_but_is_not_created() {
    for text in ["", "   \n\t"] {
        let (edited, state) = edit_settings(Some(text), MANAGED_URL, None).unwrap();
        assert!(
            !state.created,
            "the file existed (just empty), unlike an absent one: {text:?}"
        );
        let document = parse_jsonc(&edited);
        assert_eq!(document[OVERRIDE_KEY], MANAGED_URL);
        assert_eq!(
            ignored_settings(&document),
            vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
        );
    }
}

#[test]
fn byte_identical_preservation_for_a_commented_crlf_trailing_comma_fixture_with_the_override_appended()
 {
    let (edited, state) = edit_settings(Some(FIXTURE_NO_OVERRIDE), MANAGED_URL, None).unwrap();
    assert_original_lines_preserved_in_order(FIXTURE_NO_OVERRIDE, &edited);

    let document = parse_jsonc(&edited);
    assert_eq!(document[OVERRIDE_KEY], MANAGED_URL);
    assert_eq!(
        ignored_settings(&document),
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );
    assert!(!state.created);
    assert!(state.ignored_created);
    assert_eq!(state.override_before, None);
}

#[test]
fn override_is_replaced_in_place_at_its_position_keeping_its_same_line_comment() {
    let (edited, state) = edit_settings(Some(FIXTURE_WITH_OVERRIDE), MANAGED_URL, None).unwrap();
    assert!(
        edited.contains(&format!("\"{OVERRIDE_KEY}\": \"{MANAGED_URL}\", // old")),
        "the value changes but the same-line comment travels with it: {edited}"
    );
    let a_index = edited.find("\"a.userSetting\"").unwrap();
    let override_index = edited.find(&format!("\"{OVERRIDE_KEY}\": ")).unwrap();
    let z_index = edited.find("\"z.userSetting\"").unwrap();
    assert!(
        a_index < override_index && override_index < z_index,
        "the override must stay between the two user keys, only its value changing: {edited}"
    );
    assert_eq!(
        state.override_before,
        Some(Value::String(FIXTURE_WITH_OVERRIDE_VALUE.to_owned()))
    );
}

#[test]
fn ignore_array_created_extended_and_user_entries_kept_in_order() {
    // Absent entirely: created fresh, with only our two entries.
    let (edited, state) = edit_settings(Some("{\n  \"a\": 1\n}\n"), MANAGED_URL, None).unwrap();
    let ignored = ignored_settings(&parse_jsonc(&edited));
    assert_eq!(
        ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );
    assert!(state.ignored_created);
    assert_eq!(state.added_ignored, ignored);

    // Present with a user entry: extended, the user's entry kept first.
    let fixture =
        format!("{{\n  \"a\": 1,\n  \"{IGNORED_SETTINGS_KEY}\": [\"custom.setting\"]\n}}\n");
    let (edited, state) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    let ignored = ignored_settings(&parse_jsonc(&edited));
    assert_eq!(
        ignored,
        vec![
            "custom.setting".to_owned(),
            OVERRIDE_KEY.to_owned(),
            CAPI_ALIAS_KEY.to_owned(),
        ]
    );
    assert!(!state.ignored_created, "the array already existed");
    assert_eq!(
        state.added_ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );
}

#[test]
fn a_user_held_ignore_entry_of_ours_is_not_added_and_survives_removal() {
    let fixture =
        format!("{{\n  \"a\": 1,\n  \"{IGNORED_SETTINGS_KEY}\": [\"{OVERRIDE_KEY}\"]\n}}\n");
    let (edited, state) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    assert_eq!(
        state.added_ignored,
        vec![CAPI_ALIAS_KEY.to_owned()],
        "the override key was already user-held, only the alias is ours to add"
    );
    let ignored = ignored_settings(&parse_jsonc(&edited));
    assert_eq!(
        ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );

    let removed = remove_settings(&edited, Some(&state), None).unwrap();
    let Removal::Write(remaining) = removed else {
        panic!("editor.fontSize and the user's own entry must survive: {removed:?}");
    };
    let document = parse_jsonc(&remaining);
    assert_eq!(
        ignored_settings(&document),
        vec![OVERRIDE_KEY.to_owned()],
        "the user's own entry must survive removal, the alias we added must not: {document}"
    );
}

#[test]
fn a_users_own_empty_ignored_settings_array_comes_back_as_empty() {
    let fixture = format!("{{\n  \"a\": 1,\n  \"{IGNORED_SETTINGS_KEY}\": []\n}}\n");
    let (edited, state) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    assert!(
        !state.ignored_created,
        "the property already existed, empty"
    );
    assert_eq!(
        state.added_ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );

    let removed = remove_settings(&edited, Some(&state), None).unwrap();
    let Removal::Write(remaining) = removed else {
        panic!("{removed:?}");
    };
    let document = parse_jsonc(&remaining);
    assert_eq!(
        document[IGNORED_SETTINGS_KEY],
        json!([]),
        "the user's own empty array must come back, not be removed: {document}"
    );
}

#[test]
fn a_null_override_is_restored_as_null() {
    let fixture = format!("{{\n  \"{OVERRIDE_KEY}\": null\n}}\n");
    let (edited, state) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    assert_eq!(
        state.override_before,
        Some(Value::Null),
        "an explicit null must be kept as Some(Value::Null), not treated as absent"
    );
    assert_eq!(parse_jsonc(&edited)[OVERRIDE_KEY], MANAGED_URL);

    let removed = remove_settings(&edited, Some(&state), None).unwrap();
    let Removal::Write(remaining) = removed else {
        panic!("{removed:?}");
    };
    assert_eq!(
        parse_jsonc(&remaining)[OVERRIDE_KEY],
        Value::Null,
        "null must be restored, not the key removed"
    );
}

// --- remove_settings: restoring what edit_settings changed --------------

#[test]
fn removal_restores_the_pre_apply_fixture_byte_for_byte() {
    let (edited, state) = edit_settings(Some(FIXTURE_NO_OVERRIDE), MANAGED_URL, None).unwrap();
    let removed = remove_settings(&edited, Some(&state), None).unwrap();
    assert_eq!(removed, Removal::Write(FIXTURE_NO_OVERRIDE.to_owned()));
}

#[test]
fn users_own_override_is_restored_with_its_same_line_comment() {
    let (edited, state) = edit_settings(Some(FIXTURE_WITH_OVERRIDE), MANAGED_URL, None).unwrap();
    assert_eq!(
        state.override_before,
        Some(Value::String(FIXTURE_WITH_OVERRIDE_VALUE.to_owned()))
    );

    let removed = remove_settings(&edited, Some(&state), None).unwrap();
    assert_eq!(removed, Removal::Write(FIXTURE_WITH_OVERRIDE.to_owned()));
}

#[test]
fn user_deleted_the_override_after_apply_removal_appends_override_before() {
    let (edited, state) = edit_settings(Some(FIXTURE_WITH_OVERRIDE), MANAGED_URL, None).unwrap();
    assert_eq!(
        state.override_before,
        Some(Value::String(FIXTURE_WITH_OVERRIDE_VALUE.to_owned()))
    );

    // The user deletes the whole override property line by hand; whatever we
    // appended (the ignore entries) stays.
    let without_override: String = edited
        .lines()
        .filter(|line| {
            !line
                .trim_start()
                .starts_with(&format!("\"{OVERRIDE_KEY}\":"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(
        parse_jsonc(&without_override).get(OVERRIDE_KEY).is_none(),
        "the fixture must no longer have the property"
    );

    let removed = remove_settings(&without_override, Some(&state), None).unwrap();
    let Removal::Write(remaining) = removed else {
        panic!("{removed:?}");
    };
    let document = parse_jsonc(&remaining);
    assert_eq!(
        document[OVERRIDE_KEY], FIXTURE_WITH_OVERRIDE_VALUE,
        "the property must be appended back, not left missing: {document}"
    );
}

#[test]
fn a_created_file_is_deleted_when_only_braces_or_nothing_remain_but_kept_with_a_comment() {
    let state = SettingsState {
        version: SETTINGS_STATE_VERSION,
        created: true,
        override_before: None,
        added_ignored: vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()],
        ignored_created: true,
    };
    for remaining in ["{}\n", "", "   \n"] {
        assert_eq!(
            remove_settings(remaining, Some(&state), None).unwrap(),
            Removal::Delete,
            "{remaining:?}"
        );
    }
    // A comment is something to keep, even though nothing else is left.
    let with_comment = "{\n  // still here\n}\n";
    let removed = remove_settings(with_comment, Some(&state), None).unwrap();
    assert!(
        !matches!(removed, Removal::Delete),
        "a remaining comment must keep the file: {removed:?}"
    );
}

// --- Conflicts -----------------------------------------------------------

#[test]
fn edit_settings_conflicts() {
    let cases: Vec<(&str, String, SettingsConflict)> = vec![
        (
            "parse error",
            "{ \"a\": }".to_owned(),
            SettingsConflict::Parse,
        ),
        (
            "missing comma",
            "{ \"a\": 1 \"b\": 2 }".to_owned(),
            SettingsConflict::Parse,
        ),
        (
            "single-quoted string",
            "{ 'a': 1 }".to_owned(),
            SettingsConflict::Parse,
        ),
        (
            "non-object root",
            "[1, 2]".to_owned(),
            SettingsConflict::NotAnObject,
        ),
        (
            "non-array ignoredSettings",
            format!("{{ \"{IGNORED_SETTINGS_KEY}\": 5 }}"),
            SettingsConflict::IgnoredNotArray,
        ),
        (
            "duplicate override key",
            format!("{{ \"{OVERRIDE_KEY}\": \"a\", \"{OVERRIDE_KEY}\": \"b\" }}"),
            SettingsConflict::DuplicateKey,
        ),
    ];
    for (name, text, expected) in cases {
        assert_eq!(
            edit_settings(Some(&text), MANAGED_URL, None),
            Err(expected),
            "{name}: {text}"
        );
    }
}

#[test]
fn remove_settings_without_a_sidecar_treats_an_unparseable_file_as_unchanged_not_a_conflict() {
    // Matches today's `plan_remove_orphaned` (settings.rs:157-163): skipped
    // with a debug line, not a hard conflict, since there is no sidecar to
    // say anything was ever ours.
    assert_eq!(
        remove_settings("{ \"a\": }", None, Some(MANAGED_URL)),
        Ok(Removal::Unchanged)
    );
}

#[test]
fn remove_settings_with_a_sidecar_treats_an_unparseable_file_as_a_conflict() {
    let state = SettingsState {
        version: SETTINGS_STATE_VERSION,
        created: false,
        override_before: None,
        added_ignored: vec![OVERRIDE_KEY.to_owned()],
        ignored_created: false,
    };
    assert_eq!(
        remove_settings("{ \"a\": }", Some(&state), None),
        Err(SettingsConflict::Parse)
    );
}

// --- read_state: the earlier whole-document sidecar upgraded --------------

#[test]
fn a_v1_sidecar_is_read_and_upgraded() {
    let before = json!({ "a": 1, OVERRIDE_KEY: FIXTURE_WITH_OVERRIDE_VALUE });
    let after = json!({
        "a": 1,
        OVERRIDE_KEY: MANAGED_URL,
        IGNORED_SETTINGS_KEY: [OVERRIDE_KEY, CAPI_ALIAS_KEY],
    });
    let v1 = json!({ "created": false, "before": before, "after": after });
    let bytes = serde_json::to_vec(&v1).unwrap();

    let state = read_state(&bytes).expect("a v1 sidecar (no `version` field) must be recognized");
    assert_eq!(state.version, SETTINGS_STATE_VERSION);
    assert!(!state.created);
    assert_eq!(
        state.override_before,
        Some(Value::String(FIXTURE_WITH_OVERRIDE_VALUE.to_owned())),
        "override_before = before[OVERRIDE_KEY]"
    );
    assert_eq!(
        state.added_ignored,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()],
        "our two entries were not in `before.settingsSync.ignoredSettings`"
    );
    assert!(
        state.ignored_created,
        "`before.settingsSync.ignoredSettings` was absent"
    );
}

#[test]
fn read_state_is_none_for_bytes_that_are_neither_v1_nor_v2() {
    assert!(read_state(b"not json at all").is_none());
}

// --- SettingsState's own serde shape (exact camelCase, deny_unknown_fields,
// an explicit null override_before kept distinct from an absent one) -----

#[test]
fn settings_state_serializes_camelcase_and_keeps_an_explicit_null_override_before() {
    let with_null = SettingsState {
        version: SETTINGS_STATE_VERSION,
        created: false,
        override_before: Some(Value::Null),
        added_ignored: vec![OVERRIDE_KEY.to_owned()],
        ignored_created: true,
    };
    let json = serde_json::to_value(&with_null).unwrap();
    assert_eq!(
        json,
        json!({
            "version": 2,
            "created": false,
            "overrideBefore": null,
            "addedIgnored": [OVERRIDE_KEY],
            "ignoredCreated": true,
        })
    );
    let round_tripped: SettingsState = serde_json::from_value(json).unwrap();
    assert_eq!(round_tripped, with_null);

    // Absent (no user override existed before the first apply): the field is
    // omitted entirely, not written as `null`.
    let without = SettingsState {
        override_before: None,
        ..with_null.clone()
    };
    let json = serde_json::to_value(&without).unwrap();
    assert!(
        json.get("overrideBefore").is_none(),
        "skip_serializing_if must omit an absent override: {json}"
    );
    let round_tripped: SettingsState = serde_json::from_value(json).unwrap();
    assert_eq!(round_tripped.override_before, None);

    // deny_unknown_fields: a stray field must not silently pass through.
    let mut with_extra = serde_json::to_value(&without).unwrap();
    with_extra["unexpectedField"] = json!(true);
    assert!(serde_json::from_value::<SettingsState>(with_extra).is_err());
}

// =========================================================================
// Plan-level tests (existing helpers): the v2 sidecar and the in-place edit
// through `plan`/`apply`.
// =========================================================================

#[test]
fn unchanged_reapply_writes_nothing_and_leaves_the_sidecar_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let settings_before = fs::read(&path).unwrap();
    let state_path = super::super::json_merge::state_path(&path);
    let sidecar_before = fs::read(&state_path).unwrap();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    assert_eq!(
        fs::read(&path).unwrap(),
        settings_before,
        "settings.json must not be rewritten when nothing changes"
    );
    assert_eq!(
        fs::read(&state_path).unwrap(),
        sidecar_before,
        "the sidecar must only be rewritten when its content changes"
    );
}

#[cfg(unix)]
#[test]
fn unchanged_reapply_on_a_0664_file_is_byte_identical_but_tightened_to_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let bytes_before = fs::read(&path).unwrap();

    // VS Code's own save: same bytes, looser mode.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    assert_eq!(
        fs::read(&path).unwrap(),
        bytes_before,
        "bytes must be identical"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "a file looser than 0600 must be tightened even when nothing else changed"
    );
}

#[test]
fn v2_sidecar_has_the_exact_camelcase_fields_after_a_fresh_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let state_path = super::super::json_merge::state_path(&path);
    let sidecar: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(sidecar["version"], 2, "{sidecar}");
    assert_eq!(sidecar["created"], true, "{sidecar}");
    assert!(sidecar.get("overrideBefore").is_none(), "{sidecar}");
    assert_eq!(sidecar["ignoredCreated"], true, "{sidecar}");
    let added: Vec<String> = sidecar["addedIgnored"]
        .as_array()
        .unwrap_or_else(|| panic!("{sidecar}"))
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        added,
        vec![OVERRIDE_KEY.to_owned(), CAPI_ALIAS_KEY.to_owned()]
    );

    let typed: SettingsState = serde_json::from_value(sidecar).unwrap();
    assert_eq!(typed.version, SETTINGS_STATE_VERSION);
}

/// An overridden URL the user owns; must never appear in an error message.
const SECRET: &str = "https://secret-user-override.example.com/v1";

/// `settings.json` holding the user's override, applied once so a valid
/// sidecar exists. Returns the settings path and the sidecar path.
fn applied_with_user_override() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(
        &path,
        &json!({ OVERRIDE_KEY: SECRET, "editor.fontSize": 14 }),
    );
    apply_managed(
        &path,
        listen_addr(),
        PAIRING,
        &github_models_config(),
        &gateway_with_proxy(),
    );
    let state_path = super::super::json_merge::state_path(&path);
    (dir, path, state_path)
}

/// Plans an apply and returns the plan error text, asserting nothing was
/// written (the plan failed before `apply`).
fn apply_plan_error(path: &std::path::Path) -> String {
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    let changes = ReconcilePlan::default();
    let error = plan(
        path,
        Some((listen_addr(), PAIRING)),
        Some((&config, Some(&gateway))),
        &changes,
    )
    .expect_err("an unreadable sidecar must fail the plan");
    format!("{error:#}")
}

fn removal_plan_error(path: &std::path::Path) -> String {
    let changes = ReconcilePlan::default();
    let error = plan(path, None, None, &changes)
        .expect_err("an unreadable sidecar must fail the removal plan");
    format!("{error:#}")
}

#[test]
fn corrupt_sidecar_fails_the_plan_on_apply() {
    let (_dir, path, state_path) = applied_with_user_override();
    fs::write(&state_path, b"{ not json").unwrap();
    let settings_before = fs::read(&path).unwrap();

    let message = apply_plan_error(&path);

    assert!(
        message.contains(&state_path.display().to_string()),
        "{message}"
    );
    assert_eq!(fs::read(&path).unwrap(), settings_before);
    assert_eq!(fs::read(&state_path).unwrap(), b"{ not json");
}

#[test]
fn corrupt_sidecar_fails_the_plan_on_removal() {
    let (_dir, path, state_path) = applied_with_user_override();
    fs::write(&state_path, b"{ not json").unwrap();
    let settings_before = fs::read(&path).unwrap();

    let message = removal_plan_error(&path);

    assert!(
        message.contains(&state_path.display().to_string()),
        "{message}"
    );
    assert_eq!(fs::read(&path).unwrap(), settings_before);
    assert_eq!(fs::read(&state_path).unwrap(), b"{ not json");
}

#[test]
fn sidecar_with_unknown_version_fails_the_plan() {
    let (_dir, path, state_path) = applied_with_user_override();
    let bytes = serde_json::to_vec(&json!({
        "version": 99,
        "created": false,
        "overrideBefore": SECRET,
        "addedIgnored": [],
        "ignoredCreated": false,
    }))
    .unwrap();
    fs::write(&state_path, &bytes).unwrap();

    let message = apply_plan_error(&path);
    assert!(
        message.contains(&state_path.display().to_string()),
        "{message}"
    );
    let message = removal_plan_error(&path);
    assert!(
        message.contains(&state_path.display().to_string()),
        "{message}"
    );
    assert_eq!(fs::read(&state_path).unwrap(), bytes);
}

#[test]
fn sidecar_with_wrong_shape_fails_the_plan() {
    for shape in [
        // Current version, a field missing and an unknown one.
        json!({ "version": SETTINGS_STATE_VERSION, "bogus": SECRET }),
        // No version, not the whole-document legacy form either.
        json!({ "unrelated": SECRET }),
        // Valid JSON, not an object.
        json!([SECRET]),
    ] {
        let (_dir, path, state_path) = applied_with_user_override();
        let bytes = serde_json::to_vec(&shape).unwrap();
        fs::write(&state_path, &bytes).unwrap();

        let message = apply_plan_error(&path);
        assert!(
            message.contains(&state_path.display().to_string()),
            "{shape}: {message}"
        );
        assert_eq!(fs::read(&state_path).unwrap(), bytes, "{shape}");
    }
}

#[test]
fn the_unreadable_sidecar_error_quotes_nothing_from_the_file() {
    let (_dir, path, state_path) = applied_with_user_override();
    // Invalid JSON and valid-but-wrong-shape JSON, both carrying the secret.
    for bytes in [
        format!("{{\n  \"overrideBefore\": \"{SECRET}\" oops\n}}").into_bytes(),
        format!("{{\"version\": 2, \"overrideBefore\": \"{SECRET}\"}}").into_bytes(),
    ] {
        fs::write(&state_path, &bytes).unwrap();
        for message in [apply_plan_error(&path), removal_plan_error(&path)] {
            assert!(
                message.contains(&state_path.display().to_string()),
                "{message}"
            );
            assert!(!message.contains("secret-user-override"), "{message}");
            assert!(!message.contains("oops"), "{message}");
        }
    }
    // A parse error names the position only: line and column.
    fs::write(&state_path, b"{\n  \"version\": }\n").unwrap();
    let message = apply_plan_error(&path);
    assert!(
        message.contains("line 2") && message.contains("column"),
        "{message}"
    );
}

#[test]
fn corrupt_sidecar_does_not_lose_the_users_override() {
    let (_dir, path, state_path) = applied_with_user_override();
    let valid_sidecar = fs::read(&state_path).unwrap();
    let settings_before = fs::read(&path).unwrap();

    fs::write(&state_path, b"{ not json").unwrap();
    apply_plan_error(&path);
    assert_eq!(
        fs::read(&path).unwrap(),
        settings_before,
        "settings.json unchanged"
    );
    assert_eq!(fs::read(&state_path).unwrap(), b"{ not json");

    // A valid sidecar restored, the removal brings the user's value back.
    fs::write(&state_path, &valid_sidecar).unwrap();
    let changes = apply_removal(&path, None, None);
    changes.apply().unwrap();
    assert_eq!(read(&path)[OVERRIDE_KEY], SECRET);
}

#[cfg(unix)]
#[test]
fn identical_sidecar_with_looser_mode_is_rewritten_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let state_path = super::super::json_merge::state_path(&path);
    let sidecar_before = fs::read(&state_path).unwrap();
    fs::set_permissions(&state_path, fs::Permissions::from_mode(0o664)).unwrap();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    assert_eq!(fs::read(&state_path).unwrap(), sidecar_before);
    assert_eq!(
        fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
        0o600,
        "an identical sidecar looser than 0600 must be rewritten at 0600"
    );
}

#[test]
fn sidecar_less_reapply_then_removal_clears_the_override_and_both_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({ "editor.fontSize": 14 }));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let state_path = super::super::json_merge::state_path(&path);
    fs::remove_file(&state_path).unwrap();

    // Re-apply without a sidecar: the existing override, equal to our
    // own URL, must not become "the user's value" to restore later.
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();

    let after = read(&path);
    assert_eq!(after["editor.fontSize"], 14);
    assert!(after.get(OVERRIDE_KEY).is_none(), "{after}");
    assert!(after.get(IGNORED_SETTINGS_KEY).is_none(), "{after}");
}

#[test]
fn sidecar_less_removal_by_own_url_on_a_commented_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let own_url = override_url(listen_addr(), PAIRING);
    let contents = format!(
        "{{\n  // kept\n  \"editor.fontSize\": 14,\n  \"{OVERRIDE_KEY}\": \"{own_url}\",\n  \"{IGNORED_SETTINGS_KEY}\": [\"{OVERRIDE_KEY}\", \"{CAPI_ALIAS_KEY}\"]\n}}\n"
    );
    fs::write(&path, &contents).unwrap();

    apply_removal(&path, Some((listen_addr(), PAIRING)), None)
        .apply()
        .unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("// kept"), "{written}");
    let document = parse_jsonc(&written);
    assert_eq!(document["editor.fontSize"], 14);
    assert!(document.get(OVERRIDE_KEY).is_none(), "{document}");
    assert!(document.get(IGNORED_SETTINGS_KEY).is_none(), "{document}");
}

#[cfg(unix)]
#[test]
fn a_created_parent_directory_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/profile/settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "a directory the daemon creates to hold settings.json must be owner-only"
    );
}

#[test]
fn redaction_keeps_the_pairing_out_of_the_sidecar_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let state_path = super::super::json_merge::state_path(&path);
    let sidecar = fs::read_to_string(&state_path).unwrap();
    assert!(
        !sidecar.contains(PAIRING),
        "the sidecar must not carry the pairing value: {sidecar}"
    );
}

#[test]
fn an_earlier_daemon_url_is_ours_not_the_users_value() {
    let stale = "http://127.0.0.1:18099/vscode-copilot-capi/OLD-PAIRING";
    let fixture = format!("{{\n  \"{OVERRIDE_KEY}\": \"{stale}\"\n}}\n");
    let (edited, state) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    assert_eq!(
        state.override_before, None,
        "a loopback CAPI URL is never the user's own"
    );
    assert!(
        !serde_json::to_string(&state)
            .unwrap()
            .contains("OLD-PAIRING")
    );
    assert_eq!(parse_jsonc(&edited)[OVERRIDE_KEY], MANAGED_URL);
    // Sidecar-less removal takes a stale one out as well.
    assert!(matches!(
        remove_settings(&fixture, None, Some(MANAGED_URL)).unwrap(),
        Removal::Write(remaining) if parse_jsonc(&remaining).get(OVERRIDE_KEY).is_none()
    ));
    // A user's own non-loopback URL stays theirs.
    let users = format!(
        "{{\n  \"{OVERRIDE_KEY}\": \"https://capi.example.invalid/vscode-copilot-capi/x\"\n}}\n"
    );
    let (_, state) = edit_settings(Some(&users), MANAGED_URL, None).unwrap();
    assert!(state.override_before.is_some());
}

#[test]
fn a_byte_order_mark_is_kept_and_edited_around() {
    let fixture = "\u{feff}{\n  \"a\": 1\n}\n";
    let (edited, state) = edit_settings(Some(fixture), MANAGED_URL, None).unwrap();
    assert!(edited.starts_with('\u{feff}'), "{edited:?}");
    assert_eq!(
        parse_jsonc(edited.trim_start_matches('\u{feff}'))[OVERRIDE_KEY],
        MANAGED_URL
    );
    assert_eq!(
        remove_settings(&edited, Some(&state), None).unwrap(),
        Removal::Write(fixture.to_owned())
    );
}

#[test]
fn a_list_or_file_recreated_by_a_reapply_goes_again_on_removal() {
    // The user had an ignore list and deleted it after the first apply; the
    // re-apply recreates it, so removal takes it out rather than leaving [].
    let fixture = format!("{{\n  \"a\": 1,\n  \"{IGNORED_SETTINGS_KEY}\": [\"x\"]\n}}\n");
    let (_, first) = edit_settings(Some(&fixture), MANAGED_URL, None).unwrap();
    let (edited, state) =
        edit_settings(Some("{\n  \"a\": 1\n}\n"), MANAGED_URL, Some(&first)).unwrap();
    assert!(state.ignored_created);
    let Removal::Write(remaining) = remove_settings(&edited, Some(&state), None).unwrap() else {
        panic!("user key a must remain");
    };
    assert!(
        parse_jsonc(&remaining).get(IGNORED_SETTINGS_KEY).is_none(),
        "{remaining}"
    );
    // The user deleted the whole file; the re-apply recreated it.
    let (recreated, state) = edit_settings(None, MANAGED_URL, Some(&first)).unwrap();
    assert!(state.created);
    assert_eq!(
        remove_settings(&recreated, Some(&state), None).unwrap(),
        Removal::Delete
    );
}

#[test]
fn a_duplicated_ignore_list_is_a_conflict() {
    let text = format!("{{ \"{IGNORED_SETTINGS_KEY}\": [], \"{IGNORED_SETTINGS_KEY}\": [] }}");
    assert_eq!(
        edit_settings(Some(&text), MANAGED_URL, None),
        Err(SettingsConflict::DuplicateKey)
    );
}

#[test]
fn removal_records_unchanged_only_when_something_of_ours_was_there() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({ "editor.fontSize": 14 }));
    // No program, no sidecar: nothing of ours, nothing recorded.
    let plan_without = apply_removal(&path, Some((listen_addr(), PAIRING)), None);
    assert!(
        plan_without.render().contains("0 changes, 0 unchanged"),
        "{}",
        plan_without.render()
    );
    // A sidecar and a deleted file: recorded as unchanged, sidecar removed.
    apply_managed(
        &path,
        listen_addr(),
        PAIRING,
        &github_models_config(),
        &gateway_with_proxy(),
    );
    fs::remove_file(&path).unwrap();
    let plan_absent = apply_removal(&path, Some((listen_addr(), PAIRING)), None);
    assert!(
        plan_absent.render().contains("1 unchanged"),
        "{}",
        plan_absent.render()
    );
    plan_absent.apply().unwrap();
    assert!(!super::super::json_merge::state_path(&path).exists());
}

#[cfg(unix)]
#[test]
fn identical_sidecar_within_0600_is_not_rewritten() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    let state_path = super::super::json_merge::state_path(&path);
    let inode = fs::metadata(&state_path).unwrap().ino();

    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    assert_eq!(fs::metadata(&state_path).unwrap().ino(), inode);
}

#[test]
fn a_sidecar_with_a_byte_order_mark_is_read() {
    let (_dir, path, state_path) = applied_with_user_override();
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend(fs::read(&state_path).unwrap());
    fs::write(&state_path, &bytes).unwrap();

    apply_removal(&path, None, None).apply().unwrap();

    assert_eq!(read(&path)[OVERRIDE_KEY], SECRET);
}

// --- failClosed in place, failOpen reason -----------------------------------

#[test]
fn fail_closed_first_apply_reports_not_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let config = github_models_config();
    let gateway = gateway_with_proxy();

    let changes = apply_removal(&path, None, Some((&config, Some(&gateway))));
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_NOT_IN_PLACE_REASON)
    );
    changes.apply().unwrap();
    assert!(
        !path.exists(),
        "nothing is written when nothing is in place"
    );
    assert!(!json_merge::state_path(&path).exists());
}

#[test]
fn fail_open_reports_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let mut gateway = gateway_with_proxy();
    gateway.when_proxy_unavailable = ProxyUnavailable::FailOpen;
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);

    let changes = apply_removal(&path, None, Some((&config, Some(&gateway))));
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_OPEN_REASON)
    );
}

#[test]
fn fail_closed_after_the_user_removed_the_override_reports_not_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let config = github_models_config();
    let gateway = gateway_with_proxy();
    apply_managed(&path, listen_addr(), PAIRING, &config, &gateway);
    assert!(read(&path).get(OVERRIDE_KEY).is_some());

    // The user deletes the override, keeping their own keys.
    write_user_settings(&path, &json!({"editor.fontSize": 14}));
    let before = fs::read(&path).unwrap();

    let changes = apply_removal(&path, None, Some((&config, Some(&gateway))));
    assert_eq!(
        changes.inactive_reason().as_deref(),
        Some(crate::reconcile::FAIL_CLOSED_NOT_IN_PLACE_REASON)
    );
    changes.apply().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before, "nothing is written");
}
