use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const STANDALONE_PAYLOADS: [(&str, &str); 5] = [
    ("installer", "AGENTDESKTOP_PAYLOAD_INSTALLER"),
    ("connector", "AGENTDESKTOP_PAYLOAD_CONNECTOR"),
    ("agentgateway", "AGENTDESKTOP_PAYLOAD_AGENTGATEWAY"),
    ("capture-setup", "AGENTDESKTOP_PAYLOAD_CAPTURE_SETUP"),
    ("config", "AGENTDESKTOP_PAYLOAD_CONFIG"),
];

const MANAGED_PAYLOADS: [(&str, &str); 2] = [
    ("installer", "AGENTDESKTOP_PAYLOAD_INSTALLER"),
    ("connector", "AGENTDESKTOP_PAYLOAD_CONNECTOR"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBEDDED_INSTALLER");
    println!("cargo:rerun-if-env-changed=AGENTDESKTOP_INSTALLER_MODE");
    for (_, variable) in STANDALONE_PAYLOADS {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let generated = output.join("embedded_payload.rs");
    let mode = env::var("AGENTDESKTOP_INSTALLER_MODE").unwrap_or_else(|_| "standalone".to_owned());
    let payloads: &[(&str, &str)] = match mode.as_str() {
        "standalone" => &STANDALONE_PAYLOADS,
        "managed" => &MANAGED_PAYLOADS,
        other => panic!("unsupported AGENTDESKTOP_INSTALLER_MODE {other:?}"),
    };
    let configured_payloads = payloads
        .iter()
        .filter(|(_, variable)| env::var_os(variable).is_some())
        .count();
    if env::var_os("CARGO_FEATURE_EMBEDDED_INSTALLER").is_none() || configured_payloads == 0 {
        fs::write(
            generated,
            "const EMBEDDED: bool = false;\nconst INSTALLER_MODE: &str = \"none\";\nconst PAYLOADS: &[EmbeddedPayload] = &[];\n",
        )
        .expect("write empty embedded payload");
        return;
    }
    if configured_payloads != payloads.len() {
        panic!(
            "{mode} embedded installer requires all {} mode-specific payload variables",
            payloads.len()
        );
    }

    let mut source = format!(
        "const EMBEDDED: bool = true;\nconst INSTALLER_MODE: &str = {mode:?};\nconst PAYLOADS: &[EmbeddedPayload] = &[\n"
    );
    for (name, variable) in payloads {
        let path = env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{variable} is required with embedded-installer"));
        println!("cargo:rerun-if-changed={}", path.display());
        let compressed = output.join(format!("{name}.zst"));
        compress(&path, &compressed).unwrap_or_else(|error| {
            panic!("failed to embed {} from {}: {error}", name, path.display())
        });
        let digest = sha256(&path).unwrap_or_else(|error| {
            panic!("failed to hash {} from {}: {error}", name, path.display())
        });
        source.push_str(&format!(
            "    EmbeddedPayload {{ name: {name:?}, sha256: {digest:?}, compressed: include_bytes!({path:?}) }},\n",
            path = compressed.to_string_lossy(),
        ));
    }
    source.push_str("];\n");
    fs::write(generated, source).expect("write embedded payload source");
}

fn compress(input: &Path, output: &Path) -> io::Result<()> {
    let mut input = File::open(input)?;
    let output = File::create(output)?;
    let mut encoder = zstd::Encoder::new(output, 15)?;
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

fn sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    let digest = hasher.finalize();
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(result, "{byte:02x}").expect("write to string");
    }
    Ok(result)
}
