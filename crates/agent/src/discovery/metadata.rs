use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::Skill;
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

pub(super) fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Home directories that may contain per-user developer-tool configuration.
///
/// The daemon commonly runs as root or in a container, so its own `HOME` is
/// not necessarily the home of the users whose tools it discovers.
pub(super) fn user_home_dirs() -> Vec<PathBuf> {
    let mut homes = BTreeSet::new();
    homes.extend(home_dir());

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

    for parent in [Path::new("/home"), Path::new("/Users")] {
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
