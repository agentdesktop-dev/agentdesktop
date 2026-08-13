use std::sync::Arc;

use anyhow::Context;
use rcgen::{
    CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use time::{Duration, OffsetDateTime};

#[derive(Clone)]
pub struct DeviceCertificateIssuer {
    inner: Arc<Inner>,
}

struct Inner {
    issuer: Issuer<'static, KeyPair>,
    ca_certificate_pem: String,
}

pub struct IssuedCertificate {
    pub chain_pem: Vec<u8>,
    pub expires_at_unix_seconds: u64,
}

impl DeviceCertificateIssuer {
    pub fn from_pem(ca_certificate_pem: String, ca_private_key_pem: &str) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(ca_private_key_pem).context("parse device CA private key")?;
        let issuer = Issuer::from_ca_cert_pem(&ca_certificate_pem, key)
            .context("parse device CA certificate")?;
        Ok(Self {
            inner: Arc::new(Inner {
                issuer,
                ca_certificate_pem,
            }),
        })
    }

    pub fn issue(&self, device_id: &str, csr_der: &[u8]) -> anyhow::Result<IssuedCertificate> {
        if csr_der.is_empty() {
            anyhow::bail!("certificate signing request is required");
        }
        let csr_der = csr_der.into();
        let mut csr = CertificateSigningRequestParams::from_der(&csr_der)
            .context("parse and verify certificate signing request")?;

        // The controller owns identity-bearing certificate fields. Only the verified
        // public key is accepted from the daemon's CSR.
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, device_id);
        csr.params.distinguished_name = distinguished_name;
        csr.params.subject_alt_names = vec![SanType::DnsName(
            format!("{device_id}.device.agentdesktop.invalid")
                .try_into()
                .context("encode device certificate DNS name")?,
        )];
        csr.params.is_ca = IsCa::ExplicitNoCa;
        csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let now = OffsetDateTime::now_utc();
        csr.params.not_before = now - Duration::minutes(5);
        let not_after = now + Duration::days(30);
        csr.params.not_after = not_after;

        let certificate = csr
            .signed_by(&self.inner.issuer)
            .context("sign device certificate")?;
        let chain_pem = format!("{}{}", certificate.pem(), self.inner.ca_certificate_pem);
        Ok(IssuedCertificate {
            chain_pem: chain_pem.into_bytes(),
            expires_at_unix_seconds: not_after.unix_timestamp().try_into().unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};

    use super::DeviceCertificateIssuer;

    #[test]
    fn signs_a_verified_csr_for_the_controller_assigned_device() {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = DeviceCertificateIssuer::from_pem(ca.pem(), &ca_key.serialize_pem()).unwrap();

        let device_key = KeyPair::generate().unwrap();
        let csr = CertificateParams::new(vec!["ignored.example".to_owned()])
            .unwrap()
            .serialize_request(&device_key)
            .unwrap();
        let issued = issuer.issue("device-123", csr.der().as_ref()).unwrap();

        let pem = String::from_utf8(issued.chain_pem).unwrap();
        assert!(pem.matches("BEGIN CERTIFICATE").count() >= 2);

        let mut tampered = csr.der().as_ref().to_vec();
        let last = tampered.last_mut().unwrap();
        *last ^= 1;
        assert!(issuer.issue("device-123", &tampered).is_err());
    }
}
