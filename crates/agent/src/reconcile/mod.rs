//! Shared reconciliation orchestration, plan application, and dry-run reporting.
mod plan;
pub use plan::ReconcilePlan;

use crate::provider::{
    Provider, ReconcileContext, claude_code::ClaudeCode, claude_desktop::ClaudeDesktop,
    codex::Codex, cursor::Cursor, grok::Grok, ollama::Ollama, opencode::OpenCode, vscode::VsCode,
};
use agentdesktop_core::{config::DaemonConfig, model::Discovery};
use serde_json::Value;
use similar::TextDiff;
use std::{
    cell::RefCell,
    fmt::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

// Preserve callers of the existing default-path helpers.
pub use crate::provider::{
    claude_code::default_claude_code_managed_settings_dir,
    claude_desktop::{
        default_claude_desktop_credential_helper_path, default_claude_desktop_managed_settings_path,
    },
    codex::default_codex_managed_config_path,
    opencode::{default_open_code_managed_config_path, default_open_code_plugin_path},
    vscode::default_vscode_settings_path,
};

#[derive(Clone)]
pub struct Reconciler {
    context: ReconcileContext,
    providers: Arc<Vec<Box<dyn Provider>>>,
}

impl Reconciler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        merge_user_settings: bool,
        claude_code_settings_path: PathBuf,
        claude_desktop_managed_settings_path: PathBuf,
        claude_desktop_credential_helper_path: PathBuf,
        codex_managed_config_path: PathBuf,
        open_code_managed_config_path: PathBuf,
        open_code_plugin_path: PathBuf,
        vscode_settings_path: PathBuf,
        credential_helper: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            context: ReconcileContext {
                merge_user_settings,
                credential_helper,
                socket,
            },
            providers: Arc::new(vec![
                Box::new(ClaudeCode {
                    settings_path: claude_code_settings_path,
                }),
                Box::new(ClaudeDesktop {
                    managed_settings_path: claude_desktop_managed_settings_path,
                    credential_helper_path: claude_desktop_credential_helper_path,
                }),
                Box::new(Codex {
                    managed_config_path: codex_managed_config_path,
                }),
                Box::new(OpenCode {
                    managed_config_path: open_code_managed_config_path,
                    plugin_path: open_code_plugin_path,
                }),
                Box::new(VsCode {
                    settings_path: vscode_settings_path,
                }),
                Box::new(Cursor),
                Box::new(Grok),
                Box::new(Ollama),
            ]),
        }
    }

    /// Plan every provider, including cleanup for disabled providers, before
    /// applying any writes. A later provider's failure leaves all files intact.
    pub fn plan(&self, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let mut plan = ReconcilePlan::default();
        for provider in self.providers.iter() {
            plan.append(provider.plan(&self.context, config)?)?;
        }
        Ok(plan)
    }

    pub async fn discover(&self) -> Discovery {
        let mut discovery = Discovery {
            agents: Vec::new(),
            model_runtimes: Vec::new(),
        };
        for provider in self.providers.iter() {
            let found = provider.discover().await;
            discovery.agents.extend(found.agents);
            discovery.model_runtimes.extend(found.model_runtimes);
        }
        discovery
    }

    pub fn apply(&self, config: &DaemonConfig) -> anyhow::Result<()> {
        self.plan(config)?.apply()
    }

    pub fn dry_run(&self, config: &DaemonConfig) -> anyhow::Result<()> {
        print!("{}", self.plan(config)?.render());
        Ok(())
    }
}

#[derive(Default)]
struct DryRunReport {
    changes: RefCell<Vec<DryRunChange>>,
}

struct DryRunChange {
    display_name: String,
    description: String,
    action: String,
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

impl DryRunReport {
    fn record(
        &self,
        display_name: &str,
        description: &str,
        action: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.changes.borrow_mut().push(DryRunChange {
            display_name: display_name.to_owned(),
            description: description.to_owned(),
            action: action.to_owned(),
            path: path.to_owned(),
            before: before.map(|value| String::from_utf8_lossy(value).into_owned()),
            after: after.map(|value| String::from_utf8_lossy(value).into_owned()),
        });
    }

