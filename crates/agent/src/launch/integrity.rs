//! Executable-configuration integrity: snapshot the read-only granted files
//! of a launch before its box spawns, verify them while the box runs and when
//! it exits, and report every change. Alert-first: nothing is restored or
//! blocked here.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests;

/// File identity across platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume: u64,
    pub file_id: u128,
}

/// Entry kind with file size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File { len: u64 },
    Folder,
    Link,
}

/// Snapshot of a file or folder at a point in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileSnapshot {
    Absent,
    File {
        sha256: [u8; 32],
        identity: FileIdentity,
    },
    Folder {
        sha256: [u8; 32],
        identity: FileIdentity,
    },
    FolderIdentity {
        identity: FileIdentity,
    },
    Link {
        target_sha256: [u8; 32],
        identity: FileIdentity,
    },
    /// Over the content budget: names, kinds, sizes and identities are
    /// hashed recursively, file contents are not read.
    TooLarge {
        identity: FileIdentity,
        structure_sha256: [u8; 32],
    },
    Unreadable {
        kind: io::ErrorKind,
    },
}

/// Mode for integrity root checking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootMode {
    Recursive,
    IdentityOnly,
}

/// An integrity check root path with its mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityRoot {
    pub path: PathBuf,
    pub mode: RootMode,
}

/// Kind of change detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Replaced,
    Appeared,
    Disappeared,
    KindChanged,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Modified => write!(f, "modified"),
            ChangeKind::Replaced => write!(f, "replaced"),
            ChangeKind::Appeared => write!(f, "appeared"),
            ChangeKind::Disappeared => write!(f, "disappeared"),
            ChangeKind::KindChanged => write!(f, "kind_changed"),
        }
    }
}

/// A detected change between snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub before: FileSnapshot,
    pub after: FileSnapshot,
}

/// Result of verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyResult {
    pub changed: Vec<(PathBuf, Change)>,
}

/// Filesystem operations for integrity checking.
pub trait IntegrityFs: Send + Sync {
    fn metadata(&self, p: &Path) -> io::Result<EntryKind>;
    fn read_bytes(&self, p: &Path) -> io::Result<Vec<u8>>;
    fn read_dir_names(&self, p: &Path) -> io::Result<Vec<OsString>>;
    fn file_identity(&self, p: &Path) -> io::Result<FileIdentity>;
    fn read_link(&self, p: &Path) -> io::Result<OsString>;
}

/// Platform encoding of a file name or link target, as the spec fixes it:
/// `OsStr::as_encoded_bytes` (stable, deterministic per platform).
fn os_bytes(s: &std::ffi::OsStr) -> &[u8] {
    s.as_encoded_bytes()
}

/// Real filesystem implementation.
pub struct RealIntegrityFs;

pub const REAL_FS: RealIntegrityFs = RealIntegrityFs;

const MAX_ENTRIES: usize = 1000;
const MAX_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
/// Entry cap for the structure-only walk of an over-budget tree.
const STRUCTURE_MAX_ENTRIES: usize = 10_000;
/// Maximum nesting depth for recursive folder walks. This prevents stack
/// overflow on the 2 MiB blocking-thread stack (found by external review).
const MAX_DEPTH: usize = 64;

impl IntegrityFs for RealIntegrityFs {
    fn metadata(&self, p: &Path) -> io::Result<EntryKind> {
        let meta = std::fs::symlink_metadata(p)?;
        if meta.is_symlink() {
            Ok(EntryKind::Link)
        } else if meta.is_dir() {
            Ok(EntryKind::Folder)
        } else {
            Ok(EntryKind::File { len: meta.len() })
        }
    }

    fn read_bytes(&self, p: &Path) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::open(p)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn read_dir_names(&self, p: &Path) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(p)? {
            let entry = entry?;
            names.push(entry.file_name());
        }
        Ok(names)
    }

    fn file_identity(&self, p: &Path) -> io::Result<FileIdentity> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
                GetFileInformationByHandleEx,
            };

            // Attribute-only access: it cannot hit a sharing violation on a
            // file another process holds open exclusively.
            const FILE_READ_ATTRIBUTES: u32 = 0x0080;
            let file = std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(p)?;

            // SAFETY: FILE_ID_INFO is a plain-old-data struct; all-zero bytes
            // are a valid initial value for it.
            let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
            // SAFETY: the handle is open, the buffer is a FILE_ID_INFO and
            // the size passed is exactly its size, as the API requires.
            let success = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle() as _,
                    FileIdInfo,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of::<FILE_ID_INFO>() as u32,
                )
            };

            if success == 0 {
                return Err(io::Error::last_os_error());
            }

            let file_id = u128::from_le_bytes(info.FileId.Identifier);
            let volume = info.VolumeSerialNumber as u64;

            Ok(FileIdentity { volume, file_id })
        }

        #[cfg(not(windows))]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::symlink_metadata(p)?;
            let volume = meta.dev();
            let file_id = meta.ino() as u128;
            Ok(FileIdentity { volume, file_id })
        }
    }

    fn read_link(&self, p: &Path) -> io::Result<OsString> {
        std::fs::read_link(p).map(|pb| pb.into_os_string())
    }
}

