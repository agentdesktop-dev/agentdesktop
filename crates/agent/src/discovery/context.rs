use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

pub(super) struct ScanContext {
    home: Option<PathBuf>,
    homes: Vec<PathBuf>,
    cwd: Option<PathBuf>,
    search_path: Vec<PathBuf>,
    environment: BTreeMap<&'static str, std::ffi::OsString>,
    include_system_paths: bool,
    ancestor_root: Option<PathBuf>,
}

impl ScanContext {
    pub(super) fn capture() -> Self {
        let home = home_dir();
        let homes = user_home_dirs(home.as_deref());
        Self {
            home,
            homes,
            cwd: env::current_dir().ok(),
            search_path: env::var_os("PATH")
                .map(|value| env::split_paths(&value).collect())
                .unwrap_or_default(),
            environment: ["CODEX_HOME", "PATHEXT", "ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(|name| env::var_os(name).map(|value| (name, value)))
                .collect(),
            include_system_paths: true,
            ancestor_root: None,
        }
    }

    pub(super) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub(super) fn homes(&self) -> &[PathBuf] {
        &self.homes
    }

    /// Empty values are treated as unset, matching how the harnesses read them.
    pub(super) fn env_path(&self, name: &str) -> Option<PathBuf> {
        self.environment
            .get(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    pub(super) fn system_path(&self, path: &str) -> Option<PathBuf> {
        self.include_system_paths.then(|| PathBuf::from(path))
    }

    pub(super) fn current_dir_ancestors(&self, relative: &Path) -> Vec<PathBuf> {
        self.cwd
            .as_ref()
            .into_iter()
            .flat_map(|cwd| cwd.ancestors())
            .take_while(|directory| {
                self.ancestor_root
                    .as_ref()
                    .is_none_or(|root| directory.starts_with(root))
            })
            .map(|directory| directory.join(relative))
            .collect()
    }

    pub(super) fn executable_candidates(&self, name: &str) -> Vec<PathBuf> {
        let extensions = self.executable_extensions(name);
        self.search_path
            .iter()
            .flat_map(|directory| {
                extensions
                    .iter()
                    .map(move |extension| directory.join(format!("{name}{extension}")))
            })
            .collect()
    }

    pub(super) fn find_executable(
        &self,
        name: &str,
        additional: impl IntoIterator<Item = PathBuf>,
    ) -> Option<PathBuf> {
        self.executable_candidates(name)
            .into_iter()
            .chain(additional)
            .find(|candidate| candidate.is_file())
    }

    fn executable_extensions(&self, name: &str) -> Vec<String> {
        #[cfg(windows)]
        {
            if Path::new(name).extension().is_some() {
                return vec![String::new()];
            }
            let extensions = self
                .environment
                .get("PATHEXT")
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
            std::iter::once(String::new())
                .chain(
                    extensions
                        .split(';')
                        .filter(|extension| !extension.is_empty())
                        .map(|extension| {
                            if extension.starts_with('.') {
                                extension.to_owned()
                            } else {
                                format!(".{extension}")
                            }
                        }),
                )
                .collect()
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            vec![String::new()]
        }
    }

    #[cfg(test)]
    pub(super) fn isolated(home: PathBuf, cwd: PathBuf) -> Self {
        Self {
            ancestor_root: home.parent().map(Path::to_path_buf),
            homes: vec![home.clone()],
            search_path: vec![home.join("bin")],
            home: Some(home),
            cwd: Some(cwd),
            environment: BTreeMap::new(),
            include_system_paths: false,
        }
    }

    #[cfg(test)]
    pub(super) fn with_search_path(mut self, directories: Vec<PathBuf>) -> Self {
        self.search_path = directories;
        self
    }

    #[cfg(test)]
    pub(super) fn with_home(mut self, home: PathBuf) -> Self {
        self.homes.push(home);
        self.homes.sort();
        self.homes.dedup();
        self
    }

    #[cfg(test)]
    pub(super) fn with_override(mut self, name: &'static str, value: PathBuf) -> Self {
        self.environment.insert(name, value.into_os_string());
        self
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = env_path("USERPROFILE") {
            return Some(profile);
        }
        Some(PathBuf::from(env::var_os("HOMEDRIVE")?).join(env::var_os("HOMEPATH")?))
    }
    #[cfg(unix)]
    env_path("HOME")
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn user_home_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut homes = BTreeSet::new();
    homes.extend(home.map(Path::to_path_buf));
    #[cfg(unix)]
    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let fields: Vec<_> = line.split(':').collect();
            if let Some(home) = fields.get(5).filter(|home| home.starts_with('/')) {
                let path = PathBuf::from(home);
                if path.is_dir() {
                    homes.insert(path);
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    let parents = vec![PathBuf::from("/home")];
    #[cfg(target_os = "macos")]
    let parents = vec![PathBuf::from("/Users")];
    #[cfg(windows)]
    let parents: Vec<_> = home
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .into_iter()
        .chain(env_path("SystemDrive").map(|drive| drive.join("Users")))
        .collect();
    for parent in parents {
        if let Ok(entries) = fs::read_dir(parent) {
            homes.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir()),
            );
        }
    }
    homes.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_context_never_inherits_host_paths_or_environment() {
        let root = env::temp_dir().join("discovery-context");
        let context = ScanContext::isolated(root.join("home"), root.join("project"));
        assert_eq!(context.homes(), &[root.join("home")]);
        assert!(context.env_path("CODEX_HOME").is_none());
        assert!(context.system_path("/etc/codex/config.toml").is_none());
        assert!(
            context
                .executable_candidates("codex")
                .iter()
                .all(|path| path.starts_with(root.join("home/bin")))
        );
    }

    #[test]
    fn empty_override_is_treated_as_absent() {
        let context = ScanContext::isolated(PathBuf::new(), PathBuf::new())
            .with_override("CODEX_HOME", PathBuf::new());
        assert_eq!(context.env_path("CODEX_HOME"), None);
    }

    #[test]
    fn lookup_preserves_path_order_before_native_fallbacks() {
        let root = env::temp_dir().join(format!("scan-path-{}", rand::random::<u64>()));
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("tool"), "not executable").unwrap();
        fs::write(second.join("tool"), "not executable").unwrap();
        let mut context = ScanContext::isolated(root.join("home"), root.join("project"));
        context.search_path = vec![first.clone(), second.clone()];
        assert_eq!(
            context.find_executable("tool", [second.join("tool")]),
            Some(first.join("tool"))
        );
        fs::remove_file(first.join("tool")).unwrap();
        assert_eq!(
            context.find_executable("tool", []),
            Some(second.join("tool"))
        );
        context.search_path.clear();
        assert_eq!(
            context.find_executable("tool", [second.join("tool")]),
            Some(second.join("tool"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_extensions_use_captured_pathext() {
        let context = ScanContext::isolated(PathBuf::new(), PathBuf::new())
            .with_override("PATHEXT", PathBuf::from("EXE;.cmd"));
        assert_eq!(context.executable_extensions("code"), ["", ".EXE", ".cmd"]);
        assert_eq!(context.executable_extensions("code.cmd"), [""]);
    }
}
