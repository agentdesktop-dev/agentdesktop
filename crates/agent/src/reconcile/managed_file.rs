use std::{fs, path::Path};

use anyhow::Context;
use tracing::info;

use crate::secure_fs;

use super::{ReconcileMode, program_name};

/// A managed file identified by a fixed ownership header.
pub(super) struct HeaderOwnedFile {
    pub program: &'static str,
    pub header: &'static str,
}

impl HeaderOwnedFile {
    /// Writes the configured header followed by the unmarked native body, byte for byte.
    /// The caller controls the body's final newline; `None` removes only an owned file.
    pub fn reconcile(
        &self,
        path: &Path,
        description: &str,
        body: Option<&[u8]>,
        mode: ReconcileMode<'_>,
    ) -> anyhow::Result<()> {
        let rendered = body.map(|body| [self.header.as_bytes(), body].concat());
        let contents = rendered.as_deref();
        let program = self.program;
        let name = program_name(program);
        let existing = match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {name} {description} from {}", path.display()));
            }
        };
        let owned = existing
            .as_ref()
            .is_some_and(|bytes| bytes.starts_with(self.header.as_bytes()));
        let action = match (existing.as_deref(), contents) {
            (before, after) if before == after => "unchanged",
            (Some(_), None) if owned => "remove",
            (_, None) => "unchanged",
            (None, Some(_)) => "create",
            (Some(_), Some(_)) if owned => "update",
            _ if mode.is_dry_run() => "conflict",
            _ => anyhow::bail!(
                "refusing to replace {name} {description} not owned by Agentdesktop at {}",
                path.display()
            ),
        };

        if mode.writes() {
            match action {
                "create" | "update" => {
                    let directory = path
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    fs::create_dir_all(directory).with_context(|| {
                        format!("create {name} directory {}", directory.display())
                    })?;
                    secure_fs::atomic_write(path, contents.expect("write has contents"), 0o644)?;
                }
                "remove" => fs::remove_file(path).with_context(|| {
                    format!("remove {name} {description} at {}", path.display())
                })?,
                _ => {}
            }
        }
        if action == "unchanged" {
            mode.record(program, description, action, path);
        } else {
            mode.record_diff(
                program,
                description,
                action,
                path,
                existing.as_deref(),
                contents,
            );
        }
        info!(program, kind = description, action, path = %path.display(), "reconciled managed file");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::DryRunReport;

    const FORMATS: [HeaderOwnedFile; 2] = [
        HeaderOwnedFile {
            program: "codex",
            header: "# Managed by Agentdesktop. Manual changes will be replaced.\n",
        },
        HeaderOwnedFile {
            program: "opencode",
            header: "// Managed by Agentdesktop. Manual changes will be replaced.\n",
        },
    ];

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "managed-file-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            )))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn previews_and_applies_create_update_unchanged_and_remove_for_both_headers() {
        for format in FORMATS {
            let fixture = Fixture::new();
            let path = fixture.0.join("nested/settings");
            for (body, expected) in [
                (Some("old\n"), "create"),
                (Some("old\n"), "unchanged"),
                (Some("new\n"), "update"),
                (None, "remove"),
                (None, "unchanged"),
            ] {
                let rendered = body.map(|body| format!("{}{body}", format.header));
                let before = fs::read(&path).ok();
                let directory_existed = fixture.0.exists();
                let report = DryRunReport::default();
                format
                    .reconcile(
                        &path,
                        "configuration",
                        body.map(str::as_bytes),
                        ReconcileMode::DryRun(&report),
                    )
                    .unwrap();
                assert_eq!(fs::read(&path).ok(), before);
                assert_eq!(fixture.0.exists(), directory_existed);
                let changes = report.changes.borrow();
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].action, expected);
                assert_eq!(changes[0].program, format.program);
                assert_eq!(changes[0].path, path);
                if expected == "unchanged" {
                    assert!(changes[0].before.is_none() && changes[0].after.is_none());
                } else {
                    assert_eq!(
                        changes[0].before.as_deref(),
                        before
                            .as_deref()
                            .map(|bytes| std::str::from_utf8(bytes).unwrap())
                    );
                    assert_eq!(changes[0].after.as_deref(), rendered.as_deref());
                }
                drop(changes);
                let modified = fs::metadata(&path)
                    .ok()
                    .map(|meta| meta.modified().unwrap());
                format
                    .reconcile(
                        &path,
                        "configuration",
                        body.map(str::as_bytes),
                        ReconcileMode::Apply,
                    )
                    .unwrap();
                assert_eq!(
                    fs::read(&path).ok().as_deref(),
                    rendered.as_deref().map(str::as_bytes)
                );
                if expected == "unchanged" {
                    assert_eq!(
                        fs::metadata(&path)
                            .ok()
                            .map(|meta| meta.modified().unwrap()),
                        modified
                    );
                }
                #[cfg(unix)]
                if body.is_some() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = fs::metadata(&path).unwrap().permissions().mode();
                    assert_eq!(
                        mode & 0o133,
                        0,
                        "managed config must not be executable or writable by others"
                    );
                    assert_ne!(mode & 0o400, 0);
                }
            }
        }
    }

    #[test]
    fn header_preserves_native_body_bytes_and_final_newlines() {
        for format in FORMATS {
            let fixture = Fixture::new();
            let path = fixture.0.join("settings");
            for body in [
                b"".as_slice(),
                b"no final newline",
                b"one final newline\n",
                b"two final newlines\n\n",
                b"native\r\n",
                b"non-utf8: \xff\n",
            ] {
                let rendered = [format.header.as_bytes(), body].concat();
                let before = fs::read(&path).ok();
                let report = DryRunReport::default();
                format
                    .reconcile(
                        &path,
                        "configuration",
                        Some(body),
                        ReconcileMode::DryRun(&report),
                    )
                    .unwrap();
                assert_eq!(fs::read(&path).ok(), before);
                assert_eq!(
                    report.changes.borrow()[0].after.as_deref(),
                    Some(String::from_utf8_lossy(&rendered).as_ref())
                );
                format
                    .reconcile(&path, "configuration", Some(body), ReconcileMode::Apply)
                    .unwrap();
                assert_eq!(fs::read(&path).unwrap(), rendered);
                let report = DryRunReport::default();
                format
                    .reconcile(
                        &path,
                        "configuration",
                        Some(body),
                        ReconcileMode::DryRun(&report),
                    )
                    .unwrap();
                assert_eq!(report.changes.borrow()[0].action, "unchanged");
            }
        }
    }

    #[test]
    fn foreign_files_conflict_on_update_and_survive_removal() {
        for format in FORMATS {
            let fixture = Fixture::new();
            fs::create_dir_all(&fixture.0).unwrap();
            let path = fixture.0.join("settings");
            let body = "managed\n";
            let rendered = format!("{}{body}", format.header);
            for foreign in [
                "user-owned\n",
                "prefix # Managed by Agentdesktop. Manual changes will be replaced.\n",
            ] {
                fs::write(&path, foreign).unwrap();
                let report = DryRunReport::default();
                format
                    .reconcile(
                        &path,
                        "configuration",
                        Some(body.as_bytes()),
                        ReconcileMode::DryRun(&report),
                    )
                    .unwrap();
                assert_eq!(report.changes.borrow()[0].action, "conflict");
                assert_eq!(report.changes.borrow()[0].before.as_deref(), Some(foreign));
                assert_eq!(
                    report.changes.borrow()[0].after.as_deref(),
                    Some(rendered.as_str())
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
                let error = format
                    .reconcile(
                        &path,
                        "configuration",
                        Some(body.as_bytes()),
                        ReconcileMode::Apply,
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("not owned by Agentdesktop"));
                assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
                let report = DryRunReport::default();
                format
                    .reconcile(&path, "configuration", None, ReconcileMode::DryRun(&report))
                    .unwrap();
                assert_eq!(report.changes.borrow()[0].action, "unchanged");
                format
                    .reconcile(&path, "configuration", None, ReconcileMode::Apply)
                    .unwrap();
                assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
            }
        }
    }

    #[test]
    fn read_errors_are_not_treated_as_missing_files() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        let format = &FORMATS[0];
        for body in [None, Some(b"body\n".as_slice())] {
            let report = DryRunReport::default();
            for mode in [ReconcileMode::Apply, ReconcileMode::DryRun(&report)] {
                let error = format
                    .reconcile(&fixture.0, "configuration", body, mode)
                    .unwrap_err();
                assert!(error.to_string().contains("read Codex configuration"));
                assert!(fixture.0.is_dir());
            }
            assert!(report.changes.borrow().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_filename_preflight_leaves_no_target_or_temporary_file() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.0).unwrap();
        let path = fixture.0.join(std::ffi::OsString::from_vec(vec![0xff]));
        let error = FORMATS[0]
            .reconcile(
                &path,
                "configuration",
                Some(b"body\n"),
                ReconcileMode::Apply,
            )
            .unwrap_err();
        assert!(error.to_string().contains("UTF-8 file name"));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }
}