/// Snapshot the given roots.
pub fn snapshot(roots: &[IntegrityRoot], fs: &dyn IntegrityFs) -> BTreeMap<PathBuf, FileSnapshot> {
    let mut result = BTreeMap::new();
    for root in roots {
        let snap = snapshot_one(&root.path, root.mode, fs);
        result.insert(root.path.clone(), snap);
    }
    result
}

fn snapshot_one(path: &Path, mode: RootMode, fs: &dyn IntegrityFs) -> FileSnapshot {
    // One retry after 100 ms covers transient locks on any read the attempt
    // makes, not only the first metadata call.
    match snapshot_one_attempt(path, mode, fs) {
        FileSnapshot::Unreadable { .. } => {
            std::thread::sleep(Duration::from_millis(100));
            snapshot_one_attempt(path, mode, fs)
        }
        snap => snap,
    }
}

fn snapshot_one_attempt(path: &Path, mode: RootMode, fs: &dyn IntegrityFs) -> FileSnapshot {
    let kind = match fs.metadata(path) {
        Ok(k) => k,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return FileSnapshot::Absent,
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => return FileSnapshot::Absent,
        Err(e) => return FileSnapshot::Unreadable { kind: e.kind() },
    };

    match kind {
        EntryKind::File { len } => {
            let identity = match fs.file_identity(path) {
                Ok(id) => id,
                Err(e) => return FileSnapshot::Unreadable { kind: e.kind() },
            };
            if len > MAX_BYTES {
                // A root file over the budget: its size stands in for content.
                return FileSnapshot::TooLarge {
                    identity,
                    structure_sha256: sha256_hash(&len.to_le_bytes()),
                };
            }
            let bytes = match fs.read_bytes(path) {
                Ok(b) => b,
                Err(e) => return FileSnapshot::Unreadable { kind: e.kind() },
            };
            if bytes.len() as u64 > MAX_BYTES {
                // The file grew between the size check and the read.
                return FileSnapshot::TooLarge {
                    identity,
                    structure_sha256: sha256_hash(&(bytes.len() as u64).to_le_bytes()),
                };
            }
            let sha256 = sha256_hash(&bytes);
            FileSnapshot::File { sha256, identity }
        }
        EntryKind::Folder => {
            let identity = match fs.file_identity(path) {
                Ok(id) => id,
                Err(e) => return FileSnapshot::Unreadable { kind: e.kind() },
            };
            if mode == RootMode::IdentityOnly {
                return FileSnapshot::FolderIdentity { identity };
            }
            // Recursive mode
            let mut entries_count = 0;
            let mut cumulative_bytes = 0u64;
            match snapshot_folder_recursive(path, fs, &mut entries_count, &mut cumulative_bytes, 0)
            {
                Ok(sha256) => FileSnapshot::Folder { sha256, identity },
                Err(FolderError::TooLarge) => {
                    // Over the content budget: hash the structure instead
                    // (names, kinds, sizes, identities), so a changed manifest
                    // or an atomic save still shows up. In-place edits that
                    // keep a file's size do not.
                    let mut entries = 0usize;
                    let structure_sha256 = match structure_hash(path, fs, &mut entries, 0) {
                        Ok(hash) => hash,
                        Err(FolderError::TooLarge) => match fs.read_dir_names(path) {
                            Ok(names) => hash_dir_names(&names),
                            Err(_) => [0; 32],
                        },
                        Err(FolderError::Unreadable(kind)) => {
                            return FileSnapshot::Unreadable { kind };
                        }
                    };
                    FileSnapshot::TooLarge {
                        identity,
                        structure_sha256,
                    }
                }
                Err(FolderError::Unreadable(kind)) => FileSnapshot::Unreadable { kind },
            }
        }
        EntryKind::Link => {
            let identity = match fs.file_identity(path) {
                Ok(id) => id,
                Err(e) => return FileSnapshot::Unreadable { kind: e.kind() },
            };
            let target_sha256 = match fs.read_link(path) {
                Ok(target) => sha256_hash(os_bytes(&target)),
                Err(_) => [0; 32],
            };
            FileSnapshot::Link {
                target_sha256,
                identity,
            }
        }
    }
}

