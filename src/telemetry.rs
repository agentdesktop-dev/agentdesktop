use anyhow::{Result, anyhow};
use tracing::Level;

pub fn init() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_max_level(Level::INFO)
        .with_current_span(false)
        .with_span_list(false)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize structured logging: {error}"))
}
