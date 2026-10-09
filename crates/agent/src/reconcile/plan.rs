use super::DryRunReport;
use anyhow::Context;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

/// A read-only snapshot of proposed file changes, including ownership sidecars.
/// Apply checks all observed files before making any changes. Individual writes
/// are atomic, but applying a multi-file plan is not a filesystem transaction.
#[derive(Default)]
pub struct ReconcilePlan {
    observed: RefCell<BTreeMap<PathBuf, Option<Vec<u8>>>>,
    operations: RefCell<Vec<FileChange>>,
    private_dirs: PrivateDirs,
    report: DryRunReport,
    /// Why the program that owns this plan is configured but not pointed at
    /// the gateway (the loopback proxy is absent); set by the provider.
    inactive: RefCell<Option<String>>,
}

/// What one provider's plan contains, taken before it is appended to the
/// device-wide plan, for per-program reporting.
pub(super) struct PlanSummary {
    /// The plan writes or removes something that is not already so on disk,
    /// or records a create/update/remove (a mode-only rewrite).
    pub changes: bool,
    /// One line per conflicting file.
    pub conflicts: Vec<String>,
    pub inactive: Option<String>,
    /// Paths the plan read or writes.
    pub paths: Vec<PathBuf>,
}

/// Where an apply stopped.
pub(super) enum ApplyStop {
    /// A recorded conflict refused the whole plan before anything ran.
    Conflict,
    /// An observed file changed after planning; nothing was written.
    Observed(PathBuf),
    /// A private directory could not be created; nothing was written.
    PrivateDir(PathBuf),
    /// The operation at this index failed; the ones before it were written.
    Operation(usize),
}

struct FileChange {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    permissions: u32,
}

/// A directory to create owner-only before the file changes are applied.
/// Used for a directory that will hold a secret-bearing file and does not
/// exist yet; an existing directory is left as it is.
#[derive(Default)]
struct PrivateDirs(RefCell<Vec<PathBuf>>);

impl ReconcilePlan {
    pub fn render(&self) -> String {
        self.report.render()
    }

    pub fn has_conflicts(&self) -> bool {
        self.report
            .changes
            .borrow()
            .iter()
            .any(|change| change.action == "conflict")
    }

    /// Consume a plan so it cannot accidentally be applied twice.
    pub fn apply(self) -> anyhow::Result<()> {
        self.apply_tracked().map_err(|(error, _)| error)
    }

    /// Records that the program owning this plan uses the gateway, a gateway
    /// is configured, and the loopback proxy is absent.
    pub fn inactive(&self, reason: &str) {
        *self.inactive.borrow_mut() = Some(reason.to_owned());
    }

    /// The reason recorded by [`ReconcilePlan::inactive`], for tests.
    #[cfg(test)]
    pub(crate) fn inactive_reason(&self) -> Option<String> {
        self.inactive.borrow().clone()
    }

    pub(super) fn operation_count(&self) -> usize {
        self.operations.borrow().len()
    }

    pub(super) fn summary(&self) -> PlanSummary {
        let observed = self.observed.borrow();
        let operations = self.operations.borrow();
        let changes = self.report.changes.borrow();
        let differs = operations.iter().any(|change| {
            observed.get(&change.path).map(Option::as_deref) != Some(change.contents.as_deref())
        });
        let recorded = changes
            .iter()
            .any(|change| matches!(change.action.as_str(), "create" | "update" | "remove"));
        let conflicts = changes
            .iter()
            .filter(|change| change.action == "conflict")
            .map(|change| {
                format!(
                    "{} at {}: conflicting or invalid existing configuration",
                    change.description,
                    change.path.display()
                )
            })
            .collect();
        let mut paths: Vec<PathBuf> = observed.keys().cloned().collect();
        for change in operations.iter() {
            if !paths.contains(&change.path) {
                paths.push(change.path.clone());
            }
        }
        PlanSummary {
            changes: differs || recorded,
            conflicts,
            inactive: self.inactive.borrow().clone(),
            paths,
        }
    }

