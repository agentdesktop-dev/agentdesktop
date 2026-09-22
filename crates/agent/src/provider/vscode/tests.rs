use std::{fs, path::PathBuf};

use agentdesktop_core::config::{DaemonConfig, parse_daemon};
use serde_json::Value;

use super::VsCode;
use crate::{
    provider::{Provider, ReconcileContext, json_merge},
    reconcile::ReconcilePlan,
};

struct Fixture {
    root: tempfile::TempDir,
    provider: VsCode,
    context: ReconcileContext,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let provider = VsCode {
            settings_path: root.path().join("Code/User/settings.json"),
        };
        let context = ReconcileContext {
            merge_user_settings: true,
            credential_helper: root.path().join("agentdesktop"),
            socket: root.path().join("agentdesktop.sock"),
        };
        Self {
            root,
            provider,
            context,
        }
    }

    fn state_path(&self) -> PathBuf {
        json_merge::state_path(&self.provider.settings_path)
    }

    fn write_settings(&self, contents: impl AsRef<[u8]>) {
        fs::create_dir_all(self.provider.settings_path.parent().unwrap()).unwrap();
        fs::write(&self.provider.settings_path, contents).unwrap();
    }

    fn settings(&self) -> Vec<u8> {
        fs::read(&self.provider.settings_path).unwrap()
    }

    fn document(&self) -> Value {
        json5::from_str(std::str::from_utf8(&self.settings()).unwrap()).unwrap()
    }

    fn plan(&self, config: &DaemonConfig) -> ReconcilePlan {
        self.provider
            .plan(&self.context, config)
            .expect("plan VS Code settings")
    }
}

fn config() -> DaemonConfig {
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

fn disabled() -> DaemonConfig {
    parse_daemon("programs:\n  vscode:\n    useLlmGateway: false\n").unwrap()
}

#[test]
fn preserves_comments_and_restores_the_previous_proxy() {
    let fixture = Fixture::new();
    let original = br#"{
    // Keep this comment.
    "editor.fontSize": 15,
    "github.copilot.advanced": {
        "debug.overrideProxyUrl": "https://previous.example.com/v1", // Keep proxy comment.
        "authProvider": "github",
    },
}
"#;
    fixture.write_settings(original);

    let plan = fixture.plan(&config());
    assert!(!plan.has_conflicts());
    assert!(plan.render().contains("UPDATE  VS Code settings"));
    assert_eq!(fixture.settings(), original);
    assert!(!fixture.state_path().exists());
    plan.apply().unwrap();

    let managed = fixture.settings();
    let text = std::str::from_utf8(&managed).unwrap();
    assert!(text.contains("// Keep this comment."));
    assert!(text.contains("// Keep proxy comment."));
    let document = fixture.document();
    assert_eq!(document["editor.fontSize"], 15);
    assert_eq!(
        document["github.copilot.advanced"]["authProvider"],
        "github"
    );
    assert_eq!(
        document["github.copilot.advanced"]["debug.overrideProxyUrl"],
        "http://127.0.0.1:4002/v1"
    );
    assert_eq!(
        document["github.copilot.advanced"]["debug.overrideCapiUrl"],
        "http://127.0.0.1:4002"
    );

    let state = fs::read(fixture.state_path()).unwrap();
    let repeated = fixture.plan(&config());
    assert!(repeated.render().contains("Summary: 0 changes"));
    assert_eq!(fixture.settings(), managed);
    assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
    repeated.apply().unwrap();
    assert_eq!(fixture.settings(), managed);
    assert_eq!(fs::read(fixture.state_path()).unwrap(), state);

    let cleanup = fixture.plan(&DaemonConfig::default());
    assert_eq!(fixture.settings(), managed);
    assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
    cleanup.apply().unwrap();
    let restored = String::from_utf8(fixture.settings()).unwrap();
    assert!(restored.contains("// Keep this comment."));
    assert!(restored.contains("// Keep proxy comment."));
    assert!(restored.contains("https://previous.example.com/v1"));
    assert!(!restored.contains("debug.overrideCapiUrl"));
    assert!(!fixture.state_path().exists());
}

