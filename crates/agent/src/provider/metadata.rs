use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::SystemTime,
};

use agentdesktop_core::model::Skill;
use memchr::memmem;
use serde::Deserialize;

struct PackageMetadata {
    name: Option<String>,
    version: String,
}

pub(super) fn find_in_path(name: &str) -> Option<PathBuf> {
    find_all_in_path(name).into_iter().next()
}

/// Every match for `name` on `PATH`, in `PATH` order.
///
/// Discovery for a tool with editor forks needs to look past the first hit,
/// because the fork's launcher can shadow the real one.
pub(super) fn find_all_in_path(name: &str) -> Vec<PathBuf> {
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    let extensions = executable_extensions(name);
    env::split_paths(&path)
        .flat_map(|directory| {
            extensions
                .iter()
                .map(|extension| directory.join(format!("{name}{extension}")))
                .collect::<Vec<_>>()
        })
        .filter(|candidate| candidate.is_file())
        .collect()
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

const MAX_WRAPPER_SCRIPT_SIZE: u64 = 4 * 1024;
const MAX_WRAPPER_HOPS: usize = 4;

/// Follows `makeWrapper`-style exec scripts (Nix's generated shebang shims: a
/// script that only sets up the environment and `exec`s a sibling binary) to
/// the real executable they wrap, without executing them.
///
/// A script whose `exec` target is a shell variable rather than a literal
/// path (as in a hand-written launcher) is left unresolved, since the real
/// target can only be determined by actually running the shell.
pub(super) fn resolve_exec_wrapper(executable: &Path) -> PathBuf {
    let mut current = executable.to_path_buf();
    for _ in 0..MAX_WRAPPER_HOPS {
        match exec_wrapper_target(&current) {
            Some(target) if target.is_file() => current = target,
            _ => break,
        }
    }
    current
}

fn exec_wrapper_target(path: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_WRAPPER_SCRIPT_SIZE {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    if !contents.starts_with("#!") {
        return None;
    }
    // Only the last `exec` line counts.
    let last_exec = contents
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("exec "))?;
    exec_target_from_line(last_exec)
}

fn exec_target_from_line(line: &str) -> Option<PathBuf> {
    let rest = line.trim().strip_prefix("exec ")?;
    // `exec -a "$0"` or, with `--argv0`, `exec -a "<name>"`.
    let rest = match rest.strip_prefix("-a ") {
        Some(argv0) => quoted_word(argv0.trim_start())?.1,
        None => rest,
    };
    let (target, _) = quoted_word(rest.trim_start())?;
    let target = Path::new(target);
    target.is_absolute().then(|| target.to_path_buf())
}

/// Leading double-quoted word and the remainder.
fn quoted_word(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix('"')?.split_once('"')
}

const EMBEDDED_VERSION_MAX_LENGTH: usize = 64;

/// Reads a version string embedded verbatim in a binary's bytes, directly after a
/// literal `marker`, without executing it. The first `/`-separated segment that
/// parses as a version wins.
///
/// Install-layout conventions (a `versions/` directory, an npm `package.json`, ...)
/// vary by install method; the marker baked into the binary itself does not.
///
/// Results are cached per `marker` until the file's path, size or mtime changes.
pub(super) fn embedded_version(
    executable: &Path,
    marker: &'static [u8],
    max_binary_size: u64,
) -> Option<String> {
    let path = executable.canonicalize().ok()?;
    let file = fs::File::open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > max_binary_size {
        return None;
    }
    // No mtime: same-size rewrites are undetectable, so never cache.
    let stamp = metadata.modified().ok().map(|modified| FileStamp {
        path,
        len: metadata.len(),
        modified,
    });
    cached_embedded_version(stamp, marker, || {
        embedded_version_from_reader(file, marker, EMBEDDED_VERSION_CHUNK_SIZE)
    })
}

fn cached_embedded_version(
    stamp: Option<FileStamp>,
    marker: &'static [u8],
    scan: impl FnOnce() -> io::Result<Option<String>>,
) -> Option<String> {
    static CACHE: Mutex<EmbeddedVersionCache> = Mutex::new(BTreeMap::new());
    if let Some(stamp) = &stamp {
        let cached = CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(marker)
            .filter(|(cached, _)| cached == stamp)
            .map(|(_, version)| version.clone());
        if let Some(version) = cached {
            return version;
        }
    }
    // Read errors are not cached so the next refresh retries.
    let version = scan().ok()?;
    if let Some(stamp) = stamp {
        CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(marker, (stamp, version.clone()));
    }
    version
}

/// One entry per marker; an upgrade replaces it.
type EmbeddedVersionCache = BTreeMap<&'static [u8], (FileStamp, Option<String>)>;

#[derive(PartialEq)]
struct FileStamp {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

const EMBEDDED_VERSION_CHUNK_SIZE: usize = 64 * 1024;

fn embedded_version_from_reader(
    mut reader: impl Read,
    marker: &[u8],
    chunk_size: usize,
) -> io::Result<Option<String>> {
    let chunk_size = chunk_size.max(1);
    let retained_length = marker.len() + EMBEDDED_VERSION_MAX_LENGTH;
    let mut chunk = vec![0; chunk_size];
    let mut retained = Vec::new();

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let finished = read == 0;
        retained.extend_from_slice(&chunk[..read]);
        if let Some(version) = find_embedded_version(&retained, marker, finished) {
            return Ok(Some(version));
        }
        if finished {
            return Ok(None);
        }
        if retained.len() > retained_length {
            retained.drain(..retained.len() - retained_length);
        }
    }
}

fn find_embedded_version(bytes: &[u8], marker: &[u8], finished: bool) -> Option<String> {
    for start in memmem::find_iter(bytes, marker) {
        let value = &bytes[start + marker.len()..];
        let length = value
            .iter()
            .take(EMBEDDED_VERSION_MAX_LENGTH)
            .take_while(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'/')
            })
            .count();
        if length == value.len() && !finished {
            continue;
        }
        let Ok(token) = std::str::from_utf8(&value[..length]) else {
            continue;
        };
        // e.g. `opencode/latest/2.0.15/cli`.
        if let Some(version) = token
            .split('/')
            .find(|segment| valid_embedded_version(segment))
        {
            return Some(version.to_owned());
        }
    }
    None
}

