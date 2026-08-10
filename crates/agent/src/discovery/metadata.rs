use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

pub(super) fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

pub(super) fn version_after_component(executable: &Path, component: &str) -> Option<String> {
    let resolved = executable.canonicalize().ok()?;
    let mut components = resolved.components();
    while components.next()?.as_os_str() != component {
        // Keep looking for the named install-layout component.
    }
    components.next()?.as_os_str().to_str().map(str::to_owned)
}

pub(super) fn json_version(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct PackageMetadata {
        version: String,
    }

    let contents = fs::read(path).ok()?;
    let metadata: PackageMetadata = serde_json::from_slice(&contents).ok()?;
    Some(metadata.version).filter(|version| !version.is_empty())
}

#[cfg(target_os = "linux")]
pub(super) fn pacman_version_for_file(executable: &Path) -> Option<String> {
    let relative = executable.strip_prefix("/").ok()?.to_str()?;
    let packages = fs::read_dir("/var/lib/pacman/local").ok()?;

    for package in packages.flatten() {
        let package_path = package.path();
        let files = fs::read_to_string(package_path.join("files")).ok();
        let owns_executable = files.as_deref().is_some_and(|files| {
            files
                .lines()
                .skip_while(|line| *line != "%FILES%")
                .skip(1)
                .any(|line| line == relative)
        });
        if !owns_executable {
            continue;
        }

        let description = fs::read_to_string(package_path.join("desc")).ok()?;
        let version = field(&description, "%VERSION%")?;
        return Some(strip_pacman_release(version).to_owned());
    }

    None
}

#[cfg(not(target_os = "linux"))]
pub(super) fn pacman_version_for_file(_executable: &Path) -> Option<String> {
    None
}

fn field<'a>(contents: &'a str, name: &str) -> Option<&'a str> {
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        if line == name {
            return lines.next().filter(|value| !value.is_empty());
        }
    }
    None
}

fn strip_pacman_release(version: &str) -> &str {
    match version.rsplit_once('-') {
        Some((upstream, release))
            if release.chars().all(|character| character.is_ascii_digit()) =>
        {
            upstream
        }
        _ => version,
    }
}