#[test]
fn removes_a_settings_file_created_only_for_the_proxy() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&config());
    assert!(!fixture.root.path().join("Code").exists());
    assert!(plan.render().contains("CREATE  VS Code settings"));
    plan.apply().unwrap();
    let managed = fixture.settings();
    let state = fs::read(fixture.state_path()).unwrap();
    let document = fixture.document();
    assert!(document["github.copilot.advanced"]["debug.overrideProxyUrl"].is_string());
    assert!(document["github.copilot.advanced"]["debug.overrideCapiUrl"].is_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&fixture.provider.settings_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(fixture.state_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let cleanup = fixture.plan(&disabled());
    assert!(cleanup.render().contains("REMOVE  VS Code settings"));
    assert_eq!(fixture.settings(), managed);
    assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
    cleanup.apply().unwrap();
    assert!(!fixture.provider.settings_path.exists());
    assert!(!fixture.state_path().exists());
}

#[test]
fn upgrades_legacy_state_and_restores_existing_capi_url() {
    let fixture = Fixture::new();
    fixture.write_settings(
        br#"{
    "github.copilot.advanced": {
        "debug.overrideProxyUrl": "http://127.0.0.1:4002/v1",
        "debug.overrideCapiUrl": "https://previous-capi.example.com"
    }
}
"#,
    );
    let legacy = br#"{
  "created": false,
  "advanced_created": false,
  "before": "https://previous-proxy.example.com/v1",
  "after": "http://127.0.0.1:4002/v1"
}
"#;
    fs::write(fixture.state_path(), legacy).unwrap();
    let before = fixture.settings();
    let upgrade = fixture.plan(&config());
    assert_eq!(fixture.settings(), before);
    assert_eq!(fs::read(fixture.state_path()).unwrap(), legacy);
    upgrade.apply().unwrap();
    assert_eq!(
        fixture.document()["github.copilot.advanced"]["debug.overrideCapiUrl"],
        "http://127.0.0.1:4002"
    );

    fixture.plan(&DaemonConfig::default()).apply().unwrap();
    let document = fixture.document();
    assert_eq!(
        document["github.copilot.advanced"]["debug.overrideProxyUrl"],
        "https://previous-proxy.example.com/v1"
    );
    assert_eq!(
        document["github.copilot.advanced"]["debug.overrideCapiUrl"],
        "https://previous-capi.example.com"
    );
    assert!(!fixture.state_path().exists());
}

#[test]
fn updates_endpoints_without_losing_original_values() {
    let fixture = Fixture::new();
    let original = br#"{
  "github.copilot.advanced": {
    "debug.overrideProxyUrl": "https://personal.example.com/v1",
    "debug.overrideCapiUrl": "https://personal-capi.example.com"
  }
}
"#;
    fixture.write_settings(original);
    fixture.plan(&config()).apply().unwrap();
    let mut update = config();
    update.programs.vscode.as_mut().unwrap().copilot_proxy_url =
        Some("https://gateway.example.com/copilot/v1/".parse().unwrap());
    fixture.plan(&update).apply().unwrap();
    assert_eq!(
        fixture.document()["github.copilot.advanced"]["debug.overrideCapiUrl"],
        "https://gateway.example.com/copilot"
    );
    fixture.plan(&disabled()).apply().unwrap();
    assert_eq!(
        fixture.document(),
        serde_json::from_slice::<Value>(original).unwrap()
    );
}

#[test]
fn disabling_preserves_user_changes_and_unmanaged_comments() {
    let fixture = Fixture::new();
    fixture.plan(&config()).apply().unwrap();
    fixture.write_settings(
        br#"{
  // User preference added after reconciliation.
  "editor.fontSize": 18,
  "github.copilot.advanced": {
    "debug.overrideProxyUrl": "https://user-edited.example.com/v1",
    "debug.overrideCapiUrl": "http://127.0.0.1:4002",
    "authProvider": "github"
  }
}
"#,
    );
    let before = fixture.settings();
    let cleanup = fixture.plan(&disabled());
    assert_eq!(fixture.settings(), before);
    cleanup.apply().unwrap();
    let document = fixture.document();
    assert_eq!(document["editor.fontSize"], 18);
    assert_eq!(
        document["github.copilot.advanced"]["debug.overrideProxyUrl"],
        "https://user-edited.example.com/v1"
    );
    assert_eq!(
        document["github.copilot.advanced"]["authProvider"],
        "github"
    );
    assert!(
        document["github.copilot.advanced"]
            .get("debug.overrideCapiUrl")
            .is_none()
    );
    assert!(
        String::from_utf8(fixture.settings())
            .unwrap()
            .contains("// User preference")
    );
    assert!(!fixture.state_path().exists());
}

