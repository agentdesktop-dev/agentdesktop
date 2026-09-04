use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use agentdesktop_core::model::Skill;
use serde::Deserialize;

pub(super) fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    find_in_directories(name, env::split_paths(&path))
}

pub(super) fn find_executable(
    name: &str,
    additional_candidates: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    find_in_path(name).or_else(|| {
        additional_candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

fn find_in_directories(
    name: &str,
    directories: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let extensions = executable_extensions(name);
    directories.into_iter().find_map(|directory| {
        extensions
            .iter()
            .map(|extension| directory.join(format!("{name}{extension}")))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn executable_extensions(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![String::new()];
    }

    let mut extensions = vec![String::new()];
    let path_extensions = env::var_os("PATHEXT")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    extensions.extend(
        path_extensions
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| {
                if extension.starts_with('.') {
                    extension.to_owned()
                } else {
                    format!(".{extension}")
                }
            }),
    );
    extensions
}

#[cfg(unix)]
fn executable_extensions(_name: &str) -> Vec<String> {
    vec![String::new()]
}

pub(super) fn version_after_component(executable: &Path, component: &str) -> Option<String> {
    let resolved = executable.canonicalize().ok()?;
    let mut components = resolved.components();
    while components.next()?.as_os_str() != component {
        // Keep looking for the named install-layout component.
    }
    components.next()?.as_os_str().to_str().map(str::to_owned)
}

pub(super) fn json_package_version(path: &Path, expected_name: &str) -> Option<String> {
    let metadata = json_package_metadata(path)?;
    (metadata.name.as_deref() == Some(expected_name)).then_some(metadata.version)
}

pub(super) fn json_package_name(path: &Path) -> Option<String> {
    json_package_metadata(path)?.name
}

/// `package.json` manifests that may describe the packaged application an
/// executable belongs to.
///
/// Electron editors keep the manifest a fixed number of directories above their
/// launcher, and the launcher on `PATH` is frequently a symlink into the
/// install root.
pub(super) fn packaged_manifest_candidates(executable: &Path) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(directory) = executable.parent() {
            candidates.insert(directory.join("resources/app/package.json"));
            candidates.insert(directory.join("../resources/app/package.json"));
            candidates.insert(directory.join("../../package.json"));
        }
    }
    candidates.into_iter().collect()
}

struct PackageMetadata {
    name: Option<String>,
    version: String,
}

fn json_package_metadata(path: &Path) -> Option<PackageMetadata> {
    #[derive(Deserialize)]
    struct JsonPackageMetadata {
        #[serde(default)]
        name: Option<String>,
        version: String,
    }

    let contents = fs::read(path).ok()?;
    let metadata: JsonPackageMetadata = serde_json::from_slice(&contents).ok()?;
    (!metadata.version.is_empty()).then_some(PackageMetadata {
        name: metadata.name,
        version: metadata.version,
    })
}

/// Reads an Electron application's version from its packaged root `package.json`.
///
/// ASAR stores a JSON file index followed by uncompressed file contents, so this
/// does not execute the application or unpack the archive.
pub(super) fn electron_asar_version(path: &Path, product_name: &str) -> Option<String> {
    const MAX_HEADER_SIZE: u64 = 16 * 1024 * 1024;
    const MAX_PACKAGE_SIZE: u64 = 1024 * 1024;

    let mut archive = fs::File::open(path).ok()?;
    let mut prefix = [0_u8; 8];
    archive.read_exact(&mut prefix).ok()?;
    if u32::from_le_bytes(prefix[..4].try_into().ok()?) != 4 {
        return None;
    }
    let header_size = u32::from_le_bytes(prefix[4..].try_into().ok()?) as u64;
    if !(8..=MAX_HEADER_SIZE).contains(&header_size) {
        return None;
    }

    let mut header = vec![0; usize::try_from(header_size).ok()?];
    archive.read_exact(&mut header).ok()?;
    let json_size = u32::from_le_bytes(header.get(4..8)?.try_into().ok()?) as usize;
    let json = header.get(8..8_usize.checked_add(json_size)?)?;
    let index: serde_json::Value = serde_json::from_slice(json).ok()?;
    let package = index.get("files")?.get("package.json")?;
    let offset = package.get("offset")?.as_str()?.parse::<u64>().ok()?;
    let size = package.get("size")?.as_u64()?;
    if size > MAX_PACKAGE_SIZE {
        return None;
    }

    archive
        .seek(SeekFrom::Start(
            8_u64.checked_add(header_size)?.checked_add(offset)?,
        ))
        .ok()?;
    let mut contents = vec![0; usize::try_from(size).ok()?];
    archive.read_exact(&mut contents).ok()?;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PackageMetadata {
        product_name: String,
        version: String,
    }

    let metadata: PackageMetadata = serde_json::from_slice(&contents).ok()?;
    (metadata.product_name == product_name && !metadata.version.is_empty())
        .then_some(metadata.version)
}

pub(super) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = env_path("USERPROFILE") {
            return Some(profile);
        }
        let drive = env::var_os("HOMEDRIVE")?;
        let path = env::var_os("HOMEPATH")?;
        Some(PathBuf::from(drive).join(path))
    }

    #[cfg(unix)]
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
pub(super) fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Home directories that may contain per-user developer-tool configuration.
///
/// The daemon commonly runs as root or in a container, so its own `HOME` is
/// not necessarily the home of the users whose tools it discovers.
pub(super) fn user_home_dirs() -> Vec<PathBuf> {
    let mut homes = BTreeSet::new();
    homes.extend(home_dir());

    #[cfg(unix)]
    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let fields: Vec<_> = line.split(':').collect();
            let Some(home) = fields.get(5).filter(|home| home.starts_with('/')) else {
                continue;
            };
            let path = PathBuf::from(home);
            if path.is_dir() {
                homes.insert(path);
            }
        }
    }

    #[cfg(target_os = "linux")]
    let parents = vec![PathBuf::from("/home")];
    #[cfg(target_os = "macos")]
    let parents = vec![PathBuf::from("/Users")];
    #[cfg(windows)]
    let parents: Vec<PathBuf> = home_dir()
        .and_then(|home| home.parent().map(Path::to_path_buf))
        .into_iter()
        .chain(env_path("SystemDrive").map(|drive| drive.join("Users")))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for parent in &parents {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        homes.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }

    homes.into_iter().collect()
}

