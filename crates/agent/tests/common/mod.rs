use std::{io::IsTerminal, sync::Once};

use tracing_subscriber::EnvFilter;

pub use agentgateway::Agentgateway;
pub use container::Container;
pub use gateway::{Gateway, TOKEN};
pub use issuer::Issuer;

mod agentgateway;
mod container;
mod gateway;
pub mod issuer;

fn setup_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .compact()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_timer(tracing_subscriber::fmt::time::uptime())
            .with_target(false)
            .with_ansi(std::io::stdout().is_terminal())
            .with_test_writer()
            .init();
    });
}
