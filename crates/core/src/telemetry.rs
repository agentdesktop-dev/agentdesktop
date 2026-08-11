use std::{env, io};

use tracing_subscriber::EnvFilter;

/// Initializes process logging.
///
/// `RUST_LOG` overrides `default_level`. Setting `LOG_FORMAT=json` selects JSON
/// output; otherwise the `json` argument supplies the default format.
pub fn setup_logging(
    default_level: &str,
    json: bool,
) -> tracing_appender::non_blocking::WorkerGuard {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let use_json = env::var("LOG_FORMAT")
        .map(|format| format == "json")
        .unwrap_or(json);
    let (writer, guard) = tracing_appender::non_blocking(io::stdout());
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);

    if use_json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }

    guard
}