enum FolderError {
    TooLarge,
    Unreadable(io::ErrorKind),
}

fn snapshot_folder_recursive(
    path: &Path,
    fs: &dyn IntegrityFs,
    entries_count: &mut usize,
    cumulative_bytes: &mut u64,
    depth: usize,
) -> Result<[u8; 32], FolderError> {
    if depth > MAX_DEPTH {
        return Err(FolderError::TooLarge);
    }

    let names = fs
        .read_dir_names(path)
        .map_err(|e| FolderError::Unreadable(e.kind()))?;

    let mut children: Vec<(OsString, u8, [u8; 32], Option<FileIdentity>)> = Vec::new();

    for name in names {
        // Each entry counts once, whatever its kind.
        *entries_count += 1;
        if *entries_count > MAX_ENTRIES {
            return Err(FolderError::TooLarge);
        }

        let child_path = path.join(&name);
        let kind = match fs.metadata(&child_path) {
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // removed mid-walk
            Err(e) => return Err(FolderError::Unreadable(e.kind())),
        };

        let (kind_byte, hash) = match kind {
            EntryKind::File { len } => {
                if *cumulative_bytes + len > MAX_BYTES {
                    return Err(FolderError::TooLarge);
                }
                let bytes = fs
                    .read_bytes(&child_path)
                    .map_err(|e| FolderError::Unreadable(e.kind()))?;
                *cumulative_bytes += bytes.len() as u64;
                if *cumulative_bytes > MAX_BYTES {
                    // The file grew between the size check and the read.
                    return Err(FolderError::TooLarge);
                }
                (0u8, sha256_hash(&bytes))
            }
            EntryKind::Folder => {
                let hash = snapshot_folder_recursive(
                    &child_path,
                    fs,
                    entries_count,
                    cumulative_bytes,
                    depth + 1,
                )?;
                (1u8, hash)
            }
            EntryKind::Link => {
                let target_hash = match fs.read_link(&child_path) {
                    Ok(target) => sha256_hash(os_bytes(&target)),
                    Err(_) => [0; 32],
                };
                (2u8, target_hash)
            }
        };

        let identity = fs.file_identity(&child_path).ok();
        children.push((name, kind_byte, hash, identity));
    }

    children.sort_by(|a, b| os_bytes(&a.0).cmp(os_bytes(&b.0)));

    // Build the hash from sorted children
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;

    for (name, kind_byte, hash, identity) in children {
        let name_bytes = os_bytes(&name);
        hasher.update((name_bytes.len() as u32).to_le_bytes());
        hasher.update(name_bytes);
        hasher.update([kind_byte]);
        hasher.update(hash);
        if let Some(id) = identity {
            hasher.update([1u8]); // presence marker
            hasher.update(id.volume.to_le_bytes());
            hasher.update(id.file_id.to_le_bytes());
        } else {
            hasher.update([0u8]); // absence marker
        }
    }

    Ok(hasher.finalize().into())
}

