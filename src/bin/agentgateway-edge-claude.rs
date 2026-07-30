use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use agentgateway_edge_connector::apps::claude::{ClaudeConfig, ClaudePath};
use anyhow::Context;
use clap::Parser;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Launch Claude Code through a standalone Agent Gateway path"
)]
struct Cli {
    /// Route Claude directly to Agent Gateway or through the connector.
    #[arg(long, value_enum)]
    path: ClaudePath,

    /// Override the selected path's loopback base URL.
    #[arg(long)]
    base_url: Option<Url>,

    /// Claude Code executable.
    #[arg(long, default_value = "claude")]
    claude_binary: PathBuf,

    /// Arguments passed to Claude Code.
    #[arg(last = true, allow_hyphen_values = true)]
    claude_args: Vec<OsString>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let api_key = std::env::var("AGENTGATEWAY_EDGE_CLAUDE_CREDENTIAL")
        .unwrap_or_else(|_| "local-gateway-placeholder".to_owned());
    let config = ClaudeConfig::standalone(cli.path, cli.base_url, api_key)?;
    let status = Command::new(&cli.claude_binary)
        .args(cli.claude_args)
        .env("ANTHROPIC_BASE_URL", config.base_url.as_str())
        .env("ANTHROPIC_API_KEY", config.api_key)
        .status()
        .with_context(|| {
            format!(
                "failed to launch Claude Code from {}",
                cli.claude_binary.display()
            )
        })?;

    std::process::exit(status.code().unwrap_or(1));
}
