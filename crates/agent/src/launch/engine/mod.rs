//! Sandbox engine interface.

use anyhow::Result;

use std::net::SocketAddr;

use super::plan::{LaunchPlan, PathStyle};

pub mod mxc;

/// A sandbox engine that accepts a resolved launch plan and produces a request file.
pub(crate) trait Engine: Send + Sync {
    /// The engine name as it appears in the config and posture.
    fn name(&self) -> &'static str;

    /// The container id for a given launch id.
    fn container_id(&self, launch_id: &str) -> String;

    /// The relay as a box on a host of `style` reaches it, given its
    /// address on the host. Equal to `host_relay` when the box shares host
    /// loopback.
    fn box_relay(&self, style: PathStyle, host_relay: SocketAddr) -> SocketAddr;

    /// Build the request bytes from a plan.
    fn request(&self, plan: &LaunchPlan, launch_id: &str) -> Result<Vec<u8>>;
}
