#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("transparent capture setup is only available on Linux")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use agentgateway_edge_connector::platform::linux::{
        CAPTURE_TABLE, CaptureSpec, remove_ruleset,
    };
    use anyhow::{Context, Result, bail};
    use clap::{Parser, Subcommand};

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
                let spec = CaptureSpec::new(&cgroup, redirect_port)?;
                preflight(&cgroup, &nft)?;
                check_ruleset(&nft, &installation_ruleset(&nft, &spec)?)?;
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
                let spec = CaptureSpec::new(&cgroup, redirect_port)?;
                preflight(&cgroup, &nft)?;
                apply_ruleset(&nft, &installation_ruleset(&nft, &spec)?)?;
                println!("installed ephemeral Linux capture rules");
                Ok(())
            }
            CaptureCommand::Remove { nft } => {
                require_root()?;
                remove_if_present(&nft)?;
                println!("removed ephemeral Linux capture rules");
                Ok(())
            }
        }
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

    fn installation_ruleset(nft: &Path, spec: &CaptureSpec) -> Result<String> {
        Ok(spec.installation_ruleset(table_exists(nft)?))
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

    fn remove_if_present(nft: &Path) -> Result<()> {
        if table_exists(nft)? {
            apply_ruleset(nft, &remove_ruleset())?;
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
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}
