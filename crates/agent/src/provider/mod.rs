//! Built-in agent and model-runtime integrations.
//!
//! Discovery is best effort: an absent installation or unreachable runtime
//! contributes no entries. Reconciliation is optional and only builds a plan.
use std::path::PathBuf;

use agentdesktop_core::{
    config::DaemonConfig,
    model::{DaemonScope, Discovery},
};

use crate::reconcile::ReconcilePlan;

pub mod claude_code;
pub mod claude_desktop;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod grok;
mod json_merge;
pub(crate) mod metadata;
pub mod ollama;
pub mod opencode;
pub mod pi;
pub(crate) mod shared;
pub mod vscode;

/// An integration with a developer tool or local model runtime.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// The provider's ID (the inventory's agent kind and the program key in
    /// configuration status reports).
    fn id(&self) -> &'static str;

    async fn discover(&self) -> Discovery;

    /// Propose configuration changes, including cleanup when disabled.
    /// Discovery-only providers leave configuration untouched.
    fn plan(
        &self,
        _ctx: &ReconcileContext,
        _config: &DaemonConfig,
    ) -> anyhow::Result<ReconcilePlan> {
        Ok(ReconcilePlan::default())
    }
}

/// Common inputs for configuration management. Provider-specific paths live
/// on the concrete provider, so callers can override them independently.
#[derive(Clone)]
pub struct ReconcileContext {
    pub merge_user_settings: bool,
    pub credential_helper: PathBuf,
    pub socket: PathBuf,
    /// The bound loopback LLM proxy, for programs whose client files point at
    /// it instead of a credential helper. `None` when the proxy is off, its bind
    /// failed, or no pairing could be created: such programs must then remove
    /// or neutralise any pointer they wrote earlier, never leave a stale one.
    pub llm_proxy: Option<crate::llm_proxy::LlmProxyContext>,
}

impl ReconcileContext {
    /// Skip providers outside this daemon's scope before they read or clean up
    /// any files. Only configured programs need an inactive status.
    fn skip_unless(
        &self,
        required_scope: DaemonScope,
        configured: bool,
        display_name: &str,
    ) -> Option<ReconcilePlan> {
        if self.merge_user_settings == (required_scope == DaemonScope::User) {
            return None;
        }
        Some(if configured {
            let (required, current) = match required_scope {
                DaemonScope::User => ("user mode (--user)", "system mode"),
                DaemonScope::System => ("system mode", "user mode"),
            };
            ReconcilePlan::skipped(&format!(
                "{display_name} requires {required}; skipped in {current}"
            ))
        } else {
            ReconcilePlan::default()
        })
    }
}
