//! Shared reconciliation orchestration, plan application, and dry-run reporting.
mod plan;
pub use plan::ReconcilePlan;

use crate::provider::{
    Provider, ReconcileContext, claude_code::ClaudeCode, claude_desktop::ClaudeDesktop,
    codex::Codex, copilot::Copilot, cursor::Cursor, grok::Grok, ollama::Ollama, opencode::OpenCode,
    vscode::VsCode,
};
use agentdesktop_core::{
    config::{DaemonConfig, ProgramsConfig},
    model::Discovery,
};
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
    copilot::default_copilot_providers_path,
    grok::default_grok_managed_config_path,
    opencode::{default_open_code_managed_config_path, default_open_code_plugin_path},
    vscode::{default_vscode_chat_models_path, default_vscode_settings_path},
};

#[derive(Clone)]
pub struct Reconciler {
    context: ReconcileContext,
    providers: Arc<Vec<Box<dyn Provider>>>,
    /// The apply lock and the report of the last apply. Held for planning
    /// and applying, so a push, the startup apply and a reconcile tick never
    /// overlap across clones; never held across an `.await`. `plan`,
    /// `plan_with_report` and `dry_run` take no lock. The report feeds the
    /// tick's log rule.
    last_apply: Arc<std::sync::Mutex<Option<ApplyReport>>>,
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
        grok_managed_config_path: PathBuf,
        copilot_providers_path: Option<PathBuf>,
        vscode_chat_models_path: Option<PathBuf>,
        vscode_settings_path: Option<PathBuf>,
        credential_helper: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            context: ReconcileContext {
                merge_user_settings,
                credential_helper,
                socket,
                llm_proxy: None,
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
                    chat_models_path: vscode_chat_models_path,
                    settings_path: vscode_settings_path,
                }),
                Box::new(Cursor),
                Box::new(Grok {
                    managed_config_path: grok_managed_config_path,
                }),
                Box::new(Copilot {
                    providers_path: copilot_providers_path,
                }),
                Box::new(Ollama),
            ]),
            last_apply: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Attach the bound loopback LLM proxy so reconcilers can point client files at it.
    pub fn with_llm_proxy(mut self, llm_proxy: Option<crate::llm_proxy::LlmProxyContext>) -> Self {
        self.context.llm_proxy = llm_proxy;
        self
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
        self.apply_with_report(config).1
    }

    pub fn dry_run(&self, config: &DaemonConfig) -> anyhow::Result<()> {
        print!("{}", self.plan(config)?.render());
        Ok(())
    }

    /// Plans every provider like [`Reconciler::plan`], but a provider whose
    /// `plan` fails is recorded as failed and the others are still planned,
    /// so each program's outcome is known. Nothing is written here.
    pub fn plan_with_report(&self, config: &DaemonConfig) -> AttributedPlan {
        let configured = configured_programs(&config.programs);
        let mut plan = ReconcilePlan::default();
        let mut entries: Vec<ProgramEntry> = Vec::new();
        let mut first_error = None;
        for provider in self.providers.iter() {
            let program = provider.id();
            let mut entry = ProgramEntry {
                program,
                configured: configured.contains(&program),
                failed: None,
                changes: false,
                conflicts: Vec::new(),
                inactive: None,
                paths: Vec::new(),
                operations: 0..0,
            };
            match provider.plan(&self.context, config) {
                Err(error) => {
                    entry.failed = Some(format!("{error:#}"));
                    first_error.get_or_insert(error);
                }
                Ok(own) => {
                    let summary = own.summary();
                    entry.changes = summary.changes;
                    entry.conflicts = summary.conflicts;
                    entry.inactive = summary.inactive;
                    entry.paths = summary.paths;
                    let start = plan.operation_count();
                    if let Err((error, path)) = plan.append_attributed(own) {
                        // Both programs of the disagreement fail.
                        for other in entries
                            .iter_mut()
                            .filter(|other| other.paths.contains(&path))
                        {
                            other.failed.get_or_insert_with(|| format!("{error:#}"));
                        }
                        entry.failed = Some(format!("{error:#}"));
                        first_error.get_or_insert(error);
                    }
                    entry.operations = start..plan.operation_count();
                }
            }
            entries.push(entry);
        }
        AttributedPlan {
            plan,
            entries,
            first_error,
        }
    }

    /// [`Reconciler::plan_with_report`] followed by [`AttributedPlan::apply`],
    /// under the apply lock.
    pub fn apply_with_report(&self, config: &DaemonConfig) -> (ApplyReport, anyhow::Result<()>) {
        let (_, report, result) = self.apply_after_previous(config);
        (report, result)
    }

    /// Like [`Reconciler::apply_with_report`], also returning the report of
    /// the apply before this one (from any source), read under the same lock.
    pub(crate) fn apply_after_previous(
        &self,
        config: &DaemonConfig,
    ) -> (Option<ApplyReport>, ApplyReport, anyhow::Result<()>) {
        let (previous, report, result) = self
            .apply_read_under_lock(|| Some(Arc::new(config.clone())))
            .expect("a configuration was given");
        (previous, report, result)
    }

    /// Takes the apply lock, then reads the configuration to apply, so a
    /// configuration pushed while this call waited for the lock is the one
    /// applied (a tick never re-applies an older revision after a push).
    /// `None` from `read` applies nothing.
    #[allow(clippy::type_complexity)]
    pub(crate) fn apply_read_under_lock(
        &self,
        read: impl FnOnce() -> Option<Arc<DaemonConfig>>,
    ) -> Option<(Option<ApplyReport>, ApplyReport, anyhow::Result<()>)> {
        let mut last = self
            .last_apply
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = read()?;
        let (report, result) = self.plan_with_report(&config).apply();
        let previous = last.replace(report.clone());
        Some((previous, report, result))
    }
}

