mod evidence;

use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use agentdesktop_core::model::{AccessCoverage, AccessCoverageStatus, AccessSourceKind};
use serde_json::Value;

use self::evidence::{ObservationCollector, visit_value, workspace_from_record};
pub(super) use self::evidence::{RuntimeCollector, permission_mode, runtime_capability};
use super::{CollectedAccess, claude_code, claude_desktop, codex, plural_suffix, vscode};

const MAX_HISTORY_FILES: usize = 96;
const MAX_HISTORY_ENTRIES: usize = 4096;
const MAX_HISTORY_OBSERVATIONS: usize = 1000;
const MAX_HISTORY_RUNTIME_CAPABILITIES: usize = 1000;
const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HISTORY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HISTORY_LINE_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct HistoryAdapter {
    pub kind: &'static str,
    pub root: PathBuf,
    pub include_file: Option<fn(&Path) -> bool>,
    pub workspace_for_file: Option<fn(&Path) -> Option<PathBuf>>,
    pub inspect_runtime: fn(&serde_json::Map<String, Value>, Option<&Path>, &mut RuntimeCollector),
    pub coverage_limitation: Option<&'static str>,
}

pub(super) fn inspect(kind: &str, home: &Path) -> CollectedAccess {
    let Some(adapter) = history_adapter(kind, home) else {
        return coverage_only(
            AccessCoverageStatus::Unsupported,
            &format!("No structured history adapter is available for {kind}"),
        );
    };
    if !adapter.root.is_dir() {
        return coverage_only(
            AccessCoverageStatus::Unavailable,
            &format!("No local {} history store was found", adapter.kind),
        );
    }

    let (candidates, traversal) = history_candidates(&adapter);
    let total_files = candidates.len();
    let mut selected = Vec::new();
    let mut selected_bytes = 0_u64;
    for candidate in candidates {
        if selected.len() >= MAX_HISTORY_FILES
            || candidate.bytes > MAX_HISTORY_FILE_BYTES
            || selected_bytes.saturating_add(candidate.bytes) > MAX_HISTORY_BYTES
        {
            continue;
        }
        selected_bytes += candidate.bytes;
        selected.push(candidate);
    }

    let mut observations = ObservationCollector::default();
    let mut runtime = RuntimeCollector::default();
    let mut parsed_files = 0;
    let mut incomplete_files = 0;
    let mut bytes_read = 0;
    let mut byte_limit_reached = false;
    for candidate in &selected {
        let remaining = MAX_HISTORY_BYTES.saturating_sub(bytes_read);
        if remaining == 0 {
            byte_limit_reached = true;
            break;
        }
        let result = scan_history_file(
            &adapter,
            candidate,
            remaining,
            &mut observations,
            &mut runtime,
        );
        bytes_read = bytes_read.saturating_add(result.bytes_read);
        if result.byte_limit_reached && remaining <= MAX_HISTORY_FILE_BYTES {
            byte_limit_reached = true;
        }
        if result.parsed {
            parsed_files += 1;
        }
        if result.incomplete {
            incomplete_files += 1;
        }
    }
    let (observations, observations_limited) = observations.finish();
    let (runtime_capabilities, runtime_limited) = runtime.finish();
    let mut collected = CollectedAccess {
        observations,
        capabilities: runtime_capabilities,
        ..CollectedAccess::default()
    };
    let complete = !traversal.limited
        && !traversal.unreadable
        && !byte_limit_reached
        && incomplete_files == 0
        && !observations_limited
        && !runtime_limited
        && parsed_files == total_files
        && adapter.coverage_limitation.is_none();
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::History,
        status: if complete {
            AccessCoverageStatus::Complete
        } else {
            AccessCoverageStatus::Partial
        },
        detail: if let Some(limitation) = adapter.coverage_limitation {
            format!(
                "Inspected {parsed_files} local session record{}; {limitation}",
                plural_suffix(parsed_files),
            )
        } else if traversal.unreadable {
            format!(
                "Inspected {parsed_files} session history file{}; some directories were unreadable or unsafe to traverse",
                plural_suffix(parsed_files)
            )
        } else if traversal.limited {
            format!(
                "Inspected {parsed_files} session history file{}; discovery stopped after {MAX_HISTORY_ENTRIES} filesystem entries",
                plural_suffix(parsed_files)
            )
        } else if byte_limit_reached {
            format!(
                "Inspected {parsed_files} session history file{} before reaching the {} MiB read limit",
                plural_suffix(parsed_files),
                MAX_HISTORY_BYTES / 1024 / 1024
            )
        } else if runtime_limited {
            format!(
                "Inspected {parsed_files} session history file{}; returned the first {MAX_HISTORY_RUNTIME_CAPABILITIES} unique recorded controls",
                plural_suffix(parsed_files)
            )
        } else if observations_limited {
            format!(
                "Inspected {parsed_files} session history file{}; returned the first {MAX_HISTORY_OBSERVATIONS} unique resources encountered in newest-first files",
                plural_suffix(parsed_files)
            )
        } else if incomplete_files > 0 {
            format!(
                "Inspected {parsed_files} session history file{}; {incomplete_files} contained malformed or oversized records",
                plural_suffix(parsed_files)
            )
        } else if complete {
            format!(
                "Inspected {parsed_files} session history file{}",
                plural_suffix(parsed_files)
            )
        } else {
            format!(
                "Inspected {parsed_files} of {total_files} session history file{}, bounded to {} MiB",
                plural_suffix(total_files),
                MAX_HISTORY_BYTES / 1024 / 1024
            )
        },
    });
    collected
}

