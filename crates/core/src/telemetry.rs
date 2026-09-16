use std::{env, io, path::Path};

use anyhow::Context;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// Initializes process logging.
///
/// `RUST_LOG` overrides `default_level`. Setting `LOG_FORMAT=json` selects JSON
/// output; otherwise the `json` argument supplies the default format.
///
/// When `log_dir` is given, logs are written to a daily-rotated file under
/// that directory instead of stdout. Use this for daemons that cannot rely
/// on their supervisor (e.g. launchd, systemd) to capture and persist
/// stdout/stderr on their behalf — an unmanaged supervisor entry silently
/// discards everything the process ever logs, with no error to signal that
/// logging is a no-op. Writing straight to a file the process controls
/// itself is the only way to guarantee logs exist regardless of how the
/// process happens to be supervised.
pub fn setup_logging(
    default_level: &str,
    json: bool,
    log_dir: Option<&Path>,
) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let use_json = env::var("LOG_FORMAT")
        .map(|format| format == "json")
        .unwrap_or(json);

    let (writer, guard) = match log_dir {
        Some(dir) => tracing_appender::non_blocking(daily_rotating_file(dir)?),
        None => tracing_appender::non_blocking(io::stdout()),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(log_dir.is_none())
        .with_writer(writer);

    if use_json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }

    Ok(guard)
}

/// Builds a daily-rotated log file appender under `dir`, creating the
/// directory if needed. Split out from `setup_logging` so the file-creation
/// logic can be tested without touching the process-global `tracing`
/// subscriber, which can only be installed once per process.
fn daily_rotating_file(dir: &Path) -> anyhow::Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create log directory {}", dir.display()))?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("daemon")
        .filename_suffix("log")
        .max_log_files(14)
        .build(dir)
        .with_context(|| format!("open daemon log file in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::daily_rotating_file;

    #[test]
    fn daily_rotating_file_creates_the_log_directory_and_writes_through() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agentdesktop-telemetry-test-{}-{unique}",
            std::process::id(),
        ));
        // The directory must not exist yet: this exercises the daemon's
        // actual startup case, where nothing has created `state_dir/logs`
        // before the first log line is written.
        assert!(!dir.exists());

        let mut appender = daily_rotating_file(&dir).expect("build daily rotating file appender");
        appender
            .write_all(b"hello from the daemon\n")
            .expect("write a log line");
        appender.flush().expect("flush the log line");

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read the created log directory")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one log file");
        let contents = std::fs::read_to_string(entries[0].path()).expect("read the log file");
        assert!(contents.contains("hello from the daemon"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