    /// Applies like [`ReconcilePlan::apply`] and says where it stopped.
    pub(super) fn apply_tracked(self) -> Result<(), (anyhow::Error, ApplyStop)> {
        if let Some(conflict) = self
            .report
            .changes
            .borrow()
            .iter()
            .find(|change| change.action == "conflict")
        {
            return Err((
                anyhow::anyhow!(
                    "refusing to change {} {} at {}: conflicting or invalid existing configuration",
                    conflict.display_name,
                    conflict.description,
                    conflict.path.display()
                ),
                ApplyStop::Conflict,
            ));
        }
        for (path, expected) in self.observed.borrow().iter() {
            let stop = || ApplyStop::Observed(path.clone());
            let current = read_optional(path)
                .with_context(|| format!("validate {} before applying plan", path.display()))
                .map_err(|error| (error, stop()))?;
            if &current != expected {
                return Err((
                    anyhow::anyhow!(
                        "{} changed since reconciliation was planned; retry reconciliation",
                        path.display()
                    ),
                    stop(),
                ));
            }
        }
        for dir in self.private_dirs.0.into_inner() {
            if !dir.exists() {
                crate::secure_fs::ensure_private_dir(&dir)
                    .map_err(|error| (error, ApplyStop::PrivateDir(dir.clone())))?;
            }
        }
        let observed = self.observed.into_inner();
        for (index, change) in self.operations.into_inner().into_iter().enumerate() {
            if let Some(contents) = &change.contents
                && already_in_place(&change.path, contents, change.permissions, &observed)
            {
                continue;
            }
            let result = (|| -> anyhow::Result<()> {
                match change.contents {
                    Some(contents) => {
                        let parent = change
                            .path
                            .parent()
                            .filter(|p| !p.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."));
                        fs::create_dir_all(parent)
                            .with_context(|| format!("create directory {}", parent.display()))?;
                        crate::secure_fs::atomic_write(&change.path, &contents, change.permissions)
                    }
                    None => fs::remove_file(&change.path)
                        .with_context(|| format!("remove {}", change.path.display())),
                }
            })();
            result.map_err(|error| (error, ApplyStop::Operation(index)))?;
            tracing::info!(path = %change.path.display(), "applied reconciliation change");
        }
        Ok(())
    }

    pub(super) fn append(&mut self, other: Self) -> anyhow::Result<()> {
        self.append_attributed(other).map_err(|(error, _)| error)
    }

    /// Appends like [`ReconcilePlan::append`]; on failure also returns the
    /// path the two plans disagree about.
    pub(super) fn append_attributed(
        &mut self,
        other: Self,
    ) -> Result<(), (anyhow::Error, PathBuf)> {
        let observations = self.observed.get_mut();
        for (path, expected) in other.observed.into_inner() {
            if let Some(previous) = observations.get(&path)
                && previous != &expected
            {
                return Err((
                    anyhow::anyhow!("{} changed while planning reconciliation", path.display()),
                    path,
                ));
            }
            observations.insert(path, expected);
        }
        self.private_dirs
            .0
            .get_mut()
            .extend(other.private_dirs.0.into_inner());
        let operations = self.operations.get_mut();
        for change in other.operations.into_inner() {
            if operations
                .iter()
                .any(|existing| existing.path == change.path)
            {
                return Err((
                    anyhow::anyhow!(
                        "multiple providers plan to modify {}",
                        change.path.display()
                    ),
                    change.path,
                ));
            }
            operations.push(change);
        }
        self.report
            .changes
            .get_mut()
            .extend(other.report.changes.into_inner());
        Ok(())
    }

    /// Cache reads so ownership decisions and application use the same snapshot.
    pub(crate) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut observed = self.observed.borrow_mut();
        if !observed.contains_key(path) {
            observed.insert(path.to_owned(), read_optional(path)?);
        }
        observed[path]
            .clone()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    pub(crate) fn write_file(
        &self,
        path: &Path,
        contents: &[u8],
        permissions: u32,
    ) -> anyhow::Result<()> {
        match self.read(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {} before planning write", path.display()));
            }
        }
        self.operations.borrow_mut().push(FileChange {
            path: path.to_owned(),
            contents: Some(contents.to_vec()),
            permissions,
        });
        Ok(())
    }

