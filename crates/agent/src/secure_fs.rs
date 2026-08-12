use std::{fs, io::Write, path::Path};

use anyhow::{Context, bail};

/// Creates a daemon state directory and restricts it to its owner.
pub fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create private directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect private directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "private directory path is not a directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict private directory {}", path.display()))?;
    }
    Ok(())
}

/// Atomically replaces a file using a uniquely-created sibling temporary file.
pub fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("managed path has no UTF-8 file name")?;

    for _ in 0..16 {
        let temporary = directory.join(format!(
            ".{file_name}.agentdesktop.{:016x}.tmp",
            rand::random::<u64>()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create temporary file {}", temporary.display()));
            }
        };

        let result = (|| {
            file.write_all(contents)
                .with_context(|| format!("write temporary file {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temporary file {}", temporary.display()))?;
            drop(file);
            fs::rename(&temporary, path)
                .with_context(|| format!("install managed file at {}", path.display()))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }

    bail!(
        "could not allocate a unique temporary file for {}",
        path.display()
    )
}
