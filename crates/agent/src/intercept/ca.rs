use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, pem::PemObject},
    sign::CertifiedKey,
};
use time::OffsetDateTime;

/// Validity of a generated intercept CA.
const CA_VALIDITY: Duration = Duration::from_secs(2 * 365 * 24 * 60 * 60);
/// Validity of a leaf. Leaves live only in memory and are minted again before they expire.
const LEAF_VALIDITY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A cached leaf is replaced once less than this much of its validity remains.
const LEAF_RENEW_BEFORE: Duration = Duration::from_secs(24 * 60 * 60);
/// Certificates start this far in the past, to allow for a client clock that runs behind.
const BACKDATE: Duration = Duration::from_secs(60 * 60);

/// The CA that the intercept listener mints its TLS leaves from.
///
/// The CA certificate carries X.509 Name Constraints: it may only issue for the
/// intercepted hosts (and their subdomains), and never for an IP address. A
/// verifier that enforces constraints on trust anchors (rustls-webpki,
/// OpenSSL, Chromium's verifier) then rejects any other leaf signed with this
/// key, even if the key leaks. Not every verifier enforces them, so the
/// listener also refuses to mint for any other host.
///
/// This is not the controller's device CA (`device_ca.rs`), which signs the
/// daemon's mTLS client certificates.
pub struct InterceptCa {
    issuer: Issuer<'static, KeyPair>,
    certificate_der: CertificateDer<'static>,
    certificate_pem: String,
    /// The CA private key, for the daemon's secret store. Never logged.
    key_pem: String,
    hosts: Vec<String>,
    leaves: Mutex<HashMap<String, Leaf>>,
}

struct Leaf {
    key: Arc<CertifiedKey>,
    not_after: SystemTime,
}

impl std::fmt::Debug for InterceptCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterceptCa")
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