fn coverage_only(status: AccessCoverageStatus, detail: &str) -> CollectedAccess {
    let mut collected = CollectedAccess::default();
    collected.coverage.push(AccessCoverage {
        source: AccessSourceKind::History,
        status,
        detail: detail.to_owned(),
    });
    collected
}

fn history_adapter(kind: &str, home: &Path) -> Option<HistoryAdapter> {
    match kind {
        "vscode" => Some(vscode::history_adapter(home)),
        "claude-code" => Some(claude_code::history_adapter(home)),
        "claude-desktop" => claude_desktop::history_adapter(home),
        "codex" => Some(codex::history_adapter(home)),
        _ => None,
    }
}

fn history_candidates(adapter: &HistoryAdapter) -> (Vec<HistoryCandidate>, HistoryTraversal) {
    let mut files = Vec::new();
    let mut entries_seen = 0;
    let mut traversal = HistoryTraversal::default();
    let root_is_directory =
        fs::symlink_metadata(&adapter.root).is_ok_and(|metadata| metadata.file_type().is_dir());
    if root_is_directory {
        collect_history_files(
            &adapter.root,
            0,
            &mut entries_seen,
            MAX_HISTORY_ENTRIES,
            &mut files,
            &mut traversal,
        );
    } else {
        traversal.unreadable = true;
    }
    if let Some(include_file) = adapter.include_file {
        files.retain(|candidate| include_file(&candidate.path));
    }
    if let Some(workspace_for_file) = adapter.workspace_for_file {
        for candidate in &mut files {
            candidate.workspace = workspace_for_file(&candidate.path);
        }
    }
    files.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    (files, traversal)
}

#[derive(Default)]
struct HistoryTraversal {
    limited: bool,
    unreadable: bool,
}

fn collect_history_files(
    path: &Path,
    depth: usize,
    entries_seen: &mut usize,
    entry_limit: usize,
    files: &mut Vec<HistoryCandidate>,
    traversal: &mut HistoryTraversal,
) {
    if depth > 5 {
        traversal.limited = true;
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        traversal.unreadable = true;
        return;
    };
    for entry in entries.flatten() {
        if *entries_seen >= entry_limit {
            traversal.limited = true;
            return;
        }
        *entries_seen += 1;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_history_files(
                &path,
                depth + 1,
                entries_seen,
                entry_limit,
                files,
                traversal,
            );
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json" || extension == "jsonl")
        {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files.push(HistoryCandidate {
                path,
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                workspace: None,
            });
        }
    }
}