/// Hash of an over-budget tree's structure: sorted names with kind, size and
/// identity per entry, recursing into subfolders, no file contents.
fn structure_hash(
    path: &Path,
    fs: &dyn IntegrityFs,
    entries_count: &mut usize,
    depth: usize,
) -> Result<[u8; 32], FolderError> {
    if depth > MAX_DEPTH {
        return Err(FolderError::TooLarge);
    }

    use sha2::Digest;
    let names = fs
        .read_dir_names(path)
        .map_err(|e| FolderError::Unreadable(e.kind()))?;
    let mut sorted = names;
    sorted.sort_by(|a, b| os_bytes(a).cmp(os_bytes(b)));

    let mut hasher = sha2::Sha256::new();
    for name in sorted {
        *entries_count += 1;
        if *entries_count > STRUCTURE_MAX_ENTRIES {
            return Err(FolderError::TooLarge);
        }
        let child_path = path.join(&name);
        let name_bytes = os_bytes(&name);
        hasher.update((name_bytes.len() as u32).to_le_bytes());
        hasher.update(name_bytes);
        match fs.metadata(&child_path) {
            Ok(EntryKind::File { len }) => {
                hasher.update([0u8]);
                hasher.update(len.to_le_bytes());
                if let Ok(identity) = fs.file_identity(&child_path) {
                    hasher.update([1u8]); // presence marker
                    hasher.update(identity.volume.to_le_bytes());
                    hasher.update(identity.file_id.to_le_bytes());
                } else {
                    hasher.update([0u8]); // absence marker
                }
            }
            Ok(EntryKind::Folder) => {
                hasher.update([1u8]);
                let child_hash = structure_hash(&child_path, fs, entries_count, depth + 1)?;
                hasher.update(child_hash);
                if let Ok(identity) = fs.file_identity(&child_path) {
                    hasher.update([1u8]); // presence marker
                    hasher.update(identity.volume.to_le_bytes());
                    hasher.update(identity.file_id.to_le_bytes());
                } else {
                    hasher.update([0u8]); // absence marker
                }
            }
            Ok(EntryKind::Link) => {
                hasher.update([2u8]);
                if let Ok(target) = fs.read_link(&child_path) {
                    hasher.update(sha256_hash(os_bytes(&target)));
                }
                if let Ok(identity) = fs.file_identity(&child_path) {
                    hasher.update([1u8]); // presence marker
                    hasher.update(identity.volume.to_le_bytes());
                    hasher.update(identity.file_id.to_le_bytes());
                } else {
                    hasher.update([0u8]); // absence marker
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                hasher.update([3u8]); // removed mid-walk
            }
            Err(e) => return Err(FolderError::Unreadable(e.kind())),
        }
    }
    Ok(hasher.finalize().into())
}

fn hash_dir_names(names: &[OsString]) -> [u8; 32] {
    use sha2::Digest;
    let mut sorted = names.to_vec();
    sorted.sort_by(|a, b| os_bytes(a).cmp(os_bytes(b)));
    let mut hasher = sha2::Sha256::new();
    for name in sorted {
        let name_bytes = os_bytes(&name);
        hasher.update((name_bytes.len() as u32).to_le_bytes());
        hasher.update(name_bytes);
    }
    hasher.finalize().into()
}

fn sha256_hash(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// Compute the difference between two snapshots.
pub fn diff(before: &FileSnapshot, after: &FileSnapshot) -> Option<Change> {
    use FileSnapshot::*;

    match (before, after) {
        (Absent, Absent) => None,
        (Absent, _) => Some(Change {
            kind: ChangeKind::Appeared,
            before: before.clone(),
            after: after.clone(),
        }),
        (_, Absent) => Some(Change {
            kind: ChangeKind::Disappeared,
            before: before.clone(),
            after: after.clone(),
        }),
        (
            File {
                identity: id1,
                sha256: h1,
            },
            File {
                identity: id2,
                sha256: h2,
            },
        ) => {
            if id1 != id2 {
                Some(Change {
                    kind: ChangeKind::Replaced,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else if h1 != h2 {
                Some(Change {
                    kind: ChangeKind::Modified,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        (
            Folder {
                identity: id1,
                sha256: h1,
            },
            Folder {
                identity: id2,
                sha256: h2,
            },
        ) => {
            if id1 != id2 {
                Some(Change {
                    kind: ChangeKind::Replaced,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else if h1 != h2 {
                Some(Change {
                    kind: ChangeKind::Modified,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        (FolderIdentity { identity: id1 }, FolderIdentity { identity: id2 }) => {
            if id1 != id2 {
                Some(Change {
                    kind: ChangeKind::Replaced,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        (
            Link {
                identity: id1,
                target_sha256: h1,
            },
            Link {
                identity: id2,
                target_sha256: h2,
            },
        ) => {
            if id1 != id2 {
                Some(Change {
                    kind: ChangeKind::Replaced,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else if h1 != h2 {
                Some(Change {
                    kind: ChangeKind::Modified,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        (
            TooLarge {
                identity: id1,
                structure_sha256: h1,
            },
            TooLarge {
                identity: id2,
                structure_sha256: h2,
            },
        ) => {
            if id1 != id2 {
                Some(Change {
                    kind: ChangeKind::Replaced,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else if h1 != h2 {
                Some(Change {
                    kind: ChangeKind::Modified,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        (Unreadable { kind: k1 }, Unreadable { kind: k2 }) => {
            if k1 != k2 {
                Some(Change {
                    kind: ChangeKind::KindChanged,
                    before: before.clone(),
                    after: after.clone(),
                })
            } else {
                None
            }
        }
        _ => Some(Change {
            kind: ChangeKind::KindChanged,
            before: before.clone(),
            after: after.clone(),
        }),
    }
}

/// Verify the current state of the roots against an expected snapshot map.
/// Each root keeps its declared mode, so a root that was absent when the
/// expectation was taken is still checked in that mode once it appears.
pub fn verify(
    roots: &[IntegrityRoot],
    expected: &BTreeMap<PathBuf, FileSnapshot>,
    fs: &dyn IntegrityFs,
) -> VerifyResult {
    let mut changed = Vec::new();
    for root in roots {
        let before = expected
            .get(&root.path)
            .cloned()
            .unwrap_or(FileSnapshot::Absent);
        let after = snapshot_one(&root.path, root.mode, fs);
        if let Some(change) = diff(&before, &after) {
            changed.push((root.path.clone(), change));
        }
    }
    VerifyResult { changed }
}

/// Return the list of executable-config paths to check.
/// Every executable-config entry of the launch profile, plus workspace
/// `.claude` and `.git` (identity only).
pub fn integrity_paths(home: &Path, cwd: &Path) -> Vec<IntegrityRoot> {
    let profile = &super::profile::CLAUDE_CODE;
    let claude_dir = home.join(".claude");
    let workspace_claude = cwd.join(".claude");
    let git = cwd.join(".git");
    // The workspace entries interleave with the two identity-only folders in
    // the order the watcher has always reported them.
    let mut roots: Vec<IntegrityRoot> = profile
        .home_executable_config
        .iter()
        .map(|entry| IntegrityRoot {
            path: claude_dir.join(entry),
            mode: RootMode::Recursive,
        })
        .collect();
    roots.push(IntegrityRoot {
        path: workspace_claude.clone(),
        mode: RootMode::IdentityOnly,
    });
    roots.extend(
        profile
            .workspace_claude_entries
            .iter()
            .map(|entry| IntegrityRoot {
                path: workspace_claude.join(entry),
                mode: RootMode::Recursive,
            }),
    );
    roots.push(IntegrityRoot {
        path: cwd.join(profile.mcp_json),
        mode: RootMode::Recursive,
    });
    roots.push(IntegrityRoot {
        path: git.clone(),
        mode: RootMode::IdentityOnly,
    });
    roots.push(IntegrityRoot {
        path: cwd.join(profile.workspace_head),
        mode: RootMode::Recursive,
    });
    roots.extend(
        profile
            .git_executable_config
            .iter()
            .map(|entry| IntegrityRoot {
                path: git.join(entry),
                mode: RootMode::Recursive,
            }),
    );
    roots
}

/// Verifies the roots every `period` while the launch runs. Each pass is
/// compared with the last observed state, so every transition is reported
/// once: a change, the revert of that change, and the same change made again
/// each produce one `on_drift` call.
pub async fn run_periodic(
    roots: Vec<IntegrityRoot>,
    baseline: BTreeMap<PathBuf, FileSnapshot>,
    fs: Arc<dyn IntegrityFs>,
    period: Duration,
    on_drift: Arc<dyn Fn(PathBuf, Change) + Send + Sync>,
) {
    let mut last_observed = baseline;

    // A fixed schedule: the first check is one period after the task starts,
    // and later checks stay on that schedule regardless of how long a verify
    // pass takes. Missed ticks are skipped, so after a long suspend one check
    // runs instead of a catch-up burst.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let roots_clone = roots.clone();
        let fs_clone = fs.clone();
        let current =
            match tokio::task::spawn_blocking(move || snapshot(&roots_clone, fs_clone.as_ref()))
                .await
            {
                Ok(current) => current,
                Err(error) => {
                    tracing::warn!(%error, "periodic integrity check failed; retrying next period");
                    continue;
                }
            };

        for (path, after) in &current {
            let before = last_observed.get(path).unwrap_or(&FileSnapshot::Absent);
            if let Some(change) = diff(before, after) {
                on_drift(path.clone(), change);
            }
        }
        last_observed = current;
    }
}

/// PR 6a, D12: the integrity roots of a launch of `profile`: the D9 union at
/// the workspace, the D5 executable-config entries of the harness home, and
/// Claude's `~\.claude` entries for Claude. Test-first scaffolding; the writer
/// may fold it into `integrity_paths` (compile-only test edit, ledgered).
#[allow(dead_code)] // test-first scaffolding
pub(crate) fn integrity_paths_for(
    _profile: &super::profile::ProgramProfile,
    _home: &Path,
    _cwd: &Path,
) -> Vec<IntegrityRoot> {
    todo!("PR 6a: integrity roots per profile (D12)")
}
