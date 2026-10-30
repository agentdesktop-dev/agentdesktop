pub mod anthropic_oauth;
pub mod api;
pub mod cli;
pub mod daemon;
pub mod enrollment;
pub mod gateway_oidc;
mod github_oauth;
pub mod identity;
mod inventory_watch;
pub mod launch;
mod llm_proxy;
pub mod oidc;
// The production renderer is the PowerShell build script; the Rust
// counterpart exists for the golden tests only.
#[cfg(test)]
mod packaging;
mod program_choice;
pub mod provider;
pub mod reconcile;
pub mod remote;
pub mod secret_store;
pub mod secure_fs;
pub mod subscription;
mod tick;

#[cfg(windows)]
mod windows_security;
