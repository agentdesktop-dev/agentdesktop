use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use agentdesktop_core::config::{ControllerConfig, DaemonConfig, DesiredConfig};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;

pub fn generate() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema_dir = root.join("schema");
    fs::create_dir_all(&schema_dir).context("create schema directory")?;

    write_schema::<DaemonConfig>(&root, "daemon-config", "Daemon Configuration")?;
    write_schema::<ControllerConfig>(&root, "controller-config", "Controller Configuration")?;
    write_schema::<DesiredConfig>(&root, "desired-config", "Desired Configuration")?;
    Ok(())
}

fn write_schema<T: JsonSchema>(root: &Path, stem: &str, title: &str) -> Result<()> {
    let schema_dir = root.join("schema");
    fs::write(schema_dir.join(format!("{stem}.json")), make::<T>(false)?)
        .with_context(|| format!("write schema/{stem}.json"))?;

    let inline_path = schema_dir.join(format!(".inline-{stem}.json"));
    fs::write(&inline_path, make::<T>(true)?)
        .with_context(|| format!("write temporary inline {stem} schema"))?;
    let output = Command::new(root.join("tools/schema-to-md.sh"))
        .arg(&inline_path)
        .output()
        .context("run tools/schema-to-md.sh; ensure jq is installed")?;
    let _ = fs::remove_file(&inline_path);
    if !output.status.success() {
        bail!(
            "schema documentation generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let table = String::from_utf8(output.stdout).context("schema table is not UTF-8")?;
    fs::write(
        schema_dir.join(format!("{stem}.md")),
        format!("# {title} Schema\n\n{table}"),
    )
    .with_context(|| format!("write schema/{stem}.md"))?;
    Ok(())
}

fn make<T: JsonSchema>(inline_subschemas: bool) -> Result<String> {
    let settings = schemars::generate::SchemaSettings::default().with(|settings| {
        settings.inline_subschemas = inline_subschemas;
        settings.contract = schemars::generate::Contract::Deserialize;
    });
    let generator = schemars::SchemaGenerator::new(settings);
    let schema = generator.into_root_schema_for::<T>();
    Ok(format!("{}\n", serde_json::to_string_pretty(&schema)?))
}
