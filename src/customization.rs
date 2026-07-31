use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::organization::OrganizationBootstrap;

const MAGIC: &[u8; 16] = b"AGEDGE-ORG-v1\0\0\0";
const DIGEST_LENGTH: usize = 32;
const LENGTH_LENGTH: usize = 8;
const FOOTER_LENGTH: usize = DIGEST_LENGTH + LENGTH_LENGTH + MAGIC.len();

pub fn customize_installer(
    template: &Path,
    organization: &Path,
    output: &Path,
) -> Result<OrganizationBootstrap> {
    if template == output {
        bail!("customized installer output must differ from its template");
    }
    let metadata = fs::symlink_metadata(template)
        .with_context(|| format!("installer template {} is unavailable", template.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("installer template must be a regular file");
    }
    let template_bytes = fs::read(template)?;
    if decode_bootstrap(&template_bytes)?.is_some() {
        bail!("installer template is already organization-specific");
    }
    let bootstrap = OrganizationBootstrap::parse(&fs::read(organization).with_context(|| {
        format!(
            "organization bootstrap {} is unavailable",
            organization.display()
        )
    })?)?;
    let encoded = serde_json::to_vec(&bootstrap)?;
    let length = u64::try_from(encoded.len()).context("organization bootstrap is too large")?;
    let digest = Sha256::digest(&encoded);

    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        options.mode(metadata.permissions().mode());
    }
    let mut customized = options
        .open(output)
        .with_context(|| format!("customized installer {} already exists", output.display()))?;
    customized.write_all(&template_bytes)?;
    customized.write_all(&encoded)?;
    customized.write_all(&digest)?;
    customized.write_all(&length.to_le_bytes())?;
    customized.write_all(MAGIC)?;
    customized.sync_all()?;
    Ok(bootstrap)
}

pub fn read_customized_bootstrap(path: &Path) -> Result<Option<OrganizationBootstrap>> {
    decode_bootstrap(
        &fs::read(path).with_context(|| format!("failed to read installer {}", path.display()))?,
    )
}

fn decode_bootstrap(installer: &[u8]) -> Result<Option<OrganizationBootstrap>> {
    if installer.len() < MAGIC.len() || &installer[installer.len() - MAGIC.len()..] != MAGIC {
        return Ok(None);
    }
    if installer.len() < FOOTER_LENGTH {
        bail!("customized installer footer is truncated");
    }
    let length_offset = installer.len() - MAGIC.len() - LENGTH_LENGTH;
    let length = usize::try_from(u64::from_le_bytes(
        installer[length_offset..length_offset + LENGTH_LENGTH]
            .try_into()
            .expect("length slice has fixed size"),
    ))
    .context("organization bootstrap length is unsupported")?;
    let digest_offset = length_offset - DIGEST_LENGTH;
    let bootstrap_offset = digest_offset
        .checked_sub(length)
        .context("customized installer bootstrap length is invalid")?;
    let encoded = &installer[bootstrap_offset..digest_offset];
    if Sha256::digest(encoded).as_slice() != &installer[digest_offset..length_offset] {
        bail!("customized installer organization bootstrap is corrupt");
    }
    Ok(Some(OrganizationBootstrap::parse(encoded)?))
}

pub fn default_customized_name(template: &Path, organization_id: &str) -> PathBuf {
    let name = template
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("agentdesktop-installer"))
        .to_string_lossy();
    template.with_file_name(format!("{organization_id}-{name}"))
}

#[cfg(test)]
mod tests {
    use super::{customize_installer, read_customized_bootstrap};
    use std::fs;

    #[test]
    fn emits_and_reads_one_organization_specific_file() {
        let temporary = tempfile::tempdir().unwrap();
        let template = temporary.path().join("installer");
        let organization = temporary.path().join("organization.json");
        let output = temporary.path().join("acme-installer");
        fs::write(&template, b"generic executable").unwrap();
        fs::write(
            &organization,
            br#"{
              "format_version": 1,
              "organization": {
                "id": "acme",
                "display_name": "Acme Corporation",
                "support_url": "https://help.acme.example/agentdesktop"
              },
              "identity": {
                "issuer": "https://login.acme.example/",
                "client_id": "agentdesktop",
                "audience": "https://gateway.acme.example",
                "scope": "agentgateway.invoke"
              },
              "gateway": { "url": "https://gateway.acme.example/" }
            }"#,
        )
        .unwrap();

        let customized = customize_installer(&template, &organization, &output).unwrap();
        assert_eq!(customized.organization.id, "acme");
        assert_eq!(
            read_customized_bootstrap(&output)
                .unwrap()
                .unwrap()
                .organization
                .display_name,
            "Acme Corporation"
        );
        assert!(customize_installer(&output, &organization, &template).is_err());
    }

    #[test]
    fn detects_corrupted_embedded_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let template = temporary.path().join("installer");
        let organization = temporary.path().join("organization.json");
        let output = temporary.path().join("acme-installer");
        fs::write(&template, b"generic executable").unwrap();
        fs::write(
            &organization,
            br#"{
              "format_version": 1,
              "organization": {"id":"acme","display_name":"Acme","support_url":"https://help.acme.example/"},
              "identity": {"issuer":"https://login.acme.example/","client_id":"agentdesktop","audience":"gateway","scope":"invoke"},
              "gateway": {"url":"https://gateway.acme.example/"}
            }"#,
        )
        .unwrap();
        customize_installer(&template, &organization, &output).unwrap();
        let mut bytes = fs::read(&output).unwrap();
        let index = bytes.len() - 57;
        bytes[index] ^= 1;
        fs::write(&output, bytes).unwrap();

        assert!(read_customized_bootstrap(&output).is_err());
    }
}
