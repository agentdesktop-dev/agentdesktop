use std::fmt;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub endpoint: SocketAddr,
    pub tunnel_token: String,
}

impl Registration {
    pub(super) fn validate(&self) -> std::io::Result<()> {
        if !self.endpoint.ip().is_loopback() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "local Gateway endpoint must be loopback",
            ));
        }
        crate::local_gateway::validate_capability(&self.tunnel_token)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        Ok(())
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelfManagedRegistration")
            .field("endpoint", &self.endpoint)
            .field("tunnel_token", &"[REDACTED]")
            .finish()
    }
}
