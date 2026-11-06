//! Sandbox engine interface.

use anyhow::Result;

use super::plan::LaunchPlan;

pub mod mxc;

/// A sandbox engine that accepts a resolved launch plan and produces a request file.
pub(crate) trait Engine: Send + Sync {
    /// The engine name as it appears in the config and posture.
    fn name(&self) -> &'static str;

    /// The container id for a given launch id.
    fn container_id(&self, launch_id: &str) -> String;

    /// Build the request bytes from a plan.
    fn request(&self, plan: &LaunchPlan, launch_id: &str) -> Result<Vec<u8>>;
}
