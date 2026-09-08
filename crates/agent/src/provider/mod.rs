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
mod json_merge;
mod metadata;
pub mod ollama;
pub mod opencode;
pub(crate) mod shared;
pub mod vscode;

/// An integration with a developer tool or local model runtime.
/// Built-ins use static dispatch; this is not a plugin ABI.
#[allow(async_fn_in_trait)]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    async fn discover(&self) -> Discovery;
}

/// Implemented only by providers that manage configuration.
pub trait Reconcile: Provider {
    /// Read current state and propose changes without modifying the filesystem.
    /// Missing provider configuration must plan cleanup of owned settings.
    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan>;
}

/// Common inputs for configuration management. Provider-specific paths live
/// on the concrete provider, so callers can override them independently.
#[derive(Clone)]
pub struct ReconcileContext {
    pub merge_user_settings: bool,
    pub credential_helper: PathBuf,
    pub socket: PathBuf,
}