fn scan_history_file(
    adapter: &HistoryAdapter,
    candidate: &HistoryCandidate,
    remaining_bytes: u64,
    observations: &mut ObservationCollector,
    runtime: &mut RuntimeCollector,
) -> HistoryFileScan {
    let read_limit = remaining_bytes.min(MAX_HISTORY_FILE_BYTES);
    let timestamp = unix_time_ms(candidate.modified);
    let mut workspace = candidate.workspace.clone();
    if candidate
        .path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        let Ok(mut file) = fs::File::open(&candidate.path) else {
            return HistoryFileScan::default();
        };
        let mut contents = Vec::new();
        if file
            .by_ref()
            .take(read_limit + 1)
            .read_to_end(&mut contents)
            .is_err()
            || contents.len() as u64 > read_limit
        {
            return HistoryFileScan {
                parsed: false,
                incomplete: true,
                bytes_read: (contents.len() as u64).min(read_limit),
                byte_limit_reached: contents.len() as u64 > read_limit,
            };
        }
        let Ok(value) = serde_json::from_slice::<Value>(&contents) else {
            return HistoryFileScan {
                parsed: false,
                incomplete: true,
                bytes_read: contents.len() as u64,
                byte_limit_reached: false,
            };
        };
        if let Some(found) = workspace_from_record(&value) {
            workspace = Some(found);
        }
        let mut seen = BTreeSet::new();
        visit_value(
            adapter,
            &value,
            workspace.as_deref(),
            &candidate.path,
            timestamp,
            &mut seen,
            observations,
            runtime,
        );
        return HistoryFileScan {
            parsed: true,
            incomplete: false,
            bytes_read: contents.len() as u64,
            byte_limit_reached: false,
        };
    }

    let Ok(file) = fs::File::open(&candidate.path) else {
        return HistoryFileScan::default();
    };
    let take_limit = read_limit + 1;
    let mut reader = BufReader::new(file.take(take_limit));
    let mut line = Vec::new();
    let mut parsed_any = false;
    let mut incomplete = false;
    let mut seen = BTreeSet::new();
    loop {
        line.clear();
        let Ok(result) = read_bounded_line(&mut reader, &mut line, MAX_HISTORY_LINE_BYTES) else {
            incomplete = true;
            break;
        };
        let Some(oversized) = result else {
            break;
        };
        if oversized {
            incomplete = true;
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            incomplete = true;
            continue;
        };
        parsed_any = true;
        if let Some(found) = workspace_from_record(&value) {
            workspace = Some(found);
        }
        visit_value(
            adapter,
            &value,
            workspace.as_deref(),
            &candidate.path,
            timestamp,
            &mut seen,
            observations,
            runtime,
        );
    }
    let byte_limit_reached = reader.get_ref().limit() == 0;
    incomplete |= byte_limit_reached;
    HistoryFileScan {
        parsed: parsed_any,
        incomplete,
        bytes_read: take_limit
            .saturating_sub(reader.get_ref().limit())
            .min(read_limit),
        byte_limit_reached,
    }
}

#[derive(Default)]
struct HistoryFileScan {
    parsed: bool,
    incomplete: bool,
    bytes_read: u64,
    byte_limit_reached: bool,
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Option<bool>> {
    let mut oversized = false;
    let mut read_any = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(read_any.then_some(oversized));
        }
        read_any = true;
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if !oversized && line.len().saturating_add(end) <= limit {
            line.extend_from_slice(&buffer[..end]);
        } else {
            oversized = true;
        }
        let complete = end < buffer.len() || buffer[end - 1] == b'\n';
        reader.consume(end);
        if complete {
            return Ok(Some(oversized));
        }
    }
}

fn unix_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

struct HistoryCandidate {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    workspace: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::{fs, io::BufReader, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::{
        HistoryAdapter, HistoryCandidate, HistoryTraversal, ObservationCollector, RuntimeCollector,
        collect_history_files, history_candidates, read_bounded_line, scan_history_file,
    };

