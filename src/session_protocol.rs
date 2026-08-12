use std::io::{Error, ErrorKind};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod managed;
pub mod self_managed;

pub use managed::{AgentResponse, ForwarderRequest, SignatureScheme};

pub const VERSION: u16 = 1;
const MAX_FRAME_SIZE: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Registration {
    pub version: u16,
    pub certificate_generation: u64,
    pub identity: RegistrationIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationIdentity {
    Managed(managed::Registration),
    SelfManaged(self_managed::Registration),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRegistration {
    version: u16,
    certificate_generation: u64,
    #[serde(default)]
    certificate_chain_der_base64: Vec<String>,
    #[serde(default)]
    local_gateway: Option<self_managed::Registration>,
}

#[derive(Serialize)]
struct WireRegistrationRef<'a> {
    version: u16,
    certificate_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    certificate_chain_der_base64: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_gateway: Option<&'a self_managed::Registration>,
}

impl Serialize for Registration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (certificate_chain_der_base64, local_gateway) = match &self.identity {
            RegistrationIdentity::Managed(registration) => {
                (Some(&registration.certificate_chain_der_base64), None)
            }
            RegistrationIdentity::SelfManaged(registration) => (None, Some(registration)),
        };
        WireRegistrationRef {
            version: self.version,
            certificate_generation: self.certificate_generation,
            certificate_chain_der_base64,
            local_gateway,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Registration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = WireRegistration::deserialize(deserializer)?;
        let identity = match (
            wire.certificate_chain_der_base64.is_empty(),
            wire.local_gateway,
        ) {
            (false, None) => RegistrationIdentity::Managed(managed::Registration {
                certificate_chain_der_base64: wire.certificate_chain_der_base64,
            }),
            (true, Some(registration)) => RegistrationIdentity::SelfManaged(registration),
            _ => {
                return Err(serde::de::Error::custom(
                    "registration must contain exactly one identity type",
                ));
            }
        };
        Ok(Self {
            version: wire.version,
            certificate_generation: wire.certificate_generation,
            identity,
        })
    }
}

impl Registration {
    pub fn managed(certificate_generation: u64, certificate_chain_der_base64: Vec<String>) -> Self {
        Self {
            version: VERSION,
            certificate_generation,
            identity: RegistrationIdentity::Managed(managed::Registration {
                certificate_chain_der_base64,
            }),
        }
    }

    pub fn self_managed(
        certificate_generation: u64,
        registration: self_managed::Registration,
    ) -> Self {
        Self {
            version: VERSION,
            certificate_generation,
            identity: RegistrationIdentity::SelfManaged(registration),
        }
    }

    pub fn validate(&self) -> std::io::Result<()> {
        if self.version != VERSION {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("unsupported session protocol version {}", self.version),
            ));
        }
        match &self.identity {
            RegistrationIdentity::Managed(registration) => registration.validate(),
            RegistrationIdentity::SelfManaged(registration) => registration.validate(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use rcgen::{CertifiedKey, generate_simple_self_signed};

    fn certificate_der_base64() -> String {
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        base64::engine::general_purpose::STANDARD.encode(cert.der())
    }

    #[tokio::test]
    async fn registration_round_trips() {
        let expected = Registration::managed(7, vec![certificate_der_base64()]);
        let (mut writer, mut reader) = tokio::io::duplex(4096);

        write_frame(&mut writer, &expected).await.unwrap();

        assert_eq!(
            read_frame::<Registration, _>(&mut reader).await.unwrap(),
            expected
        );
    }

    #[test]
    fn registration_identities_preserve_version_one_wire_shape() {
        let certificate = certificate_der_base64();
        let managed =
            serde_json::to_value(Registration::managed(7, vec![certificate.clone()])).unwrap();
        assert_eq!(
            managed,
            serde_json::json!({
                "version": VERSION,
                "certificate_generation": 7,
                "certificate_chain_der_base64": [certificate]
            })
        );

        let self_managed = serde_json::to_value(Registration::self_managed(
            8,
            self_managed::Registration {
                endpoint: "127.0.0.1:15008".parse().unwrap(),
                tunnel_token: "owner-only-token".to_owned(),
            },
        ))
        .unwrap();
        assert_eq!(
            self_managed,
            serde_json::json!({
                "version": VERSION,
                "certificate_generation": 8,
                "local_gateway": {
                    "endpoint": "127.0.0.1:15008",
                    "tunnel_token": "owner-only-token"
                }
            })
        );
    }

    #[test]
    fn rejects_ambiguous_registration_identity_on_deserialization() {
        let certificate = certificate_der_base64();
        for value in [
            serde_json::json!({
                "version": VERSION,
                "certificate_generation": 1
            }),
            serde_json::json!({
                "version": VERSION,
                "certificate_generation": 1,
                "certificate_chain_der_base64": [certificate],
                "local_gateway": {
                    "endpoint": "127.0.0.1:15008",
                    "tunnel_token": "owner-only-token"
                }
            }),
        ] {
            assert!(serde_json::from_value::<Registration>(value).is_err());
        }
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
    fn rejects_wrong_version_and_empty_chain() {
        let mut wrong_version = Registration::managed(1, vec![certificate_der_base64()]);
        wrong_version.version = VERSION + 1;
        assert_eq!(
            wrong_version.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let empty_chain = Registration::managed(1, Vec::new());
        assert_eq!(
            empty_chain.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let invalid_certificate = Registration::managed(1, vec!["not DER".to_owned()]);
        assert_eq!(
            invalid_certificate.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn validates_only_loopback_local_gateway_registration() {
        let local = Registration::self_managed(
            1,
            self_managed::Registration {
                endpoint: "127.0.0.1:15008".parse().unwrap(),
                tunnel_token: "owner-only-token".to_owned(),
            },
        );
        local.validate().unwrap();

        let mut remote = local.clone();
        let RegistrationIdentity::SelfManaged(remote) = &mut remote.identity else {
            unreachable!();
        };
        remote.endpoint = "192.0.2.1:15008".parse().unwrap();
        assert_eq!(
            remote.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );

        let mut invalid_token = local;
        let RegistrationIdentity::SelfManaged(registration) = &mut invalid_token.identity else {
            unreachable!();
        };
        registration.tunnel_token = "secret\nheader".to_owned();
        assert_eq!(
            invalid_token.validate().unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert!(!format!("{invalid_token:?}").contains("secret"));
    }
}
