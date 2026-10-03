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
        for (index, change) in self.operations.into_inner().into_iter().enumerate() {
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
