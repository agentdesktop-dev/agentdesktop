//! Built-in agent and model-runtime integrations.
//!
//! Discovery is best effort: an absent installation or unreachable runtime
//! contributes no entries. Reconciliation is optional and only builds a plan.
use std::path::PathBuf;

use agentdesktop_core::{config::DaemonConfig, model::Discovery};

use crate::reconcile::ReconcilePlan;

pub mod claude_code;
pub mod claude_desktop;
pub mod codex;
pub mod cursor;
pub mod grok;
mod json_merge;
mod metadata;
pub mod ollama;
pub mod opencode;
pub(crate) mod shared;
pub mod vscode;

/// An integration with a developer tool or local model runtime.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
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
}