    fn render(&self) -> String {
        let changes = self.changes.borrow();
        let changed = changes
            .iter()
            .filter(|change| change.action != "unchanged" && change.action != "conflict")
            .count();
        let unchanged = changes
            .iter()
            .filter(|change| change.action == "unchanged")
            .count();
        let conflicts = changes
            .iter()
            .filter(|change| change.action == "conflict")
            .count();
        let mut output = String::from("Dry run — no files will be changed\n");

        for change in changes.iter().filter(|change| change.action != "unchanged") {
            let _ = write!(
                output,
                "\n{}  {} {}\n        {}\n",
                change.action.to_uppercase(),
                change.display_name,
                change.description,
                change.path.display()
            );
            if change.before.is_some() || change.after.is_some() {
                let (before, after) = normalized_diff(
                    change.before.as_deref().unwrap_or(""),
                    change.after.as_deref().unwrap_or(""),
                );
                let diff = TextDiff::from_lines(&before, &after)
                    .unified_diff()
                    .context_radius(3)
                    .header("current", "proposed")
                    .to_string();
                if !diff.is_empty() {
                    let _ = write!(output, "{diff}");
                }
            }
        }

        let noun = if changed == 1 { "change" } else { "changes" };
        let _ = write!(output, "\nSummary: {changed} {noun}, {unchanged} unchanged");
        if conflicts > 0 {
            let _ = write!(output, ", {conflicts} conflicts");
        }
        output.push('\n');
        output
    }
}

fn normalized_diff(before: &str, after: &str) -> (String, String) {
    match (
        serde_json::from_str::<Value>(before),
        serde_json::from_str::<Value>(after),
    ) {
        (Ok(before), Ok(after)) => (
            format!(
                "{}\n",
                serde_json::to_string_pretty(&before).expect("JSON value serializes")
            ),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&after).expect("JSON value serializes")
            ),
        ),
        _ => (before.to_owned(), after.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use agentdesktop_core::config::parse_daemon;

    use super::{DryRunReport, Reconciler};

    #[test]
    fn dry_run_report_shows_changes_and_hides_unchanged_files() {
        let report = DryRunReport::default();
        report.record(
            "Claude Code",
            "settings",
            "update",
            Path::new("/home/user/.claude/settings.json"),
            Some(br#"{"keep":true}"#),
            Some(br#"{"managed":true,"keep":true}"#),
        );
        report.record(
            "Codex",
            "configuration",
            "unchanged",
            Path::new("/home/user/.codex/config.toml"),
            None,
            None,
        );

        let rendered = report.render();
        assert!(rendered.contains("UPDATE  Claude Code settings"));
        assert!(rendered.contains("+  \"managed\": true"));
        assert!(!rendered.contains("Codex configuration"));
        assert!(rendered.contains("Summary: 1 change, 1 unchanged"));
    }

    #[test]
    fn user_mode_rejects_claude_desktop_before_writing_other_settings() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-user-desktop-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  claudeDesktop: {}
"#,
        )
        .expect("valid configuration");
        let reconciler = Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("vscode/settings.json"),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        let error = reconciler.apply(&config).expect_err("user mode must fail");

        assert!(
            error
                .to_string()
                .contains("/etc/claude-desktop/managed-settings.json")
        );
        assert!(!root.exists(), "preflight failure must not write any files");
    }

    #[test]
    fn dry_run_plans_create_update_and_remove_without_changing_files() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-dry-run-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  claudeDesktop: {}
  codex: {}
  openCode: {}
"#,
        )
        .expect("valid configuration");
        fs::create_dir_all(root.join("codex")).unwrap();
        fs::create_dir_all(root.join("claude")).unwrap();
        fs::create_dir_all(root.join("opencode")).unwrap();
        let user_claude = br#"{"theme":"dark"}\n"#;
        let old_codex =
            b"# Managed by Agentdesktop. Manual changes will be replaced.\nmodel = \"old\"\n";
        let old_plugin =
            b"// Managed by Agentdesktop. Manual changes will be replaced.\nold plugin\n";
        fs::write(root.join("claude/settings.json"), user_claude).unwrap();
        fs::write(root.join("codex/config.toml"), old_codex).unwrap();
        fs::write(root.join("opencode/plugin.js"), old_plugin).unwrap();
        let reconciler = Reconciler::new(
            false,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("vscode/settings.json"),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        reconciler.dry_run(&config).expect("dry run succeeds");

        assert_eq!(
            fs::read(root.join("claude/settings.json")).unwrap(),
            user_claude
        );
        assert!(!root.join("claude-desktop/settings.json").exists());
        assert!(!root.join("opencode/config.json").exists());
        assert_eq!(fs::read(root.join("codex/config.toml")).unwrap(), old_codex);
        assert_eq!(
            fs::read(root.join("opencode/plugin.js")).unwrap(),
            old_plugin
        );
        fs::remove_dir_all(root).unwrap();
    }