/// The reason recorded for a program that uses the gateway while the local
/// LLM proxy is absent.
pub(crate) const PROXY_ABSENT_REASON: &str = "local LLM proxy not available; see llmProxy.error in daemon-info, or set daemon.llmProxy.listen";

/// A program's outcome, by precedence (highest first): Failed > Conflict >
/// Blocked > Inactive > Applied/Removed > Unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// The program is configured and its files were changed by this apply.
    Applied,
    /// The program is configured; nothing needed to change.
    Unchanged,
    /// The program is not configured; its managed content was removed by
    /// this apply.
    Removed,
    /// A managed file held conflicting or invalid existing configuration.
    Conflict,
    /// The program uses the gateway, a gateway is configured, and the
    /// loopback proxy is absent.
    Inactive,
    /// The program had changes, but none of them was written because the
    /// apply was refused or stopped.
    Blocked,
    /// The program's plan failed, it disagreed with another program's plan,
    /// one of its writes failed, or one of its files changed before the
    /// apply.
    Failed,
}

impl ProgramState {
    /// Lowercase name, as logged and stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Removed => "removed",
            Self::Conflict => "conflict",
            Self::Inactive => "inactive",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

/// One program's outcome from an apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramOutcome {
    /// The provider ID, for example `"claude-code"` or `"vscode"`.
    pub program: &'static str,
    pub state: ProgramState,
    pub detail: String,
}

/// Every reported program's outcome from one apply, in provider
/// registration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub programs: Vec<ProgramOutcome>,
}

impl ApplyReport {
    /// One log line per program whose state is not applied or unchanged; a
    /// program that failed, conflicted, was blocked or is inactive is a
    /// warning.
    pub fn log(&self) {
        for outcome in &self.programs {
            // Inactive is a warning too: the program is configured for the
            // gateway but its files are removed. The providers log the cause
            // at debug level, since they plan on every apply.
            let warning = matches!(
                outcome.state,
                ProgramState::Failed
                    | ProgramState::Conflict
                    | ProgramState::Blocked
                    | ProgramState::Inactive
            );
            if matches!(
                outcome.state,
                ProgramState::Applied | ProgramState::Unchanged
            ) {
                continue;
            }
            if warning {
                tracing::warn!(
                    program = outcome.program,
                    state = outcome.state.as_str(),
                    detail = %outcome.detail,
                    "program configuration outcome"
                );
            } else {
                tracing::info!(
                    program = outcome.program,
                    state = outcome.state.as_str(),
                    detail = %outcome.detail,
                    "program configuration outcome"
                );
            }
        }
    }
}

/// Whether `current` (a tick's apply outcome) differs enough from
/// `previous` (the report of the last apply from any source: startup, a
/// push, or an earlier tick) to log, using the same same-kind-of-difference
/// rule as `remote::tick_config_status`: a program moving from `Applied` or
/// `Removed` to `Unchanged` is not itself a difference. `previous: None`
/// (nothing has been applied yet) always logs.
pub(crate) fn should_log(previous: Option<&ApplyReport>, current: &ApplyReport) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    outcomes_differ(
        &outcome_keys(&previous.programs),
        &outcome_keys(&current.programs),
    )
}

/// `(program, state, detail)` of each outcome, for [`outcomes_differ`].
pub(crate) fn outcome_keys(programs: &[ProgramOutcome]) -> Vec<(&str, ProgramState, &str)> {
    programs
        .iter()
        .map(|outcome| (outcome.program, outcome.state, outcome.detail.as_str()))
        .collect()
}

/// Whether two outcome lists differ by program, state or detail. A program
/// that settles from applied or removed to unchanged, or a removed program
/// that is no longer listed, is not a difference: the files are as reported.
pub(crate) fn outcomes_differ(
    previous: &[(&str, ProgramState, &str)],
    current: &[(&str, ProgramState, &str)],
) -> bool {
    let changed = current.iter().any(|&(program, state, detail)| {
        match previous.iter().find(|(before, _, _)| *before == program) {
            None => true,
            Some(&(_, before_state, before_detail)) => {
                let settled = state == ProgramState::Unchanged
                    && matches!(before_state, ProgramState::Applied | ProgramState::Removed);
                !settled && (state != before_state || detail != before_detail)
            }
        }
    });
    let gone = previous.iter().any(|&(program, state, _)| {
        state != ProgramState::Removed && !current.iter().any(|(now, _, _)| *now == program)
    });
    changed || gone
}

/// The longest detail an outcome carries, in bytes.
const DETAIL_LIMIT: usize = 1024;

