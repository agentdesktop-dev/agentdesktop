use std::path::PathBuf;

use agentdesktop::customization::{customize_installer, default_customized_name};
use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Create an organization-specific Agent Desktop installer"
)]
struct Cli {
    /// Generic managed installer template.
    installer: PathBuf,

    /// Organization bootstrap JSON.
    organization: PathBuf,

    /// Customized installer path.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let bootstrap = agentdesktop::organization::OrganizationBootstrap::parse(&std::fs::read(
        &cli.organization,
    )?)?;
    let output = cli
        .output
        .unwrap_or_else(|| default_customized_name(&cli.installer, &bootstrap.organization.id));
    customize_installer(&cli.installer, &cli.organization, &output)?;
    println!(
        "created {} installer at {}",
        bootstrap.organization.display_name,
        output.display()
    );
    Ok(())
}
