use std::fmt;
use std::io::{Error, ErrorKind, Read, Write};
use std::net::SocketAddr;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use x509_parser::parse_x509_certificate;

pub const VERSION: u16 = 1;
const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub version: u16,
    pub certificate_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate_chain_der_base64: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_gateway: Option<LocalGatewayRegistration>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalGatewayRegistration {
    pub endpoint: SocketAddr,
    pub tunnel_token: String,
}

impl fmt::Debug for LocalGatewayRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalGatewayRegistration")
            .field("endpoint", &self.endpoint)
            .field("tunnel_token", &"[REDACTED]")
            .finish()
    }
}

impl Registration {
    pub fn validate(&self) -> std::io::Result<()> {
        if self.version != VERSION {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("unsupported session protocol version {}", self.version),
            ));
        }
        if self.certificate_chain_der_base64.is_empty() == self.local_gateway.is_none() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "registration must contain exactly one identity type",
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
        if let Some(gateway) = &self.local_gateway {
            if !gateway.endpoint.ip().is_loopback() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "local Gateway endpoint must be loopback",
                ));
            }
            if gateway.tunnel_token.is_empty()
                || gateway.tunnel_token.len() > 256
                || !gateway
                    .tunnel_token
                    .bytes()
                    .all(|byte| (0x21..=0x7e).contains(&byte))
            {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "local Gateway tunnel token is invalid",
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

pub async fn write_frame<T, W>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value).map_err(Error::other)?;
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|length| (*length as usize) <= MAX_FRAME_SIZE)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "session protocol frame is too large",
            )
        })?;
    writer.write_u32(length).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

pub async fn read_frame<T, R>(reader: &mut R) -> std::io::Result<T>
where
    T: for<'de> Deserialize<'de>,
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length > MAX_FRAME_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "session protocol frame is too large",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(|error| Error::new(ErrorKind::InvalidData, error))
}

pub fn write_frame_blocking<T, W>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    T: Serialize,
    W: Write,
{
    let payload = serde_json::to_vec(value).map_err(Error::other)?;
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|length| (*length as usize) <= MAX_FRAME_SIZE)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "session protocol frame is too large",
            )
        })?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn read_frame_blocking<T, R>(reader: &mut R) -> std::io::Result<T>
where
    T: for<'de> Deserialize<'de>,
    R: Read,
{
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "session protocol frame is too large",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(|error| Error::new(ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertifiedKey, generate_simple_self_signed};

    fn certificate_der_base64() -> String {
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        base64::engine::general_purpose::STANDARD.encode(cert.der())
    }

    #[tokio::test]
    async fn registration_round_trips() {
        let expected = Registration {
            version: VERSION,
            certificate_generation: 7,
            certificate_chain_der_base64: vec![certificate_der_base64()],
            local_gateway: None,
        };
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        write_frame(&mut writer, &expected).await.unwrap();

        assert_eq!(
            read_frame::<Registration, _>(&mut reader).await.unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn rejects_oversized_incoming_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_u32(u32::try_from(MAX_FRAME_SIZE + 1).unwrap())
            .await
            .unwrap();

        let error = read_frame::<Registration, _>(&mut reader)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn rejects_unknown_fields() {
        let value = serde_json::json!({
            "version": VERSION,
            "certificate_generation": 1,
            "certificate_chain_der_base64": [],
            "user_id": "untrusted"
        });
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_frame(&mut writer, &value).await.unwrap();

        let error = read_frame::<Registration, _>(&mut reader)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn async_and_blocking_frames_are_compatible() {
        let expected = AgentResponse::Signature {
            request_id: 9,
            signature_base64: "signature".to_owned(),
        };
        let mut bytes = Vec::new();
        write_frame_blocking(&mut bytes, &expected).unwrap();
        let actual: AgentResponse = read_frame_blocking(&mut bytes.as_slice()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_wrong_version_and_empty_chain() {
        let wrong_version = Registration {
            version: VERSION + 1,
            certificate_generation: 1,
            certificate_chain_der_base64: vec![certificate_der_base64()],
            local_gateway: None,
        };
        assert_eq!(
            wrong_version.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let empty_chain = Registration {
            version: VERSION,
            certificate_generation: 1,
            certificate_chain_der_base64: Vec::new(),
            local_gateway: None,
        };
        assert_eq!(
            empty_chain.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let invalid_certificate = Registration {
            version: VERSION,
            certificate_generation: 1,
            certificate_chain_der_base64: vec!["not DER".to_owned()],
            local_gateway: None,
        };
        assert_eq!(
            invalid_certificate.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn validates_only_loopback_local_gateway_registration() {
        let local = Registration {
            version: VERSION,
            certificate_generation: 1,
            certificate_chain_der_base64: Vec::new(),
            local_gateway: Some(LocalGatewayRegistration {
                endpoint: "127.0.0.1:15008".parse().unwrap(),
                tunnel_token: "owner-only-token".to_owned(),
            }),
        };
        local.validate().unwrap();

        let mut remote = local.clone();
        remote.local_gateway.as_mut().unwrap().endpoint = "192.0.2.1:15008".parse().unwrap();
        assert_eq!(
            remote.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let mut mixed = local;
        mixed.certificate_chain_der_base64 = vec![certificate_der_base64()];
        assert_eq!(mixed.validate().unwrap_err().kind(), ErrorKind::InvalidData);

        let mut invalid_token = mixed;
        invalid_token.certificate_chain_der_base64.clear();
        invalid_token.local_gateway.as_mut().unwrap().tunnel_token = "secret\nheader".to_owned();
        assert_eq!(
            invalid_token.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert!(!format!("{invalid_token:?}").contains("secret"));
    }
}
