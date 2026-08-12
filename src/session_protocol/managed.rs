use std::io::{Error, ErrorKind};

use base64::Engine;
use serde::{Deserialize, Serialize};
use x509_parser::parse_x509_certificate;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Registration {
    pub certificate_chain_der_base64: Vec<String>,
}

impl Registration {
    pub(super) fn validate(&self) -> std::io::Result<()> {
        if self.certificate_chain_der_base64.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "managed registration requires a certificate chain",
            ));
        }
        for certificate in &self.certificate_chain_der_base64 {
            let der = base64::engine::general_purpose::STANDARD
                .decode(certificate)
                .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
            let (remaining, _) = parse_x509_certificate(&der)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "certificate is not valid DER"))?;
            if !remaining.is_empty() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "certificate DER has trailing data",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureScheme {
    EcdsaP256Sha256,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum ForwarderRequest {
    Sign {
        request_id: u64,
        scheme: SignatureScheme,
        message_base64: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum AgentResponse {
    Signature {
        request_id: u64,
        signature_base64: String,
    },
    Error {
        request_id: u64,
        reason: String,
    },
}
