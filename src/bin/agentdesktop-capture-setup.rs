#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("transparent capture setup is only available on Linux")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use agentdesktop::platform::linux::{CAPTURE_TABLE, CaptureSpec, clear_capture_set_ruleset};
    use anyhow::{Context, Result, bail};
    use clap::{Parser, Subcommand};
    use rustix::fs::FlockOperation;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    const CAPTURE_STATE_DIRECTORY: &str = "/run/agentdesktop";
    const CAPTURE_LOCK: &str = "/run/agentdesktop/capture.lock";
    const CAPTURE_STATE: &str = "/run/agentdesktop/capture-state.json";

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct CaptureRegistration {
        cgroup: PathBuf,
        redirect_port: u16,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct CaptureState {
        registrations: Vec<CaptureRegistration>,
    }

    #[derive(Debug, Parser)]
    #[command(
        version,
        about = "Install or remove ephemeral Linux transparent-capture rules"
    )]
    struct Cli {
        #[command(subcommand)]
        command: CaptureCommand,
    }

    #[derive(Debug, Subcommand)]
    enum CaptureCommand {
        Preflight {
            #[arg(long)]
            cgroup: PathBuf,
            #[arg(long, default_value_t = 15001)]
            redirect_port: u16,
            #[arg(long, default_value = "nft", hide = true)]
            nft: PathBuf,
        },
        Render {
            #[arg(long)]
            cgroup: PathBuf,
            #[arg(long, default_value_t = 15001)]
            redirect_port: u16,
        },
        Install {
            #[arg(long)]
            cgroup: PathBuf,
            #[arg(long, default_value_t = 15001)]
            redirect_port: u16,
            #[arg(long, default_value = "nft", hide = true)]
            nft: PathBuf,
        },
        Remove {
            #[arg(long)]
            cgroup: PathBuf,
            #[arg(long, default_value = "nft", hide = true)]
            nft: PathBuf,
        },
        TrustInstall {
            #[arg(long)]
            certificate: PathBuf,
            #[arg(long, default_value = "/etc/pki/ca-trust/source/anchors", hide = true)]
            anchor_directory: PathBuf,
            #[arg(long, default_value = "update-ca-trust", hide = true)]
            update_command: PathBuf,
        },
        TrustRemove {
            #[arg(long)]
            certificate: PathBuf,
            #[arg(long, default_value = "/etc/pki/ca-trust/source/anchors", hide = true)]
            anchor_directory: PathBuf,
            #[arg(long, default_value = "update-ca-trust", hide = true)]
            update_command: PathBuf,
            #[arg(long, default_value = "nft", hide = true)]
            nft: PathBuf,
        },
    }

    pub fn run() -> Result<()> {
        match Cli::parse().command {
            CaptureCommand::Preflight {
                cgroup,
                redirect_port,
                nft,
            } => {
                require_root()?;
                ensure_state_directory(Path::new(CAPTURE_STATE_DIRECTORY))?;
                let _lock = capture_lock()?;
                let spec = CaptureSpec::new(&cgroup, redirect_port)?;
                preflight(&cgroup, &nft)?;
                let mut state = load_capture_state(Path::new(CAPTURE_STATE))?;
                state.retain_live(Path::new("/sys/fs/cgroup"));
                state.add(&spec)?;
                check_capture_state(&nft, &state)?;
                println!("Linux capture prerequisites are available");
                Ok(())
            }
            CaptureCommand::Render {
                cgroup,
                redirect_port,
            } => {
                print!("{}", CaptureSpec::new(&cgroup, redirect_port)?.ruleset());
                Ok(())
            }
            CaptureCommand::Install {
                cgroup,
                redirect_port,
                nft,
            } => {
                require_root()?;
                ensure_state_directory(Path::new(CAPTURE_STATE_DIRECTORY))?;
                let _lock = capture_lock()?;
                let spec = CaptureSpec::new(&cgroup, redirect_port)?;
                preflight(&cgroup, &nft)?;
                let mut state = load_capture_state(Path::new(CAPTURE_STATE))?;
                state.retain_live(Path::new("/sys/fs/cgroup"));
                state.add(&spec)?;
                apply_and_save_capture_state(&nft, Path::new(CAPTURE_STATE), &state)?;
                println!("added Linux capture scope");
                Ok(())
            }
            CaptureCommand::Remove { cgroup, nft } => {
                require_root()?;
                ensure_state_directory(Path::new(CAPTURE_STATE_DIRECTORY))?;
                let _lock = capture_lock()?;
                CaptureSpec::new(&cgroup, 1)?;
                let mut state = load_capture_state(Path::new(CAPTURE_STATE))?;
                state.remove(&cgroup);
                state.retain_live(Path::new("/sys/fs/cgroup"));
                apply_and_save_capture_state(&nft, Path::new(CAPTURE_STATE), &state)?;
                println!("removed Linux capture scope");
                Ok(())
            }
            CaptureCommand::TrustInstall {
                certificate,
                anchor_directory,
                update_command,
            } => {
                require_root()?;
                install_trust(&certificate, &anchor_directory, &update_command)
            }
            CaptureCommand::TrustRemove {
                certificate,
                anchor_directory,
                update_command,
                nft,
            } => {
                require_root()?;
                ensure_state_directory(Path::new(CAPTURE_STATE_DIRECTORY))?;
                let _lock = capture_lock()?;
                let mut state = load_capture_state(Path::new(CAPTURE_STATE))?;
                state.retain_live(Path::new("/sys/fs/cgroup"));
                apply_and_save_capture_state(&nft, Path::new(CAPTURE_STATE), &state)?;
                if !state.registrations.is_empty() {
                    bail!("stop the active captured application before removing inspection trust");
                }
                remove_trust(&certificate, &anchor_directory, &update_command)
            }
        }
    }

    pub(super) fn install_trust(
        certificate: &Path,
        directory: &Path,
        update_command: &Path,
    ) -> Result<()> {
        let contents = read_certificate(certificate)?;
        fs::create_dir_all(directory)?;
        let anchor = anchor_path(directory, &contents);
        if anchor.exists() {
            if fs::read(&anchor)? != contents {
                bail!(
                    "inspection CA anchor {} has unexpected contents",
                    anchor.display()
                );
            }
        } else {
            fs::write(&anchor, &contents)?;
        }
        refresh_trust(update_command)?;
        println!(
            "installed Agent Desktop inspection trust at {}",
            anchor.display()
        );
        Ok(())
    }

    pub(super) fn remove_trust(
        certificate: &Path,
        directory: &Path,
        update_command: &Path,
    ) -> Result<()> {
        let contents = read_certificate(certificate)?;
        let anchor = anchor_path(directory, &contents);
        if anchor.exists() {
            if fs::read(&anchor)? != contents {
                bail!(
                    "refusing to remove modified inspection CA anchor {}",
                    anchor.display()
                );
            }
            fs::remove_file(&anchor)?;
            refresh_trust(update_command)?;
        }
        println!("removed Agent Desktop inspection trust");
        Ok(())
    }

    fn read_certificate(path: &Path) -> Result<Vec<u8>> {
        let contents = fs::read(path)
            .with_context(|| format!("read inspection CA certificate {}", path.display()))?;
        let text = std::str::from_utf8(&contents).context("inspection CA is not PEM text")?;
        if !text.contains("-----BEGIN CERTIFICATE-----")
            || !text.contains("-----END CERTIFICATE-----")
        {
            bail!("inspection CA is not a PEM certificate");
        }
        Ok(contents)
    }

    fn anchor_path(directory: &Path, contents: &[u8]) -> PathBuf {
        let digest = Sha256::digest(contents);
        let fingerprint: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        directory.join(format!("agentdesktop-{fingerprint}.pem"))
    }

    fn refresh_trust(command: &Path) -> Result<()> {
        let status = Command::new(command)
            .arg("extract")
            .status()
            .with_context(|| format!("failed to execute {}", command.display()))?;
        if !status.success() {
            bail!("{} extract failed with {status}", command.display());
        }
        Ok(())
    }

    fn preflight(cgroup: &Path, nft: &Path) -> Result<()> {
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
            bail!("Linux capture requires a mounted cgroup v2 hierarchy");
        }
        let relative = cgroup
            .strip_prefix("/")
            .expect("CaptureSpec already validated an absolute path");
        let host_path = Path::new("/sys/fs/cgroup").join(relative);
        if !host_path.is_dir() {
            bail!("capture cgroup {} does not exist", cgroup.display());
        }
        Command::new(nft)
            .arg("--version")
            .output()
            .with_context(|| format!("failed to execute {}", nft.display()))?;
        Ok(())
    }

    fn check_ruleset(nft: &Path, ruleset: &str) -> Result<()> {
        run_nft(nft, &["--check", "--file", "-"], ruleset)
    }

    fn apply_ruleset(nft: &Path, ruleset: &str) -> Result<()> {
        run_nft(nft, &["--file", "-"], ruleset)
    }

    fn table_exists(nft: &Path) -> Result<bool> {
        Ok(Command::new(nft)
            .args(["list", "table", "inet", CAPTURE_TABLE])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("failed to execute {}", nft.display()))?
            .success())
    }

    fn capture_lock() -> Result<fs::File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(CAPTURE_LOCK)
            .with_context(|| format!("open capture lock {CAPTURE_LOCK}"))?;
        rustix::fs::flock(&lock, FlockOperation::LockExclusive)
            .context("lock Linux capture state")?;
        Ok(lock)
    }

    impl CaptureState {
        fn add(&mut self, spec: &CaptureSpec) -> Result<()> {
            let cgroup = PathBuf::from(format!("/{}", spec.cgroup_path()));
            if let Some(existing) = self
                .registrations
                .iter()
                .find(|registration| registration.cgroup == cgroup)
            {
                if existing.redirect_port != spec.redirect_port() {
                    bail!("capture scope is already registered with a different redirect port");
                }
                return Ok(());
            }
            let mut candidate = self.clone();
            candidate.registrations.push(CaptureRegistration {
                cgroup,
                redirect_port: spec.redirect_port(),
            });
            candidate
                .registrations
                .sort_by(|left, right| left.cgroup.cmp(&right.cgroup));
            candidate.specs()?;
            *self = candidate;
            Ok(())
        }

        fn remove(&mut self, cgroup: &Path) {
            self.registrations
                .retain(|registration| registration.cgroup != cgroup);
        }

        fn retain_live(&mut self, cgroup_root: &Path) {
            self.registrations.retain(|registration| {
                registration
                    .cgroup
                    .strip_prefix("/")
                    .ok()
                    .is_some_and(|relative| cgroup_root.join(relative).is_dir())
            });
        }

        fn specs(&self) -> Result<Vec<CaptureSpec>> {
            let specs = self
                .registrations
                .iter()
                .map(|registration| {
                    CaptureSpec::new(&registration.cgroup, registration.redirect_port)
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some(first) = specs.first() {
                first.reconciliation_ruleset(&specs)?;
            }
            Ok(specs)
        }
    }

    fn capture_state_ruleset(nft: &Path, state: &CaptureState) -> Result<String> {
        let specs = state.specs()?;
        if table_exists(nft)? {
            return specs.first().map_or_else(
                || Ok(clear_capture_set_ruleset()),
                |first| first.reconciliation_ruleset(&specs),
            );
        }
        specs.first().map_or_else(
            || Ok(String::new()),
            |first| first.ruleset_with_cgroups(&specs),
        )
    }

    fn check_capture_state(nft: &Path, state: &CaptureState) -> Result<()> {
        let ruleset = capture_state_ruleset(nft, state)?;
        if !ruleset.is_empty() {
            check_ruleset(nft, &ruleset)?;
        }
        Ok(())
    }

    fn apply_and_save_capture_state(
        nft: &Path,
        state_path: &Path,
        state: &CaptureState,
    ) -> Result<()> {
        let parent = state_path
            .parent()
            .context("capture state path has no parent")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("create temporary capture state")?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut temporary, state).context("serialize capture state")?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;

        let ruleset = capture_state_ruleset(nft, state)?;
        if !ruleset.is_empty() {
            apply_ruleset(nft, &ruleset)?;
        }
        temporary
            .persist(state_path)
            .map_err(|error| error.error)
            .context("commit capture state")?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn load_capture_state(path: &Path) -> Result<CaptureState> {
        if !path.exists() {
            return Ok(CaptureState::default());
        }
        let metadata = fs::metadata(path)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            bail!(
                "capture state {} has unsafe ownership or permissions",
                path.display()
            );
        }
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("parse capture state {}", path.display()))
    }

    fn ensure_state_directory(path: &Path) -> Result<()> {
        if !path.exists() {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = fs::metadata(path)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            bail!(
                "capture state directory {} has unsafe ownership or permissions",
                path.display()
            );
        }
        Ok(())
    }

    fn run_nft(nft: &Path, arguments: &[&str], ruleset: &str) -> Result<()> {
        let mut child = Command::new(nft)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute {}", nft.display()))?;
        child
            .stdin
            .take()
            .expect("nft stdin was piped")
            .write_all(ruleset.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "nftables rejected capture rules: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn require_root() -> Result<()> {
        if rustix::process::geteuid().as_raw() != 0 {
            bail!("installing capture rules requires root privileges");
        }
        Ok(())
    }

    #[cfg(test)]
    mod state_tests {
        use super::{CaptureSpec, CaptureState};
        use std::fs;
        use std::path::Path;

        #[test]
        fn registry_deduplicates_and_prunes_stale_cgroups() {
            let temporary = tempfile::tempdir().unwrap();
            fs::create_dir(temporary.path().join("first.scope")).unwrap();
            fs::create_dir(temporary.path().join("second.scope")).unwrap();
            let first = CaptureSpec::new(Path::new("/first.scope"), 15001).unwrap();
            let second = CaptureSpec::new(Path::new("/second.scope"), 15001).unwrap();
            let mut state = CaptureState::default();

            state.add(&first).unwrap();
            state.add(&first).unwrap();
            state.add(&second).unwrap();
            assert_eq!(state.registrations.len(), 2);

            fs::remove_dir(temporary.path().join("first.scope")).unwrap();
            state.retain_live(temporary.path());
            assert_eq!(state.registrations.len(), 1);
            assert_eq!(state.registrations[0].cgroup, Path::new("/second.scope"));
        }

        #[test]
        fn registry_rejects_incompatible_scopes_without_mutation() {
            let mut state = CaptureState::default();
            let first = CaptureSpec::new(Path::new("/first.scope"), 15001).unwrap();
            let different_depth =
                CaptureSpec::new(Path::new("/app.slice/second.scope"), 15001).unwrap();
            let different_port = CaptureSpec::new(Path::new("/third.scope"), 16001).unwrap();

            state.add(&first).unwrap();
            assert!(state.add(&different_depth).is_err());
            assert!(state.add(&different_port).is_err());
            assert_eq!(state.registrations.len(), 1);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::linux::{install_trust, remove_trust};

    #[test]
    fn trust_lifecycle_is_idempotent_and_refuses_modified_anchor() {
        let temporary = tempfile::tempdir().unwrap();
        let certificate = temporary.path().join("ca.crt");
        let anchors = temporary.path().join("anchors");
        let update = temporary.path().join("update-ca-trust");
        fs::write(
            &certificate,
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        fs::write(&update, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&update, fs::Permissions::from_mode(0o700)).unwrap();

        install_trust(&certificate, &anchors, &update).unwrap();
        install_trust(&certificate, &anchors, &update).unwrap();
        let anchor = fs::read_dir(&anchors)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(&anchor, "modified").unwrap();
        assert!(remove_trust(&certificate, &anchors, &update).is_err());
        fs::write(&anchor, fs::read(&certificate).unwrap()).unwrap();
        remove_trust(&certificate, &anchors, &update).unwrap();
        remove_trust(&certificate, &anchors, &update).unwrap();
        assert!(!anchor.exists());
    }
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}