pub(super) fn current_dir_ancestors(relative: &Path) -> Vec<PathBuf> {
    let Ok(current) = env::current_dir() else {
        return Vec::new();
    };
    current
        .ancestors()
        .map(|directory| directory.join(relative))
        .collect()
}

pub(super) fn discover_skills(roots: impl IntoIterator<Item = PathBuf>) -> Vec<Skill> {
    let mut files = BTreeSet::new();
    let mut visited_directories = BTreeSet::new();
    for root in roots {
        collect_skill_files(&root, &mut files, &mut visited_directories);
    }

    files
        .into_iter()
        .filter_map(|path| {
            let contents = fs::read_to_string(&path).ok()?;
            let front_matter = skill_front_matter(&contents)?;
            Some(Skill { path, front_matter })
        })
        .collect()
}

fn collect_skill_files(
    path: &Path,
    files: &mut BTreeSet<PathBuf>,
    visited_directories: &mut BTreeSet<PathBuf>,
) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if path.file_name().is_some_and(|name| name == "SKILL.md") {
            files.insert(path.to_path_buf());
        }
        return;
    }
    let Ok(canonical) = path.canonicalize() else {
        return;
    };
    if !visited_directories.insert(canonical) {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect_skill_files(&entry.path(), files, visited_directories);
    }
}

fn skill_front_matter(contents: &str) -> Option<BTreeMap<String, serde_json::Value>> {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let rest = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\r\n---\r\n"))
        .or_else(|| rest.strip_suffix("\n---").map(str::len))
        .or_else(|| rest.strip_suffix("\r\n---").map(str::len))?;
    let yaml = &rest[..end];
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let json = serde_json::to_value(value).ok()?;
    serde_json::from_value(json).ok()
}

#[cfg(test)]
mod tests {
    use super::skill_front_matter;

    #[test]
    fn reads_only_skill_front_matter() {
        let front_matter = skill_front_matter(
            "---\nname: deploy\ndescription: Deploy safely\nallowed-tools:\n  - Bash\n---\nIgnore this body",
        )
        .expect("valid front matter");

        assert_eq!(front_matter["name"], "deploy");
        assert_eq!(front_matter["description"], "Deploy safely");
        assert_eq!(front_matter["allowed-tools"][0], "Bash");
        assert!(!front_matter.contains_key("body"));
    }
}
