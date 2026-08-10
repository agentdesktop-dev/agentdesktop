pub fn setup_logging(default_level: &str, json: bool) -> impl Drop {
    agent_core::telemetry::setup_logging(default_level, json)
}