impl InterceptCa {
    /// Generate a CA that may issue only for `hosts`. Each host must be a DNS
    /// name in lower case; a constraint on a name also covers its subdomains.
    pub fn generate(common_name: &str, hosts: &[String]) -> anyhow::Result<Self> {
        if hosts.is_empty() {
            anyhow::bail!("an intercept CA needs at least one host");
        }
        let key = KeyPair::generate().context("generate the intercept CA key")?;
        let mut params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: hosts
                .iter()
                .map(|host| GeneralSubtree::DnsName(host.clone()))
                .collect(),
            // DNS-only constraints leave IP-address names open, so all of them
            // are excluded.
            excluded_subtrees: vec![
                GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix([0; 4], 0)),
                GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0)),
            ],
        });
        let now = SystemTime::now();
        params.not_before = OffsetDateTime::from(now - BACKDATE);
        params.not_after = OffsetDateTime::from(now + CA_VALIDITY);
        let certificate = params
            .self_signed(&key)
            .context("self-sign the intercept CA")?;
        Ok(Self {
            certificate_der: certificate.der().clone(),
            certificate_pem: certificate.pem(),
            key_pem: key.serialize_pem(),
            issuer: Issuer::new(params, key),
            hosts: hosts.to_vec(),
            leaves: Mutex::default(),
        })
    }

    /// Load a CA that [`InterceptCa::generate`] made for `hosts` earlier. The
    /// caller keeps the host list with the certificate: a CA is only reused
    /// for the hosts its constraints were written for.
    pub fn from_pem(
        certificate_pem: &str,
        key_pem: &str,
        hosts: &[String],
    ) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(key_pem).context("parse the intercept CA key")?;
        let certificate_der = CertificateDer::from_pem_slice(certificate_pem.as_bytes())
            .context("parse the intercept CA certificate")?;
        let issuer = Issuer::from_ca_cert_der(&certificate_der, key)
            .context("load the intercept CA certificate")?;
        Ok(Self {
            issuer,
            certificate_der,
            certificate_pem: certificate_pem.to_owned(),
            key_pem: key_pem.to_owned(),
            hosts: hosts.to_vec(),
            leaves: Mutex::default(),
        })
    }

    pub(super) fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// The hosts this CA may issue for.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The CA certificate, for a trust store or an MDM profile.
    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    pub fn certificate_der(&self) -> &CertificateDer<'static> {
        &self.certificate_der
    }

    /// Whether this CA may issue for `host`.
    pub fn covers(&self, host: &str) -> bool {
        self.hosts.iter().any(|allowed| allowed == host)
    }

    /// A leaf for `host` with the CA in its chain, from the cache when it is
    /// not close to expiry. Only hosts the CA was generated for are minted.
    pub(super) fn leaf(&self, host: &str) -> anyhow::Result<Arc<CertifiedKey>> {
        if !self.covers(host) {
            anyhow::bail!("{host} is not an intercepted host");
        }
        let now = SystemTime::now();
        let mut leaves = self.leaves.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(leaf) = leaves.get(host)
            && leaf.not_after > now + LEAF_RENEW_BEFORE
        {
            return Ok(leaf.key.clone());
        }
        let leaf = self.mint(host, now)?;
        let key = leaf.key.clone();
        leaves.insert(host.to_owned(), leaf);
        Ok(key)
    }

    /// A leaf for any name, without the host check: a test uses it to show
    /// that a verifier rejects names outside the constraints.
    #[cfg(test)]
    pub(super) fn leaf_unchecked(&self, host: &str) -> anyhow::Result<Arc<CertifiedKey>> {
        Ok(self.mint(host, SystemTime::now())?.key)
    }

    fn mint(&self, host: &str, now: SystemTime) -> anyhow::Result<Leaf> {
        let key = KeyPair::generate().context("generate a leaf key")?;
        let mut params = CertificateParams::new(vec![host.to_owned()])
            .with_context(|| format!("leaf parameters for {host}"))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, host);
        params.distinguished_name = name;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let not_after = now + LEAF_VALIDITY;
        params.not_before = OffsetDateTime::from(now - BACKDATE);
        params.not_after = OffsetDateTime::from(not_after);
        let certificate = params
            .signed_by(&key, &self.issuer)
            .with_context(|| format!("sign a leaf for {host}"))?;
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&private_key)
            .context("load the leaf key")?;
        Ok(Leaf {
            key: Arc::new(CertifiedKey::new(
                vec![certificate.der().clone(), self.certificate_der.clone()],
                signing_key,
            )),
            not_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["claude.ai".to_owned(), "api.anthropic.com".to_owned()]
    }

    #[test]
    fn mints_only_for_its_hosts_and_reuses_the_cached_leaf() {
        let ca = InterceptCa::generate("test intercept CA", &hosts()).unwrap();
        let first = ca.leaf("claude.ai").unwrap();
        let second = ca.leaf("claude.ai").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.cert.len(), 2, "leaf and CA");
        assert_eq!(&first.cert[1], ca.certificate_der());
        assert!(ca.leaf("example.com").is_err());
        assert!(ca.leaf("claude.ai.example.com").is_err());
    }

    #[test]
    fn a_stored_ca_loads_and_still_mints() {
        let generated = InterceptCa::generate("test intercept CA", &hosts()).unwrap();
        let loaded =
            InterceptCa::from_pem(generated.certificate_pem(), generated.key_pem(), &hosts())
                .unwrap();
        assert_eq!(loaded.certificate_der(), generated.certificate_der());
        let leaf = loaded.leaf("api.anthropic.com").unwrap();
        assert_eq!(&leaf.cert[1], generated.certificate_der());
    }

    #[test]
    fn an_empty_host_list_is_refused() {
        assert!(InterceptCa::generate("test intercept CA", &[]).is_err());
    }

    /// Even a leaf the listener would never mint is rejected by a verifier
    /// that enforces Name Constraints on the trust anchor. This is
    /// rustls-webpki; Electron's verifier and Apple's SecTrust are not tested.
    #[test]
    fn a_verifier_rejects_names_outside_the_name_constraints() {
        use rustls::{
            RootCertStore,
            client::{WebPkiServerVerifier, danger::ServerCertVerifier},
            pki_types::{ServerName, UnixTime},
        };

        let ca = InterceptCa::generate("test intercept CA", &hosts()).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.certificate_der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .build()
        .unwrap();
        for (name, leaf, accepted) in [
            ("claude.ai", ca.leaf("claude.ai"), true),
            ("example.com", ca.leaf_unchecked("example.com"), false),
            ("127.0.0.1", ca.leaf_unchecked("127.0.0.1"), false),
        ] {
            let leaf = leaf.unwrap();
            let outcome = verifier.verify_server_cert(
                &leaf.cert[0],
                &[],
                &ServerName::try_from(name).unwrap(),
                &[],
                UnixTime::now(),
            );
            if accepted {
                assert!(outcome.is_ok(), "{name}: {outcome:?}");
            } else {
                let error = format!("{:?}", outcome.unwrap_err());
                assert!(error.contains("NameConstraint"), "{name}: {error}");
            }
        }
    }
}