fn valid_embedded_version(version: &str) -> bool {
    let core = version
        .split_once(['-', '+'])
        .map_or(version, |(core, _)| core);
    let components: Vec<_> = core.split('.').collect();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
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
            // macOS application bundles keep the manifest directly above `bin`.
            candidates.insert(directory.join("../package.json"));
            candidates.insert(directory.join("../../package.json"));
        }
    }
    candidates.into_iter().collect()
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
    use std::{
        fs::File,
        io::{self, Cursor, Read},
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    use super::{
        FileStamp, MAX_WRAPPER_HOPS, cached_embedded_version, embedded_version,
        embedded_version_from_reader, resolve_exec_wrapper, skill_front_matter,
    };

    fn scan(binary: &[u8], marker: &[u8]) -> Option<String> {
        embedded_version_from_reader(Cursor::new(binary), marker, 7).unwrap()
    }

    #[test]
    fn reads_embedded_version_across_chunk_boundaries() {
        let binary = b"prefix --user-agent=opencode/1.18.11 --use-system-ca -- suffix";
        assert_eq!(
            scan(binary, b"user-agent=opencode/").as_deref(),
            Some("1.18.11")
        );
    }

    #[test]
    fn reads_version_after_a_channel_segment() {
        let binary =
            b"--smol --user-agent=opencode/latest/2.0.15/cli --use-system-ca --no-warnings";
        assert_eq!(
            scan(binary, b"user-agent=opencode/").as_deref(),
            Some("2.0.15")
        );
    }

    #[test]
    fn reads_preview_channel_versions() {
        let binary = b"--user-agent=opencode/dev/0.0.0-dev-123/cli --use-system-ca";
        assert_eq!(
            scan(binary, b"user-agent=opencode/").as_deref(),
            Some("0.0.0-dev-123")
        );
    }

    #[test]
    fn skips_marker_hits_that_are_not_versions() {
        let binary = b"// Version: 1.2 // Version: abc // Version: 1.2.3.4 // Version: 2.1.319\n";
        assert_eq!(scan(binary, b"// Version: ").as_deref(), Some("2.1.319"));
    }

    #[test]
    fn accepts_prerelease_and_build_suffixes() {
        let binary = b"// Version: 1.2.3-beta.1+build.7\n";
        assert_eq!(
            scan(binary, b"// Version: ").as_deref(),
            Some("1.2.3-beta.1+build.7")
        );
    }

    #[test]
    fn returns_nothing_without_a_marker() {
        assert_eq!(scan(b"no version information here", b"// Version: "), None);
    }

    #[test]
    fn retries_interrupted_reads() {
        struct InterruptedOnce {
            inner: Cursor<&'static [u8]>,
            interrupted: bool,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.inner.read(buf)
            }
        }

        let reader = InterruptedOnce {
            inner: Cursor::new(b"// Version: 1.0.0\n"),
            interrupted: false,
        };
        assert_eq!(
            embedded_version_from_reader(reader, b"// Version: ", 7)
                .unwrap()
                .as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn ignores_binaries_over_the_size_limit() {
        const MARKER: &[u8] = b"size-limit-marker/";
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tool");
        std::fs::write(&binary, b"size-limit-marker/1.0.0").unwrap();

        assert_eq!(embedded_version(&binary, MARKER, 4), None);
        assert_eq!(
            embedded_version(&binary, MARKER, u64::MAX).as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn does_not_cache_a_failed_scan() {
        const MARKER: &[u8] = b"failed-scan-marker/";
        let stamp = || {
            Some(FileStamp {
                path: PathBuf::from("/tool"),
                len: 1,
                modified: SystemTime::UNIX_EPOCH,
            })
        };

        assert_eq!(
            cached_embedded_version(stamp(), MARKER, || Err(io::Error::other("disk"))),
            None
        );
        // Same stamp rescans.
        assert_eq!(
            cached_embedded_version(stamp(), MARKER, || Ok(Some("1.0.0".to_owned()))).as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn does_not_cache_without_a_stamp() {
        const MARKER: &[u8] = b"no-stamp-marker/";
        assert_eq!(
            cached_embedded_version(None, MARKER, || Ok(Some("1.0.0".to_owned()))).as_deref(),
            Some("1.0.0")
        );
        assert_eq!(
            cached_embedded_version(None, MARKER, || Ok(Some("2.0.0".to_owned()))).as_deref(),
            Some("2.0.0")
        );
    }

    #[test]
    fn follows_make_wrapper_style_exec_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join(".opencode-wrapped");
        std::fs::write(&real, b"binary bytes").unwrap();
        let wrapper = dir.path().join("opencode");
        std::fs::write(
            &wrapper,
            format!(
                "#! /nix/store/.../bash -e\nexec -a \"$0\" \"{}\"  \"$@\" \n",
                real.display()
            ),
        )
        .unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), real);
    }

    #[test]
    fn follows_make_wrapper_scripts_with_a_fixed_argv0() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join(".claude-wrapped");
        std::fs::write(&real, b"binary bytes").unwrap();
        let wrapper = dir.path().join("claude");
        std::fs::write(
            &wrapper,
            format!(
                "#! /nix/store/.../bash -e\nexport DISABLE_AUTOUPDATER='1'\nexec -a \"claude\" \"{}\"  \"$@\" \n",
                real.display()
            ),
        )
        .unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), real);
    }

    #[test]
    fn leaves_relative_exec_targets_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("opencode");
        std::fs::write(&wrapper, b"#!/bin/sh\nexec \"Cargo.toml\" \"$@\"\n").unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), wrapper);
    }

    #[test]
    fn caches_embedded_version_until_the_file_changes() {
        const MARKER: &[u8] = b"cache-test-marker/";
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tool");
        let write = |version: &str, modified: SystemTime| {
            std::fs::write(&binary, format!("head cache-test-marker/{version} tail")).unwrap();
            File::options()
                .write(true)
                .open(&binary)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        };
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        write("1.0.0", first);
        assert_eq!(
            embedded_version(&binary, MARKER, u64::MAX).as_deref(),
            Some("1.0.0")
        );

        // Same size and mtime: cache hit.
        write("2.0.0", first);
        assert_eq!(
            embedded_version(&binary, MARKER, u64::MAX).as_deref(),
            Some("1.0.0")
        );

        write("2.0.0", first + Duration::from_secs(1));
        assert_eq!(
            embedded_version(&binary, MARKER, u64::MAX).as_deref(),
            Some("2.0.0")
        );
    }

    #[test]
    fn leaves_variable_based_exec_targets_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("claude");
        std::fs::write(
            &wrapper,
            b"#!/usr/bin/env bash\nexec \"$CLAUDE_BINARY\" \"$@\"\n",
        )
        .unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), wrapper);
    }

    #[test]
    fn only_the_last_exec_line_counts() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, b"binary bytes").unwrap();
        let wrapper = dir.path().join("claude");
        std::fs::write(
            &wrapper,
            format!(
                "#!/usr/bin/env bash\nif [ -n \"$LEGACY\" ]; then\n  exec \"{}\" \"$@\"\nfi\nexec \"$CLAUDE_BINARY\" \"$@\"\n",
                real.display()
            ),
        )
        .unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), wrapper);
    }

    #[test]
    fn stops_following_wrappers_at_the_hop_limit() {
        let dir = tempfile::tempdir().unwrap();
        let chain: Vec<PathBuf> = (0..=MAX_WRAPPER_HOPS + 1)
            .map(|index| dir.path().join(format!("hop-{index}")))
            .collect();
        for pair in chain.windows(2) {
            std::fs::write(
                &pair[0],
                format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", pair[1].display()),
            )
            .unwrap();
        }
        std::fs::write(chain.last().unwrap(), b"binary bytes").unwrap();

        assert_eq!(resolve_exec_wrapper(&chain[0]), chain[MAX_WRAPPER_HOPS]);
    }

    #[test]
    fn a_wrapper_that_execs_itself_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("loop");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", wrapper.display()),
        )
        .unwrap();

        assert_eq!(resolve_exec_wrapper(&wrapper), wrapper);
    }

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
