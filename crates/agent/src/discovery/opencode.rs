use std::{collections::BTreeSet, fs::File, io::Read, path::Path};

use agentdesktop_core::model::Agent;
use memchr::memmem;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("opencode")?;
    Some(Agent {
        version: embedded_version(&executable),
        executable,
        kind: "opencode".to_owned(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
    })
}

const VERSION_MARKER: &[u8] = b"user-agent=opencode/";
const MAX_VERSION_LENGTH: usize = 64;
const MAX_BINARY_SIZE: u64 = 512 * 1024 * 1024;

/// Reads OpenCode's embedded user-agent marker without executing the binary.
fn embedded_version(executable: &Path) -> Option<String> {
    let file = File::open(executable).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BINARY_SIZE {
        return None;
    }
    embedded_version_from_reader(file, 64 * 1024)
}

fn embedded_version_from_reader(mut reader: impl Read, chunk_size: usize) -> Option<String> {
    let chunk_size = chunk_size.max(1);
    let retained_length = VERSION_MARKER.len() + MAX_VERSION_LENGTH;
    let mut chunk = vec![0; chunk_size];
    let mut retained = Vec::new();
    let mut versions = BTreeSet::new();

    loop {
        let read = reader.read(&mut chunk).ok()?;
        let finished = read == 0;
        retained.extend_from_slice(&chunk[..read]);
        collect_versions(&retained, finished, &mut versions);
        if versions.len() > 1 {
            return None;
        }
        if finished {
            break;
        }
        if retained.len() > retained_length {
            retained.drain(..retained.len() - retained_length);
        }
    }

    versions.into_iter().next()
}

fn collect_versions(bytes: &[u8], finished: bool, versions: &mut BTreeSet<String>) {
    for start in memmem::find_iter(bytes, VERSION_MARKER) {
        let value = &bytes[start + VERSION_MARKER.len()..];
        let length = value
            .iter()
            .take(MAX_VERSION_LENGTH)
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            .count();
        if length == value.len() && !finished {
            continue;
        }
        let Ok(version) = std::str::from_utf8(&value[..length]) else {
            continue;
        };
        if valid_version(version) {
            versions.insert(version.to_owned());
        }
    }
}

fn valid_version(version: &str) -> bool {
    let core = version
        .split_once(['-', '+'])
        .map_or(version, |(core, _)| core);
    let components: Vec<_> = core.split('.').collect();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::embedded_version_from_reader;

    #[test]
    fn reads_version_across_chunk_boundaries() {
        let binary = b"prefix --user-agent=opencode/1.18.11 --use-system-ca -- suffix";
        assert_eq!(
            embedded_version_from_reader(Cursor::new(binary), 7).as_deref(),
            Some("1.18.11")
        );
    }

    #[test]
    fn rejects_conflicting_embedded_versions() {
        let binary = b"user-agent=opencode/1.18.11 user-agent=opencode/1.19.0 ";
        assert_eq!(embedded_version_from_reader(Cursor::new(binary), 9), None);
    }

    #[test]
    fn ignores_unrelated_versions() {
        let binary = b"dependency/9.8.7 and some other text";
        assert_eq!(embedded_version_from_reader(Cursor::new(binary), 8), None);
    }
}