fn truncate_detail(mut detail: String) -> String {
    if detail.len() > DETAIL_LIMIT {
        let mut end = DETAIL_LIMIT;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    detail
}

struct ProgramEntry {
    program: &'static str,
    configured: bool,
    failed: Option<String>,
    changes: bool,
    conflicts: Vec<String>,
    inactive: Option<String>,
    paths: Vec<PathBuf>,
    operations: std::ops::Range<usize>,
}

impl ProgramEntry {
    /// The outcome when this program's own part did not fail or conflict:
    /// `written` says whether its changes reached the disk.
    fn settled(&self, written: bool, blocked_by: &str) -> Option<(ProgramState, String)> {
        if self.changes && !written {
            return Some((ProgramState::Blocked, format!("not applied: {blocked_by}")));
        }
        if let Some(reason) = &self.inactive {
            return Some((ProgramState::Inactive, reason.clone()));
        }
        match (self.changes, self.configured) {
            (true, true) => Some((ProgramState::Applied, String::new())),
            (true, false) => Some((ProgramState::Removed, String::new())),
            (false, true) => Some((ProgramState::Unchanged, String::new())),
            (false, false) => None,
        }
    }
}

/// A plan with each program's part identified, ready to apply with
/// per-program reporting.
pub struct AttributedPlan {
    plan: ReconcilePlan,
    entries: Vec<ProgramEntry>,
    first_error: Option<anyhow::Error>,
}

impl AttributedPlan {
    /// Applies with the all-or-nothing rules of [`ReconcilePlan::apply`] (a
    /// failed plan or any conflict means nothing is written) and returns the
    /// same first error, alongside each program's outcome.
    pub fn apply(self) -> (ApplyReport, anyhow::Result<()>) {
        let Self {
            plan,
            entries,
            first_error,
        } = self;
        let (result, stop) = if let Some(error) = first_error {
            (Err(error), Stop::BeforeWrites)
        } else if entries.iter().any(|entry| !entry.conflicts.is_empty()) {
            // The plan refuses without writing and returns the conflict error.
            (plan.apply(), Stop::BeforeWrites)
        } else {
            match plan.apply_tracked() {
                Ok(()) => (Ok(()), Stop::Completed),
                Err((error, at)) => {
                    let stop = Stop::Failed {
                        owners: owners_of(&entries, &at),
                        detail: format!("{error:#}"),
                        written: match at {
                            plan::ApplyStop::Operation(index) => index,
                            _ => 0,
                        },
                    };
                    (Err(error), stop)
                }
            }
        };
        let failed_during = |program: &str| match &stop {
            Stop::Failed { owners, .. } => owners.contains(&program),
            _ => false,
        };
        // The first program, in registration order, that stopped the apply.
        let blocked_by = entries
            .iter()
            .find_map(|entry| {
                if entry.failed.is_some() || failed_during(entry.program) {
                    Some(format!("{} failed", entry.program))
                } else if !entry.conflicts.is_empty() {
                    Some(format!("{} conflicted", entry.program))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let programs = entries
            .iter()
            .filter_map(|entry| {
                let (state, detail) = if let Some(detail) = &entry.failed {
                    (ProgramState::Failed, detail.clone())
                } else if let Stop::Failed { detail, .. } = &stop
                    && failed_during(entry.program)
                {
                    (ProgramState::Failed, detail.clone())
                } else if !entry.conflicts.is_empty() {
                    (ProgramState::Conflict, entry.conflicts.join("; "))
                } else {
                    let written = match &stop {
                        Stop::Completed => true,
                        Stop::BeforeWrites => false,
                        Stop::Failed { written, .. } => entry.operations.end <= *written,
                    };
                    entry.settled(written, &blocked_by)?
                };
                Some(outcome(entry.program, state, detail))
            })
            .collect();
        (ApplyReport { programs }, result)
    }
}

/// How an attributed apply ended.
enum Stop {
    /// Every operation was written.
    Completed,
    /// Refused before any write (a failed plan or a conflict).
    BeforeWrites,
    /// Stopped while applying: `owners` failed, the first `written`
    /// operations reached the disk.
    Failed {
        owners: Vec<&'static str>,
        detail: String,
        written: usize,
    },
}

/// The programs responsible for where an apply stopped.
fn owners_of(entries: &[ProgramEntry], at: &plan::ApplyStop) -> Vec<&'static str> {
    entries
        .iter()
        .filter(|entry| match at {
            plan::ApplyStop::Conflict => false,
            plan::ApplyStop::Observed(path) => entry.paths.contains(path),
            plan::ApplyStop::PrivateDir(dir) => {
                entry.paths.iter().any(|path| path.starts_with(dir))
            }
            plan::ApplyStop::Operation(index) => entry.operations.contains(index),
        })
        .map(|entry| entry.program)
        .collect()
}

fn outcome(program: &'static str, state: ProgramState, detail: String) -> ProgramOutcome {
    ProgramOutcome {
        program,
        state,
        detail: truncate_detail(detail),
    }
}

/// The provider IDs of the configured programs. The destructure is
/// exhaustive, so a new program key does not compile until it is mapped.
pub fn configured_programs(programs: &ProgramsConfig) -> Vec<&'static str> {
    let ProgramsConfig {
        claude_code,
        claude_desktop,
        codex,
        open_code,
        grok,
        copilot,
        vscode,
    } = programs;
    [
        (claude_code.is_some(), ClaudeCode::ID),
        (claude_desktop.is_some(), ClaudeDesktop::ID),
        (codex.is_some(), Codex::ID),
        (open_code.is_some(), OpenCode::ID),
        (grok.is_some(), Grok::ID),
        (copilot.is_some(), Copilot::ID),
        (vscode.is_some(), VsCode::ID),
    ]
    .into_iter()
    .filter_map(|(configured, id)| configured.then_some(id))
    .collect()
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

    use super::{ApplyReport, DryRunReport, ProgramOutcome, ProgramState, Reconciler};

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
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            None,
            None,
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        let error = reconciler.apply(&config).expect_err("user mode must fail");

        assert!(
            error
                .to_string()
                .contains("Claude Desktop managed settings")
        );
        assert!(!root.exists(), "preflight failure must not write any files");
    }

    #[test]
    fn user_mode_rejects_grok_before_writing_other_settings() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-user-grok-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  grok: {}
"#,
        )
        .unwrap();
        let reconciler = Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            None,
            None,
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        let error = reconciler.apply(&config).expect_err("user mode must fail");
        assert!(error.to_string().contains("Grok Build"));
        assert!(!root.exists(), "preflight failure must not write any files");
    }

    #[test]
    fn copilot_directory_is_created_owner_only_through_the_reconciler() {
        // The private-directory request must survive `ReconcilePlan::append`,
        // which the daemon always goes through (it was once dropped there).
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-copilot-dir-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let providers = root.join("copilot/.copilot/providers.json");
        let reconciler = Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            Some(providers.clone()),
            None,
            None,
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        )
        .with_llm_proxy(Some(crate::llm_proxy::LlmProxyContext {
            address: "127.0.0.1:18095".parse().unwrap(),
            pairing: std::sync::Arc::from("PAIRING-RECONCILER"),
        }));
        reconciler.apply(&config).expect("apply");
        assert!(providers.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(providers.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "directory created through the reconciler");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn vscode_directory_is_created_owner_only_through_the_reconciler() {
        // Same private-directory request as the Copilot CLI test above, for
        // the VS Code user profile directory.
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-vscode-dir-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
        )
        .unwrap();
        let chat_models = root.join("vscode/User/chatLanguageModels.json");
        let reconciler = Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            Some(chat_models.clone()),
            Some(chat_models.with_file_name("settings.json")),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        )
        .with_llm_proxy(Some(crate::llm_proxy::LlmProxyContext {
            address: "127.0.0.1:18095".parse().unwrap(),
            pairing: std::sync::Arc::from("PAIRING-RECONCILER"),
        }));
        reconciler.apply(&config).expect("apply");
        assert!(chat_models.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(chat_models.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "directory created through the reconciler");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn system_mode_rejects_copilot_before_writing_other_settings() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-system-copilot-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  copilot: {}
"#,
        )
        .unwrap();
        // The Copilot CLI program is the inverse of Grok: it manages a file in
        // the user's home, so a system daemon (no providers path) rejects it
        // before any provider writes.
        let reconciler = Reconciler::new(
            false,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            None,
            None,
            None,
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        let error = reconciler
            .apply(&config)
            .expect_err("system mode must fail");
        assert!(error.to_string().contains("GitHub Copilot CLI"), "{error}");
        assert!(error.to_string().contains("--user"), "{error}");
        assert!(!root.exists(), "preflight failure must not write any files");
    }

    #[cfg(windows)]
    #[test]
    fn windows_reconciles_system_grok_managed_config() {
        let fixture = Fixture::new();
        let config = parse_daemon("programs:\n  claudeCode: {}\n  grok: {}\n").unwrap();
        fixture.reconciler.apply(&config).unwrap();
        assert!(fixture.root.join("grok/managed_config.toml").exists());
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
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            None,
            None,
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
                root.join("grok/managed_config.toml"),
                Some(root.join("copilot/providers.json")),
                None,
                None,
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
    allowedClientIds: [claude-code, claude-desktop, codex, opencode, grok]
programs:
  claudeCode: {}
  claudeDesktop: {}
  codex: {}
  openCode:
    model: company-model
    models:
      company-model: {}
  grok:
    model: grok-4.6
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
            "grok/managed_config.toml",
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

    // --- Per-program configuration status -----------------------------------

    fn new_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-status-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    /// A user-mode reconciler with every provider's paths set (including
    /// Copilot CLI and VS Code), rooted at `root`.
    fn full_reconciler(root: &Path) -> Reconciler {
        Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            Some(root.join("vscode/User/chatLanguageModels.json")),
            Some(root.join("vscode/User/settings.json")),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        )
    }

    fn proxy_context(pairing: &str) -> crate::llm_proxy::LlmProxyContext {
        crate::llm_proxy::LlmProxyContext {
            address: "127.0.0.1:18095".parse().unwrap(),
            pairing: std::sync::Arc::from(pairing),
        }
    }

    fn program_outcome<'a>(report: &'a ApplyReport, program: &str) -> Option<&'a ProgramOutcome> {
        report
            .programs
            .iter()
            .find(|outcome| outcome.program == program)
    }

    /// A provider whose `plan` always fails with a caller-supplied message,
    /// for scenarios only the message shape matters for (detail truncation).
    struct FailWithMessage {
        message: String,
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for FailWithMessage {
        fn id(&self) -> &'static str {
            "fail-with-message"
        }

        async fn discover(&self) -> agentdesktop_core::model::Discovery {
            agentdesktop_core::model::Discovery {
                agents: Vec::new(),
                model_runtimes: Vec::new(),
            }
        }

        fn plan(
            &self,
            _ctx: &crate::provider::ReconcileContext,
            _config: &agentdesktop_core::config::DaemonConfig,
        ) -> anyhow::Result<super::ReconcilePlan> {
            anyhow::bail!("{}", self.message)
        }
    }

    #[test]
    fn applied_then_unchanged_with_report() {
        let root = new_root("applied-unchanged");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  claudeCode: {}
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-1")));

        let (report, result) = reconciler.apply_with_report(&config);
        result.expect("first apply succeeds");
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );

        let (report, result) = reconciler.apply_with_report(&config);
        result.expect("second apply succeeds");
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Unchanged)
        );
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Unchanged)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn removed_then_absent_on_the_next_apply() {
        let root = new_root("removed-absent");
        let configured = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-2")));
        reconciler
            .apply_with_report(&configured)
            .1
            .expect("apply configured");

        let disabled = parse_daemon("programs: {}").unwrap();
        let (report, result) = reconciler.apply_with_report(&disabled);
        result.expect("apply disabled");
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Removed)
        );

        let (report, result) = reconciler.apply_with_report(&disabled);
        result.expect("apply disabled again");
        assert!(
            program_outcome(&report, "copilot").is_none(),
            "an unconfigured, idle provider gets no row"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn use_llm_gateway_false_reports_applied_or_unchanged() {
        let root = new_root("use-llm-gateway-false");
        let with_gateway = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-3")));
        reconciler
            .apply_with_report(&with_gateway)
            .1
            .expect("apply with the gateway on");

        let gateway_off = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    useLlmGateway: false
"#,
        )
        .unwrap();
        let (report, result) = reconciler.apply_with_report(&gateway_off);
        result.expect("apply with the gateway off");
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Applied),
            "the managed file is removed, so a configured program is Applied, not Removed"
        );

        let (report, result) = reconciler.apply_with_report(&gateway_off);
        result.expect("apply with the gateway off again");
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Unchanged)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn conflict_blocks_other_programs_and_reports_blocked() {
        let root = new_root("conflict-blocked");
        fs::create_dir_all(root.join("codex")).unwrap();
        fs::write(root.join("codex/config.toml"), b"model = \"personal\"\n").unwrap();
        let config = parse_daemon("programs:\n  claudeCode: {}\n  codex: {}").unwrap();
        let reconciler = full_reconciler(&root);

        let (report, result) = reconciler.apply_with_report(&config);
        assert!(result.is_err(), "a conflict means nothing is written");
        assert_eq!(
            program_outcome(&report, "codex").map(|outcome| outcome.state),
            Some(ProgramState::Conflict)
        );
        let claude_code = program_outcome(&report, "claude-code").expect("claude-code is reported");
        assert_eq!(claude_code.state, ProgramState::Blocked);
        assert!(
            claude_code.detail.contains("codex"),
            "a blocked program's detail names the first conflicted program: {}",
            claude_code.detail
        );
        assert!(!root.join("claude").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_provider_plan_does_not_stop_the_others() {
        let root = new_root("failed-plan-continues");
        // Grok Build's managed config lives in a system location, so a
        // user-mode daemon rejects it in `plan()` (see
        // `user_mode_rejects_grok_before_writing_other_settings` above) while
        // Claude Code plans normally: a real, deterministic plan failure.
        let reconciler = full_reconciler(&root);
        let config = parse_daemon("programs:\n  claudeCode: {}\n  grok: {}").unwrap();

        let (report, result) = reconciler.apply_with_report(&config);
        assert!(result.is_err());
        let grok = program_outcome(&report, "grok").expect("grok is reported");
        assert_eq!(grok.state, ProgramState::Failed);
        assert!(grok.detail.contains("Grok Build"), "{}", grok.detail);
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Blocked),
            "claude-code was still planned, but nothing is written because the apply fails"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn observed_file_mismatch_fails_its_owners_and_blocks_the_rest() {
        let root = new_root("observed-mismatch");
        let reconciler = full_reconciler(&root);
        let original = parse_daemon("programs:\n  claudeCode: {}\n  codex: {}").unwrap();
        reconciler
            .apply_with_report(&original)
            .1
            .expect("initial apply");

        let update =
            parse_daemon("programs:\n  claudeCode:\n    env:\n      COMPANY: updated\n  codex: {}")
                .unwrap();
        let attributed = reconciler.plan_with_report(&update);
        // Codex's file changes after planning but before apply.
        fs::write(root.join("codex/config.toml"), b"externally changed").unwrap();
        let (report, result) = attributed.apply();
        assert!(result.is_err());
        assert_eq!(
            program_outcome(&report, "codex").map(|outcome| outcome.state),
            Some(ProgramState::Failed)
        );
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Blocked)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_failure_orders_applied_failed_blocked() {
        // SAFETY: geteuid has no preconditions and does not dereference pointers.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores directory permissions.
        }
        let root = new_root("write-failure-order");
        fs::create_dir_all(root.join("codex")).unwrap();
        let mut reconciler = full_reconciler(&root);
        reconciler.providers = std::sync::Arc::new(vec![
            Box::new(super::ClaudeCode {
                settings_path: root.join("claude/settings.json"),
            }),
            Box::new(super::Codex {
                managed_config_path: root.join("codex/config.toml"),
            }),
            Box::new(super::OpenCode {
                managed_config_path: root.join("opencode/config.json"),
                plugin_path: root.join("opencode/plugin.js"),
            }),
        ]);
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  codex: {}
  openCode:
    model: m
    models:
      m: {}
"#,
        )
        .unwrap();
        let attributed = reconciler.plan_with_report(&config);

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("codex"), fs::Permissions::from_mode(0o500)).unwrap();
        let (report, result) = attributed.apply();
        // Restore permissions so the temp directory can be removed.
        fs::set_permissions(root.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );
        assert_eq!(
            program_outcome(&report, "codex").map(|outcome| outcome.state),
            Some(ProgramState::Failed)
        );
        assert_eq!(
            program_outcome(&report, "opencode").map(|outcome| outcome.state),
            Some(ProgramState::Blocked)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copilot_and_vscode_own_models_go_inactive_when_the_proxy_is_absent() {
        let root = new_root("inactive-own-models");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-5")));
        let (applied, result) = reconciler.apply_with_report(&config);
        result.expect("apply with the proxy available");
        assert_eq!(
            program_outcome(&applied, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );
        assert_eq!(
            program_outcome(&applied, "vscode").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );

        // A restart (or a hot reload) without the proxy: from an applied
        // state, both programs go Inactive rather than being torn down as an
        // ordinary removal.
        let reconciler_without_proxy = full_reconciler(&root);
        let (report, result) = reconciler_without_proxy.apply_with_report(&config);
        result.expect("apply without the proxy still succeeds: files are removed, not written");
        for program in ["copilot", "vscode"] {
            let outcome = program_outcome(&report, program)
                .unwrap_or_else(|| panic!("{program} is reported"));
            assert_eq!(outcome.state, ProgramState::Inactive, "{program}");
            assert!(
                outcome.detail.contains("local LLM proxy not available"),
                "{program}: {}",
                outcome.detail
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn vscode_github_models_goes_inactive_when_the_proxy_is_absent() {
        let root = new_root("inactive-github-models");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  proxyUrl: https://gateway.example.com/copilot-proxy
programs:
  vscode:
    copilotChat: githubModels
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root);
        let (report, result) = reconciler.apply_with_report(&config);
        result.expect("apply without the proxy still succeeds");
        let outcome = program_outcome(&report, "vscode").expect("vscode is reported");
        assert_eq!(outcome.state, ProgramState::Inactive);
        assert!(
            outcome.detail.contains("local LLM proxy not available"),
            "{}",
            outcome.detail
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn vscode_conflict_on_one_file_outranks_inactive_on_the_other() {
        let root = new_root("vscode-conflict-outranks-inactive");
        let github_models = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  proxyUrl: https://gateway.example.com/copilot-proxy
programs:
  vscode:
    copilotChat: githubModels
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-7")));
        reconciler
            .apply_with_report(&github_models)
            .1
            .expect("apply githubModels with the proxy available");
        let settings_path = root.join("vscode/User/settings.json");
        assert!(settings_path.is_file(), "the override was written");

        // Break the shape the merge left in settings.json (an object) so its
        // removal below is a conflict, and switch to ownModels with no proxy
        // so the other managed file (chatLanguageModels.json) is Inactive in
        // the same apply: Conflict must outrank Inactive for the program.
        fs::write(&settings_path, b"[]\n").unwrap();
        let own_models = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
        )
        .unwrap();
        let reconciler = full_reconciler(&root);
        let (report, result) = reconciler.apply_with_report(&own_models);
        assert!(result.is_err(), "the conflict means nothing is written");
        let vscode = program_outcome(&report, "vscode").expect("vscode is reported");
        assert_eq!(
            vscode.state,
            ProgramState::Conflict,
            "conflict outranks inactive"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_only_and_unconfigured_idle_providers_get_no_row() {
        let root = new_root("no-row-for-idle");
        let reconciler = full_reconciler(&root);
        let config = parse_daemon("programs:\n  claudeCode: {}\n").unwrap();
        let (report, result) = reconciler.apply_with_report(&config);
        result.expect("apply succeeds");
        assert_eq!(
            program_outcome(&report, "claude-code").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );
        for absent in [
            "cursor",
            "ollama",
            "codex",
            "claude-desktop",
            "grok",
            "copilot",
            "vscode",
            "opencode",
        ] {
            assert!(
                program_outcome(&report, absent).is_none(),
                "{absent} must get no row"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn program_detail_is_truncated_to_1024_bytes() {
        let mut fixture = Fixture::new();
        // A multi-byte character straddles the 1024-byte cutoff so
        // truncation must land on a char boundary, not merely a byte count.
        let long_run = "a".repeat(1020);
        let message = format!("{long_run}\u{1F600}{long_run}");
        fixture.reconciler.providers = std::sync::Arc::new(vec![Box::new(FailWithMessage {
            message: message.clone(),
        })]);
        let config = parse_daemon("programs:\n  claudeCode: {}\n").unwrap();

        let (report, result) = fixture.reconciler.apply_with_report(&config);
        assert!(result.is_err());
        assert_eq!(
            report.programs.len(),
            1,
            "the one failing provider is reported"
        );
        let detail = &report.programs[0].detail;
        assert!(
            detail.len() <= 1024,
            "detail must be truncated to 1024 bytes: {} bytes",
            detail.len()
        );
        assert!(
            message.len() > 1024,
            "the untruncated message must exceed the limit"
        );
    }

    #[test]
    fn conflict_and_sidecar_failure_details_never_include_the_pairing() {
        const PAIRING: &str = "PAIRING-SECRET-13";

        // (a) A foreign "agentdesktop" vendor entry is a conflict.
        {
            let root = new_root("no-pairing-conflict");
            let chat_models = root.join("vscode/User/chatLanguageModels.json");
            fs::create_dir_all(chat_models.parent().unwrap()).unwrap();
            fs::write(
                &chat_models,
                serde_json::to_vec_pretty(&serde_json::json!([{
                    "name": "agentdesktop",
                    "vendor": "customendpoint",
                    "apiKey": "sk-user",
                    "apiType": "chat-completions",
                    "models": [{
                        "id": "not-ours",
                        "url": "https://not-ours.example.com/v1/chat/completions",
                    }],
                }]))
                .unwrap(),
            )
            .unwrap();
            let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context(PAIRING)));
            let config = parse_daemon(
                r#"
llmGateway:
  url: https://gateway.example.com
programs:
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
            )
            .unwrap();
            let (report, result) = reconciler.apply_with_report(&config);
            assert!(result.is_err());
            let vscode = program_outcome(&report, "vscode").expect("vscode is reported");
            assert_eq!(vscode.state, ProgramState::Conflict);
            assert!(
                !vscode.detail.contains(PAIRING),
                "conflict detail must not leak the pairing: {}",
                vscode.detail
            );
            let _ = fs::remove_dir_all(&root);
        }

        // (b) A corrupted sidecar file is a plan error (Failed), not a conflict.
        {
            let root = new_root("no-pairing-sidecar");
            let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context(PAIRING)));
            let config = parse_daemon(
                r#"
llmGateway:
  url: https://gateway.example.com
programs:
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
            )
            .unwrap();
            reconciler
                .apply_with_report(&config)
                .1
                .expect("initial apply");
            let sidecar = root.join("vscode/User/.chatLanguageModels.json.agentdesktop");
            assert!(sidecar.is_file(), "the sidecar was written");
            fs::write(&sidecar, b"{ not json").unwrap();

            let (report, result) = reconciler.apply_with_report(&config);
            assert!(result.is_err());
            let vscode = program_outcome(&report, "vscode").expect("vscode is reported");
            assert_eq!(vscode.state, ProgramState::Failed);
            assert!(
                !vscode.detail.contains(PAIRING),
                "sidecar parse failure detail must not leak the pairing: {}",
                vscode.detail
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn two_programs_planning_the_same_path_both_fail_and_the_rest_is_blocked() {
        let root = new_root("same-path");
        let shared = root.join("shared/settings.json");
        let mut reconciler = full_reconciler(&root);
        reconciler.providers = std::sync::Arc::new(vec![
            Box::new(super::ClaudeCode {
                settings_path: shared.clone(),
            }),
            Box::new(super::OpenCode {
                managed_config_path: root.join("opencode/config.json"),
                plugin_path: root.join("opencode/plugin.js"),
            }),
            Box::new(super::Codex {
                managed_config_path: shared,
            }),
        ]);
        let config = parse_daemon(
            "programs:\n  claudeCode: {}\n  codex: {}\n  openCode:\n    model: m\n    models:\n      m: {}\n",
        )
        .unwrap();
        let (report, result) = reconciler.apply_with_report(&config);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("multiple providers plan to modify")
        );
        let state = |program| program_outcome(&report, program).map(|outcome| outcome.state);
        assert_eq!(state("claude-code"), Some(ProgramState::Failed));
        assert_eq!(state("codex"), Some(ProgramState::Failed));
        assert_eq!(state("opencode"), Some(ProgramState::Blocked));
        assert!(!root.exists(), "nothing is written");
    }

    #[test]
    fn a_wrongly_typed_sidecar_field_does_not_put_its_value_in_the_detail() {
        const PAIRING: &str = "PAIRING-IN-SIDECAR";
        let root = new_root("typed-sidecar");
        let providers = root.join("copilot/providers.json");
        fs::create_dir_all(providers.parent().unwrap()).unwrap();
        fs::write(&providers, b"{}\n").unwrap();
        // `created` must be a boolean; serde would quote the string.
        fs::write(
            root.join("copilot/.providers.json.agentdesktop"),
            format!("{{\"created\": \"{PAIRING}\", \"before\": {{}}, \"after\": {{}}}}"),
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context(PAIRING)));
        let config = parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  copilot:\n    models:\n      gpt-4.1: {}\n",
        )
        .unwrap();
        let (report, result) = reconciler.apply_with_report(&config);
        assert!(result.is_err());
        let copilot = program_outcome(&report, "copilot").expect("copilot is reported");
        assert_eq!(copilot.state, ProgramState::Failed);
        assert!(!copilot.detail.contains(PAIRING), "{}", copilot.detail);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_private_directory_that_cannot_be_created_fails_its_owner_and_blocks_the_rest() {
        // SAFETY: geteuid has no preconditions and does not dereference pointers.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores directory permissions.
        }
        use std::os::unix::fs::PermissionsExt;
        let root = new_root("private-dir");
        let config = parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  claudeCode: {}\n  vscode:\n    models:\n      gpt-4.1-mini: {}\n",
        )
        .unwrap();
        let reconciler = full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-PD")));
        let attributed = reconciler.plan_with_report(&config);
        // The VS Code profile directory the plan wants to create owner-only
        // cannot be created any more.
        fs::create_dir_all(root.join("vscode")).unwrap();
        fs::set_permissions(root.join("vscode"), fs::Permissions::from_mode(0o500)).unwrap();
        let (report, result) = attributed.apply();
        fs::set_permissions(root.join("vscode"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        let state = |program| program_outcome(&report, program).map(|outcome| outcome.state);
        assert_eq!(state("vscode"), Some(ProgramState::Failed));
        assert_eq!(state("claude-code"), Some(ProgramState::Blocked));
        assert!(!root.join("claude").exists(), "nothing is written");
        let _ = fs::remove_dir_all(&root);
    }

    // --- No-op writes skipped, full reconciler ---------------------------------

    fn sidecar_path(path: &Path) -> PathBuf {
        let name = path.file_name().unwrap().to_str().unwrap();
        path.with_file_name(format!(".{name}.agentdesktop"))
    }

    #[test]
    fn second_apply_of_an_unchanged_multi_program_config_writes_nothing() {
        let root = new_root("second-apply-no-op");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  claudeCode: {}
  copilot:
    models:
      gpt-4.1: {}
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
        )
        .unwrap();
        let reconciler =
            full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-TICK-1")));
        reconciler
            .apply_with_report(&config)
            .1
            .expect("first apply succeeds");

        let paths = [
            root.join("claude/settings.json"),
            sidecar_path(&root.join("claude/settings.json")),
            root.join("copilot/providers.json"),
            sidecar_path(&root.join("copilot/providers.json")),
            root.join("vscode/User/chatLanguageModels.json"),
            sidecar_path(&root.join("vscode/User/chatLanguageModels.json")),
        ];
        let before: Vec<_> = paths
            .iter()
            .map(|path| {
                fs::metadata(path).unwrap_or_else(|error| {
                    panic!(
                        "{} must exist after the first apply: {error}",
                        path.display()
                    )
                })
            })
            .collect();

        reconciler
            .apply_with_report(&config)
            .1
            .expect("second apply succeeds");

        for (path, before) in paths.iter().zip(before) {
            let after = fs::metadata(path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(
                    after.ino(),
                    before.ino(),
                    "{} must not be rewritten by an unchanged apply",
                    path.display()
                );
            }
            assert_eq!(
                after.modified().unwrap(),
                before.modified().unwrap(),
                "{} mtime must not change on an unchanged apply",
                path.display()
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn providers_json_at_a_looser_mode_is_tightened_on_the_next_apply() {
        use std::os::unix::fs::PermissionsExt;
        let root = new_root("copilot-tighten");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let reconciler =
            full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-TICK-2")));
        reconciler
            .apply_with_report(&config)
            .1
            .expect("first apply succeeds");

        let providers_path = root.join("copilot/providers.json");
        fs::set_permissions(&providers_path, fs::Permissions::from_mode(0o664)).unwrap();
        let before_contents = fs::read(&providers_path).unwrap();

        reconciler
            .apply_with_report(&config)
            .1
            .expect("second apply succeeds");

        assert_eq!(
            fs::read(&providers_path).unwrap(),
            before_contents,
            "bytes must not change"
        );
        let mode = fs::metadata(&providers_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a looser providers.json must be tightened");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_managed_file_is_recreated_on_the_next_apply() {
        let root = new_root("recreate-deleted");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let reconciler =
            full_reconciler(&root).with_llm_proxy(Some(proxy_context("PAIRING-TICK-3")));
        reconciler
            .apply_with_report(&config)
            .1
            .expect("first apply succeeds");

        let providers_path = root.join("copilot/providers.json");
        let original = fs::read(&providers_path).unwrap();
        fs::remove_file(&providers_path).unwrap();

        let (report, result) = reconciler.apply_with_report(&config);
        result.expect("second apply recreates the deleted file");
        assert_eq!(
            program_outcome(&report, "copilot").map(|outcome| outcome.state),
            Some(ProgramState::Applied)
        );
        assert_eq!(fs::read(&providers_path).unwrap(), original);
        let _ = fs::remove_dir_all(&root);
    }

    // --- Serialized applies ----------------------------------------------------

    /// A provider whose `plan` records that it was entered (in order) and,
    /// only the first time, blocks until `release` is signalled. Later calls
    /// return immediately, so the test can tell whether a second
    /// `apply_with_report` was let into `plan` before the first released the
    /// lock.
    struct BlockingProbe {
        order: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        release: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for BlockingProbe {
        fn id(&self) -> &'static str {
            "blocking-probe"
        }

        async fn discover(&self) -> agentdesktop_core::model::Discovery {
            agentdesktop_core::model::Discovery {
                agents: Vec::new(),
                model_runtimes: Vec::new(),
            }
        }

        fn plan(
            &self,
            _ctx: &crate::provider::ReconcileContext,
            _config: &agentdesktop_core::config::DaemonConfig,
        ) -> anyhow::Result<super::ReconcilePlan> {
            let is_first = {
                let mut order = self.order.lock().unwrap();
                order.push("enter");
                order.len() == 1
            };
            if is_first {
                let (lock, condvar) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = condvar.wait(released).unwrap();
                }
            }
            Ok(super::ReconcilePlan::default())
        }
    }

    #[test]
    fn a_second_apply_on_a_clone_does_not_enter_plan_until_the_first_releases_the_lock() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let release =
            std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut reconciler = full_reconciler(&new_root("apply-lock"));
        reconciler.providers = std::sync::Arc::new(vec![Box::new(BlockingProbe {
            order: order.clone(),
            release: release.clone(),
        })]);
        let config = parse_daemon("programs: {}").unwrap();

        let first = reconciler.clone();
        let first_config = config.clone();
        let first_handle = std::thread::spawn(move || first.apply_with_report(&first_config));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while order.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first apply never entered plan"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let second = reconciler.clone();
        let second_config = config.clone();
        let second_handle = std::thread::spawn(move || second.apply_with_report(&second_config));

        // Give a wrongly-unserialized second call time to enter `plan`.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            order.lock().unwrap().len(),
            1,
            "a second apply_with_report on a clone must not enter plan while \
             the first holds the lock"
        );

        {
            let (lock, condvar) = &*release;
            *lock.lock().unwrap() = true;
            condvar.notify_all();
        }

        let (_, first_result) = first_handle.join().unwrap();
        let (_, second_result) = second_handle.join().unwrap();
        assert!(first_result.is_ok(), "{first_result:?}");
        assert!(second_result.is_ok(), "{second_result:?}");
        assert_eq!(
            order.lock().unwrap().len(),
            2,
            "the second apply must eventually run once the lock is released"
        );
    }

    // --- Tick logging dedup ----------------------------------------------------

    fn outcome_report(state: ProgramState, detail: &str) -> ApplyReport {
        ApplyReport {
            programs: vec![ProgramOutcome {
                program: "claude-code",
                state,
                detail: detail.to_owned(),
            }],
        }
    }

    #[test]
    fn should_log_with_nothing_applied_yet_always_logs() {
        let current = outcome_report(ProgramState::Applied, "");
        assert!(super::should_log(None, &current));
    }

    #[test]
    fn should_log_is_false_when_the_report_is_unchanged() {
        let previous = outcome_report(ProgramState::Unchanged, "");
        let current = outcome_report(ProgramState::Unchanged, "");
        assert!(!super::should_log(Some(&previous), &current));
    }

    #[test]
    fn should_log_is_false_when_applied_or_removed_settles_to_unchanged() {
        for state in [ProgramState::Applied, ProgramState::Removed] {
            let previous = outcome_report(state, "");
            let current = outcome_report(ProgramState::Unchanged, "");
            assert!(
                !super::should_log(Some(&previous), &current),
                "{state:?} -> Unchanged must not be logged"
            );
        }
    }

    #[test]
    fn should_log_is_true_when_a_programs_detail_changes() {
        let previous = outcome_report(ProgramState::Conflict, "conflict at a");
        let current = outcome_report(ProgramState::Conflict, "conflict at b");
        assert!(super::should_log(Some(&previous), &current));
    }

    #[test]
    fn should_log_is_true_when_a_programs_state_changes_to_something_other_than_unchanged() {
        let previous = outcome_report(ProgramState::Unchanged, "");
        let current = outcome_report(ProgramState::Failed, "boom");
        assert!(super::should_log(Some(&previous), &current));
    }

    #[test]
    fn a_program_that_disappears_is_a_difference_unless_it_was_removed() {
        let both = ApplyReport {
            programs: vec![
                ProgramOutcome {
                    program: "claude-code",
                    state: ProgramState::Unchanged,
                    detail: String::new(),
                },
                ProgramOutcome {
                    program: "copilot",
                    state: ProgramState::Unchanged,
                    detail: String::new(),
                },
            ],
        };
        let one = outcome_report(ProgramState::Unchanged, "");
        assert!(super::should_log(Some(&both), &one), "copilot vanished");
        let mut removed = both.clone();
        removed.programs[1].state = ProgramState::Removed;
        assert!(
            !super::should_log(Some(&removed), &one),
            "a removed program that is no longer listed is not news"
        );
    }
}