    struct Fixture {
        root: PathBuf,
        reconciler: Reconciler,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "agentdesktop-provider-plan-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let reconciler = Reconciler::new(
                false,
                root.join("claude/settings.json"),
                root.join("desktop/settings.json"),
                root.join("desktop/helper"),
                root.join("codex/config.toml"),
                root.join("opencode/config.json"),
                root.join("opencode/plugin.js"),
                root.join("vscode/settings.json"),
                root.join("bin/agentdesktop"),
                root.join("agentdesktop.sock"),
            );
            Self { root, reconciler }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn providers_plan_apply_repeat_and_remove_managed_files() {
        let fixture = Fixture::new();
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code, claude-desktop, codex, opencode]
programs:
  claudeCode: {}
  claudeDesktop: {}
  codex: {}
  openCode:
    model: company-model
    models:
      company-model: {}
"#,
        )
        .unwrap();
        let plan = fixture.reconciler.plan(&config).unwrap();
        assert!(
            !fixture.root.exists(),
            "planning must not create directories or sidecars"
        );
        assert!(!plan.has_conflicts());
        plan.apply().unwrap();

        let paths = [
            "claude/settings.json",
            "claude/.settings.json.owner",
            "desktop/settings.json",
            "desktop/.settings.json.owner",
            "desktop/helper",
            "desktop/.helper.owner",
            "codex/config.toml",
            "opencode/config.json",
            "opencode/plugin.js",
        ];
        let contents: Vec<_> = paths
            .iter()
            .map(|path| fs::read(fixture.root.join(path)).unwrap())
            .collect();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.root.join("desktop/helper"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        let repeated = fixture.reconciler.plan(&config).unwrap();
        assert!(repeated.render().contains("Summary: 0 changes"));
        repeated.apply().unwrap();
        for (path, expected) in paths.iter().zip(contents) {
            assert_eq!(fs::read(fixture.root.join(path)).unwrap(), expected);
        }

        let disabled = parse_daemon("programs: {}").unwrap();
        let cleanup = fixture.reconciler.plan(&disabled).unwrap();
        assert!(paths.iter().all(|path| fixture.root.join(path).exists()));
        cleanup.apply().unwrap();
        assert!(paths.iter().all(|path| !fixture.root.join(path).exists()));
    }

    #[test]
    fn later_provider_conflict_prevents_all_writes() {
        let fixture = Fixture::new();
        let path = fixture.root.join("codex/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user_config = b"model = \"personal\"\n";
        fs::write(&path, user_config).unwrap();
        let config = parse_daemon("programs:\n  claudeCode: {}\n  codex: {}").unwrap();
        let plan = fixture.reconciler.plan(&config).unwrap();
        assert!(plan.has_conflicts());
        assert!(plan.render().contains("CONFLICT  Codex"));
        assert!(plan.apply().is_err());
        assert!(!fixture.root.join("claude").exists());
        assert_eq!(fs::read(path).unwrap(), user_config);
    }

    #[test]
    fn changed_settings_or_ownership_reject_plan_before_any_writes() {
        for changed in ["codex/config.toml", "claude/.settings.json.owner"] {
            let fixture = Fixture::new();
            let original = parse_daemon("programs:\n  claudeCode: {}\n  codex: {}").unwrap();
            fixture.reconciler.apply(&original).unwrap();
            let settings = fixture.root.join("claude/settings.json");
            let before = fs::read(&settings).unwrap();
            let update = parse_daemon(
                "programs:\n  claudeCode:\n    env:\n      COMPANY: updated\n  codex: {}",
            )
            .unwrap();
            let plan = fixture.reconciler.plan(&update).unwrap();
            let changed_path = fixture.root.join(changed);
            fs::write(&changed_path, b"externally changed").unwrap();
            let error = plan.apply().unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("changed since reconciliation was planned")
            );
            assert_eq!(fs::read(settings).unwrap(), before);
            assert_eq!(fs::read(changed_path).unwrap(), b"externally changed");
        }
    }

    #[test]
    fn providers_cannot_plan_writes_to_the_same_path() {
        let mut fixture = Fixture::new();
        let path = fixture.root.join("claude/settings.json");
        fixture.reconciler.providers = std::sync::Arc::new(vec![
            Box::new(super::ClaudeCode {
                settings_path: path.clone(),
            }),
            Box::new(super::Codex {
                managed_config_path: path,
            }),
        ]);
        let config = parse_daemon("programs:\n  claudeCode: {}\n  codex: {}").unwrap();
        let error = fixture.reconciler.apply(&config).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple providers plan to modify")
        );
        assert!(!fixture.root.exists());
    }
}