    /// Create `dir` owner-only at apply time if it does not exist by then.
    pub(crate) fn ensure_private_dir(&self, dir: &Path) {
        self.private_dirs.0.borrow_mut().push(dir.to_owned());
    }

    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.read(path)?;
        self.operations.borrow_mut().push(FileChange {
            path: path.to_owned(),
            contents: None,
            permissions: 0,
        });
        Ok(())
    }

    pub(crate) fn record(&self, display_name: &str, description: &str, action: &str, path: &Path) {
        let before = (action == "remove").then(|| self.read(path).ok()).flatten();
        self.report.record(
            display_name,
            description,
            action,
            path,
            before.as_deref(),
            None,
        );
    }

    pub(crate) fn record_diff(
        &self,
        display_name: &str,
        description: &str,
        action: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.report
            .record(display_name, description, action, path, before, after);
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// --- No-op writes skipped ----------------------------------------------------

/// Whether a planned write would change nothing: the file already holds the
/// planned bytes (as observed when planning, which the apply has just
/// verified) and, on Unix, grants no permission bit outside the planned
/// mode. A file looser than planned is rewritten; a stricter one is left.
fn already_in_place(
    path: &Path,
    contents: &[u8],
    permissions: u32,
    observed: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> bool {
    // A file gone since it was observed is written again.
    observed.get(path).and_then(Option::as_deref) == Some(contents)
        && fs::metadata(path).is_ok()
        && !grants_beyond(path, permissions)
}

/// Whether the file at `path` grants a permission bit outside `mode`.
/// Unix: the current mode has a bit outside `mode`. A missing file, and any
/// file on non-Unix, is false.
pub(crate) fn grants_beyond(path: &Path, mode: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & !mode & 0o777 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        false
    }
}

/// Shared checks for the planners that write a managed file (C5.1).
#[cfg(all(test, unix))]
pub(crate) mod mode_repair {
    use super::ReconcilePlan;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::Path,
    };

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn applied(plan_fn: &dyn Fn(&ReconcilePlan) -> anyhow::Result<()>) {
        let plan = ReconcilePlan::default();
        plan_fn(&plan).unwrap();
        plan.apply().unwrap();
    }

    /// `plan_fn` plans the program's managed `files`, each planned at
    /// `planned_mode`. After a first apply every file is loosened to 0666 with
    /// its bytes untouched; the next plan must record an `update` and the
    /// apply must restore `planned_mode` on every file.
    pub(crate) fn assert_identical_bytes_with_looser_mode_is_an_update(
        files: &[&Path],
        planned_mode: u32,
        plan_fn: &dyn Fn(&ReconcilePlan) -> anyhow::Result<()>,
    ) {
        applied(plan_fn);
        let bytes: Vec<_> = files.iter().map(|file| fs::read(file).unwrap()).collect();
        for file in files {
            fs::set_permissions(file, fs::Permissions::from_mode(0o666)).unwrap();
        }

        let plan = ReconcilePlan::default();
        plan_fn(&plan).unwrap();
        let rendered = plan.render();
        assert!(
            rendered.contains("UPDATE"),
            "a loosened mode with identical bytes must be an update: {rendered}"
        );
        plan.apply().unwrap();

        for (file, bytes) in files.iter().zip(bytes) {
            assert_eq!(
                mode(file),
                planned_mode,
                "{} must be rewritten at the planned mode",
                file.display()
            );
            assert_eq!(fs::read(file).unwrap(), bytes);
        }
    }

    /// After a first apply every file is tightened to 0600 (within any planned
    /// mode); the next plan must record no change and must not write.
    pub(crate) fn assert_identical_bytes_within_mode_is_unchanged(
        files: &[&Path],
        plan_fn: &dyn Fn(&ReconcilePlan) -> anyhow::Result<()>,
    ) {
        applied(plan_fn);
        for file in files {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let before: Vec<_> = files
            .iter()
            .map(|file| {
                let metadata = fs::metadata(file).unwrap();
                (metadata.ino(), metadata.modified().unwrap())
            })
            .collect();

        let plan = ReconcilePlan::default();
        plan_fn(&plan).unwrap();
        let rendered = plan.render();
        assert!(
            !rendered.contains("UPDATE") && !rendered.contains("CREATE"),
            "an identical file within the planned mode must stay unchanged: {rendered}"
        );
        plan.apply().unwrap();

        for (file, (ino, modified)) in files.iter().zip(before) {
            let metadata = fs::metadata(file).unwrap();
            assert_eq!(
                metadata.ino(),
                ino,
                "{} must not be replaced",
                file.display()
            );
            assert_eq!(metadata.modified().unwrap(), modified);
            assert_eq!(mode(file), 0o600, "a stricter mode must be left");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReconcilePlan, already_in_place, grants_beyond};
    use std::{collections::BTreeMap, fs};

    #[test]
    fn grants_beyond_is_false_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!grants_beyond(&dir.path().join("absent"), 0o600));
    }

    #[test]
    #[cfg(unix)]
    fn grants_beyond_is_false_within_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"x").unwrap();
        for current in [0o600, 0o640, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(current)).unwrap();
            assert!(!grants_beyond(&path, 0o644), "{current:o} within 644");
        }
    }

    #[test]
    #[cfg(unix)]
    fn grants_beyond_is_true_with_a_bit_outside_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"x").unwrap();
        for (current, planned) in [(0o664, 0o644), (0o644, 0o600), (0o755, 0o644)] {
            fs::set_permissions(&path, fs::Permissions::from_mode(current)).unwrap();
            assert!(
                grants_beyond(&path, planned),
                "{current:o} beyond {planned:o}"
            );
        }
    }

    #[test]
    fn already_in_place_still_writes_a_file_removed_after_the_observed_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone");
        let observed = BTreeMap::from([(path.clone(), Some(b"hello\n".to_vec()))]);
        assert!(
            !already_in_place(&path, b"hello\n", 0o644, &observed),
            "a file that no longer exists must be written"
        );
    }

    #[test]
    fn identical_write_is_skipped_same_inode_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");

        let plan = ReconcilePlan::default();
        plan.write_file(&path, b"hello\n", 0o644).unwrap();
        plan.apply().unwrap();
        let before = fs::metadata(&path).unwrap();
        let before_mtime = before.modified().unwrap();
        #[cfg(unix)]
        let before_ino = {
            use std::os::unix::fs::MetadataExt;
            before.ino()
        };

        // A second, identical plan must not touch the file at all.
        let plan = ReconcilePlan::default();
        plan.write_file(&path, b"hello\n", 0o644).unwrap();
        plan.apply().unwrap();

        let after = fs::metadata(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                after.ino(),
                before_ino,
                "an identical write must not replace the file"
            );
        }
        assert_eq!(
            after.modified().unwrap(),
            before_mtime,
            "an identical write must not touch the file's mtime"
        );
        assert_eq!(fs::read(&path).unwrap(), b"hello\n");
    }

    #[test]
    #[cfg(unix)]
    fn same_bytes_at_a_looser_mode_are_rewritten_at_the_planned_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"hello\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();

        let plan = ReconcilePlan::default();
        plan.write_file(&path, b"hello\n", 0o600).unwrap();
        plan.apply().unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a file looser than planned must be tightened even with identical bytes"
        );
        assert_eq!(fs::read(&path).unwrap(), b"hello\n");
    }

    #[test]
    #[cfg(unix)]
    fn same_bytes_at_a_stricter_mode_are_left() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"hello\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before_ino = fs::metadata(&path).unwrap().ino();

        let plan = ReconcilePlan::default();
        plan.write_file(&path, b"hello\n", 0o644).unwrap();
        plan.apply().unwrap();

        let after = fs::metadata(&path).unwrap();
        assert_eq!(
            after.ino(),
            before_ino,
            "a file stricter than planned must not be rewritten"
        );
        assert_eq!(
            after.permissions().mode() & 0o777,
            0o600,
            "a stricter mode must not be loosened to match the plan"
        );
    }

    #[test]
    fn removal_whose_file_vanished_before_apply_fails_the_observed_check_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"hello\n").unwrap();

        let plan = ReconcilePlan::default();
        plan.remove_file(&path).unwrap();
        fs::remove_file(&path).unwrap();

        let error = plan.apply().unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&path.display().to_string()),
            "error must name the path: {message}"
        );
        assert!(
            message.contains("changed since reconciliation was planned"),
            "unexpected error: {message}"
        );
    }
}