#[test]
fn disabling_keeps_files_and_objects_with_user_comments() {
    for (prefix, object_comment, advanced_comment) in [
        ("// Keep header.\n", "", ""),
        ("", "  // Keep settings comment.\n", ""),
        ("", "", "    // Keep advanced comment.\n"),
    ] {
        let fixture = Fixture::new();
        fixture.plan(&config()).apply().unwrap();
        fixture.write_settings(format!(
            "{prefix}{{\n{object_comment}  \"github.copilot.advanced\": {{\n{advanced_comment}    \"debug.overrideProxyUrl\": \"http://127.0.0.1:4002/v1\",\n    \"debug.overrideCapiUrl\": \"http://127.0.0.1:4002\"\n  }}\n}}\n"
        ));
        fixture.plan(&disabled()).apply().unwrap();
        let restored = String::from_utf8(fixture.settings()).unwrap();
        assert!(restored.contains("// Keep"));
        assert!(!restored.contains("debug.overrideProxyUrl"));
        assert!(!restored.contains("debug.overrideCapiUrl"));
        assert!(!fixture.state_path().exists());
    }
}

#[test]
fn disabling_preserves_comments_next_to_and_inside_managed_properties() {
    let fixture = Fixture::new();
    fixture.plan(&config()).apply().unwrap();
    fixture.write_settings(
        br#"{
  "github.copilot.advanced": {
    /* Keep leading comment. */ "debug.overrideProxyUrl": /* Keep value comment. */ "http://127.0.0.1:4002/v1", // Keep trailing comment.
    "debug.overrideCapiUrl": "http://127.0.0.1:4002"
  }
}
"#,
    );
    let repeated = fixture.plan(&config());
    let before = fixture.settings();
    assert!(repeated.render().contains("Summary: 0 changes"));
    repeated.apply().unwrap();
    assert_eq!(fixture.settings(), before);

    fixture.plan(&disabled()).apply().unwrap();
    let restored = String::from_utf8(fixture.settings()).unwrap();
    for comment in [
        "/* Keep leading comment. */",
        "/* Keep value comment. */",
        "// Keep trailing comment.",
    ] {
        assert!(restored.contains(comment));
    }
    assert_eq!(
        fixture.document()["github.copilot.advanced"],
        serde_json::json!({})
    );
    assert!(!fixture.state_path().exists());
}

#[test]
fn disabling_preserves_unrelated_properties_between_managed_overrides() {
    let fixture = Fixture::new();
    fixture.plan(&config()).apply().unwrap();
    fixture.write_settings(
        br#"{
  "github.copilot.advanced": {
    "debug.overrideProxyUrl": "http://127.0.0.1:4002/v1",
    "authProvider": "github",
    "debug.overrideCapiUrl": "http://127.0.0.1:4002"
  }
}
"#,
    );
    fixture.plan(&disabled()).apply().unwrap();
    assert_eq!(
        fixture.document()["github.copilot.advanced"],
        serde_json::json!({"authProvider": "github"})
    );
}

#[test]
fn invalid_settings_are_plan_conflicts_without_writes() {
    for invalid in [
        &b"{"[..],
        b"\xff",
        b"[]",
        b"null",
        br#"{"github.copilot.advanced":false}"#,
        br#"{"github.copilot.advanced":{"debug.overrideProxyUrl":42}}"#,
        br#"{"github.copilot.advanced":{"debug.overrideCapiUrl":[]}}"#,
    ] {
        let fixture = Fixture::new();
        fixture.write_settings(invalid);
        let plan = fixture.plan(&config());
        assert!(plan.has_conflicts());
        assert!(plan.render().contains("CONFLICT  VS Code settings"));
        assert_eq!(fixture.settings(), invalid);
        assert!(!fixture.state_path().exists());
        assert!(plan.apply().is_err());
        assert_eq!(fixture.settings(), invalid);
        assert!(!fixture.state_path().exists());
    }
}

