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
    report: DryRunReport,
}

struct FileChange {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    permissions: u32,
}

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
        if let Some(conflict) = self
            .report
            .changes
            .borrow()
            .iter()
            .find(|change| change.action == "conflict")
        {
            anyhow::bail!(
                "refusing to change {} {} at {}: conflicting or invalid existing configuration",
                super::program_name(&conflict.program),
                conflict.description,
                conflict.path.display()
            );
        }
        for (path, expected) in self.observed.borrow().iter() {
            let current = read_optional(path)
                .with_context(|| format!("validate {} before applying plan", path.display()))?;
            anyhow::ensure!(
                &current == expected,
                "{} changed since reconciliation was planned; retry reconciliation",
                path.display()
            );
        }
        for change in self.operations.into_inner() {
            match change.contents {
                Some(contents) => {
                    let parent = change
                        .path
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create directory {}", parent.display()))?;
                    crate::secure_fs::atomic_write(&change.path, &contents, change.permissions)?;
                }
                None => fs::remove_file(&change.path)
                    .with_context(|| format!("remove {}", change.path.display()))?,
            }
            tracing::info!(path = %change.path.display(), "applied reconciliation change");
        }
        Ok(())
    }

    pub(super) fn append(&mut self, other: Self) -> anyhow::Result<()> {
        let observations = self.observed.get_mut();
        for (path, expected) in other.observed.into_inner() {
            if let Some(previous) = observations.get(&path) {
                anyhow::ensure!(
                    previous == &expected,
                    "{} changed while planning reconciliation",
                    path.display()
                );
            }
            observations.insert(path, expected);
        }
        let operations = self.operations.get_mut();
        for change in other.operations.into_inner() {
            anyhow::ensure!(
                !operations
                    .iter()
                    .any(|existing| existing.path == change.path),
                "multiple providers plan to modify {}",
                change.path.display()
            );
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

    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.read(path)?;
        self.operations.borrow_mut().push(FileChange {
            path: path.to_owned(),
            contents: None,
            permissions: 0,
        });
        Ok(())
    }

    pub(crate) fn record(&self, program: &str, description: &str, action: &str, path: &Path) {
        let before = (action == "remove").then(|| self.read(path).ok()).flatten();
        self.report
            .record(program, description, action, path, before.as_deref(), None);
    }

    pub(crate) fn record_diff(
        &self,
        program: &str,
        description: &str,
        action: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.report
            .record(program, description, action, path, before, after);
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