    fn ignore_runtime(
        _object: &serde_json::Map<String, serde_json::Value>,
        _workspace: Option<&Path>,
        _collected: &mut RuntimeCollector,
    ) {
    }

    fn test_adapter(root: &Path) -> HistoryAdapter {
        HistoryAdapter {
            kind: "test",
            root: root.to_path_buf(),
            include_file: None,
            workspace_for_file: None,
            inspect_runtime: ignore_runtime,
            coverage_limitation: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn history_candidates_do_not_follow_symlinked_directories() {
        let root =
            std::env::temp_dir().join(format!("agentdesktop-history-root-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!(
            "agentdesktop-history-outside-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("session.json"), "{}\n").unwrap();
        symlink(&outside, root.join("linked")).unwrap();

        let (candidates, traversal) = history_candidates(&test_adapter(&root));

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        assert!(candidates.is_empty());
        assert!(!traversal.unreadable);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_history_root_is_reported_as_unsafe() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-history-root-link-{}",
            std::process::id()
        ));
        let target = directory.join("target");
        let root = directory.join("root");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("session.json"), "{}\n").unwrap();
        symlink(&target, &root).unwrap();

        let (candidates, traversal) = history_candidates(&test_adapter(&root));

        let _ = fs::remove_dir_all(&directory);
        assert!(candidates.is_empty());
        assert!(traversal.unreadable);
    }

    #[test]
    fn history_candidate_enumeration_stops_at_its_entry_limit() {
        let root =
            std::env::temp_dir().join(format!("agentdesktop-history-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.json"), "{}\n").unwrap();
        fs::write(root.join("two.json"), "{}\n").unwrap();
        let mut files = Vec::<HistoryCandidate>::new();
        let mut entries_seen = 0;
        let mut traversal = HistoryTraversal::default();

        collect_history_files(&root, 0, &mut entries_seen, 1, &mut files, &mut traversal);

        let _ = fs::remove_dir_all(&root);
        assert!(traversal.limited);
        assert_eq!(entries_seen, 1);
        assert!(files.len() <= 1);
    }

    #[test]
    fn oversized_jsonl_lines_are_discarded_without_hiding_the_next_record() {
        let input = b"123456789\n{}\n";
        let mut reader = BufReader::new(input.as_slice());
        let mut line = Vec::new();

        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            Some(true)
        );
        line.clear();
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 8).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"{}\n");
    }

    #[test]
    fn history_file_with_skipped_records_is_incomplete() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-history-incomplete-{}.jsonl",
            std::process::id()
        ));
        fs::write(&path, "not-json\n{}\n").unwrap();
        let candidate = HistoryCandidate {
            path: path.clone(),
            bytes: fs::metadata(&path).unwrap().len(),
            modified: std::time::SystemTime::now(),
            workspace: None,
        };

        let result = scan_history_file(
            &test_adapter(Path::new("unused")),
            &candidate,
            super::MAX_HISTORY_FILE_BYTES,
            &mut ObservationCollector::default(),
            &mut RuntimeCollector::default(),
        );

        let _ = fs::remove_file(path);
        assert!(result.parsed);
        assert!(result.incomplete);
    }

    #[test]
    fn history_file_respects_the_remaining_byte_budget() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-history-byte-budget-{}.json",
            std::process::id()
        ));
        fs::write(&path, "{}\n").unwrap();
        let candidate = HistoryCandidate {
            path: path.clone(),
            bytes: fs::metadata(&path).unwrap().len(),
            modified: std::time::SystemTime::now(),
            workspace: None,
        };

        let result = scan_history_file(
            &test_adapter(Path::new("unused")),
            &candidate,
            2,
            &mut ObservationCollector::default(),
            &mut RuntimeCollector::default(),
        );

        let _ = fs::remove_file(path);
        assert_eq!(result.bytes_read, 2);
        assert!(result.byte_limit_reached);
        assert!(result.incomplete);
    }
}