#[test]
fn invalid_settings_on_disable_keep_the_ownership_record() {
    for invalid in [&b"{"[..], b"\xff", b"[]", b"null", b""] {
        let fixture = Fixture::new();
        fixture.plan(&config()).apply().unwrap();
        let state = fs::read(fixture.state_path()).unwrap();
        fixture.write_settings(invalid);
        let cleanup = fixture.plan(&disabled());
        assert!(cleanup.has_conflicts());
        assert!(cleanup.apply().is_err());
        assert_eq!(fixture.settings(), invalid);
        assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
    }
}

#[test]
fn invalid_ownership_state_conflicts_on_enable_and_disable() {
    for configured in [config(), disabled()] {
        let fixture = Fixture::new();
        fixture.write_settings(b"{}\n");
        fs::write(fixture.state_path(), b"not valid ownership state").unwrap();
        let plan = fixture.plan(&configured);
        assert!(plan.has_conflicts());
        assert!(plan.render().contains("CONFLICT  VS Code merge state"));
        assert!(plan.apply().is_err());
        assert_eq!(fixture.settings(), b"{}\n");
        assert_eq!(
            fs::read(fixture.state_path()).unwrap(),
            b"not valid ownership state"
        );
    }
}

#[test]
fn changed_settings_or_sidecar_reject_application_before_writes() {
    for change_state in [false, true] {
        let fixture = Fixture::new();
        fixture.plan(&config()).apply().unwrap();
        let settings = fixture.settings();
        let state = fs::read(fixture.state_path()).unwrap();
        let cleanup = fixture.plan(&disabled());
        let changed = if change_state {
            fixture.state_path()
        } else {
            fixture.provider.settings_path.clone()
        };
        fs::write(&changed, b"external edit").unwrap();
        let error = cleanup.apply().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed since reconciliation was planned")
        );
        assert_eq!(fs::read(changed).unwrap(), b"external edit");
        if change_state {
            assert_eq!(fixture.settings(), settings);
        } else {
            assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
        }
    }
}

#[test]
fn newly_created_settings_or_sidecar_invalidate_a_creation_plan() {
    for create_state in [false, true] {
        let fixture = Fixture::new();
        let plan = fixture.plan(&config());
        fs::create_dir_all(fixture.provider.settings_path.parent().unwrap()).unwrap();
        let (created, untouched) = if create_state {
            (fixture.state_path(), fixture.provider.settings_path.clone())
        } else {
            (fixture.provider.settings_path.clone(), fixture.state_path())
        };
        fs::write(&created, b"external edit").unwrap();
        assert!(plan.apply().is_err());
        assert_eq!(fs::read(created).unwrap(), b"external edit");
        assert!(!untouched.exists());
    }
}

#[test]
fn missing_settings_cleanup_only_plans_removal_of_the_sidecar() {
    let fixture = Fixture::new();
    fixture.plan(&config()).apply().unwrap();
    fs::remove_file(&fixture.provider.settings_path).unwrap();
    let state = fs::read(fixture.state_path()).unwrap();
    let cleanup = fixture.plan(&disabled());
    assert!(!cleanup.has_conflicts());
    assert_eq!(fs::read(fixture.state_path()).unwrap(), state);
    cleanup.apply().unwrap();
    assert!(!fixture.state_path().exists());
    assert!(!fixture.provider.settings_path.exists());
}

#[test]
fn unowned_settings_are_not_changed_when_disabled() {
    let fixture = Fixture::new();
    fixture.write_settings(b"invalid but unowned settings");
    let plan = fixture.plan(&DaemonConfig::default());
    assert!(!plan.has_conflicts());
    plan.apply().unwrap();
    assert_eq!(fixture.settings(), b"invalid but unowned settings");
    assert!(!fixture.state_path().exists());
}

#[test]
fn system_mode_rejects_even_an_explicitly_disabled_vscode_program() {
    let mut fixture = Fixture::new();
    fixture.context.merge_user_settings = false;
    for configured in [config(), disabled()] {
        let error = fixture
            .provider
            .plan(&fixture.context, &configured)
            .err()
            .expect("VS Code settings require user mode");
        assert!(error.to_string().contains("profile-specific"));
        assert!(!fixture.root.path().join("Code").exists());
    }
}
