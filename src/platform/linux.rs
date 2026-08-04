use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use tokio::net::TcpStream;

pub const CAPTURE_TABLE: &str = "agentdesktop";
pub const CAPTURE_SET: &str = "captured_cgroups";

pub fn clear_capture_set_ruleset() -> String {
    format!("flush set inet {CAPTURE_TABLE} {CAPTURE_SET}\n")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSpec {
    cgroup_path: String,
    cgroup_level: usize,
    redirect_port: u16,
}

impl CaptureSpec {
    pub fn new(cgroup: &Path, redirect_port: u16) -> Result<Self> {
        if redirect_port == 0 {
            bail!("capture redirect port must be nonzero");
        }
        if !cgroup.is_absolute() {
            bail!("capture cgroup path must be absolute from the cgroup v2 root");
        }
        let cgroup_components = cgroup
            .components()
            .filter_map(|component| match component {
                Component::RootDir => None,
                Component::Normal(value) => Some(value.to_str()),
                _ => Some(None),
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow::anyhow!("capture cgroup path contains an invalid component"))?;
        if cgroup_components.is_empty() {
            bail!("capture cgroup path must not select the cgroup v2 root");
        }
        if cgroup_components
            .iter()
            .any(|component| component.contains(['"', '\n', '\r']))
        {
            bail!("capture cgroup path contains unsupported characters");
        }
        Ok(Self {
            cgroup_path: cgroup_components.join("/"),
            cgroup_level: cgroup_components.len(),
            redirect_port,
        })
    }

    pub fn ruleset(&self) -> String {
        self.ruleset_with_cgroups(std::slice::from_ref(self))
            .expect("one capture specification is always compatible with itself")
    }

    pub fn ruleset_with_cgroups(&self, cgroups: &[Self]) -> Result<String> {
        self.ensure_compatible(cgroups)?;
        let elements = cgroups
            .iter()
            .map(|spec| format!("\"{}\"", spec.cgroup_path))
            .collect::<Vec<_>>()
            .join(", ");
        let mut ruleset = format!(
            "table inet {CAPTURE_TABLE} {{\n\
             comment \"Agent Desktop ephemeral capture\"\n\
             set {CAPTURE_SET} {{\n\
             typeof socket cgroupv2 level {}\n\
             elements = {{ {elements} }}\n\
             }}\n",
            self.cgroup_level
        );
        writeln!(
            ruleset,
            "chain redirect_tcp {{\n\
             type nat hook output priority -100; policy accept;\n\
             socket cgroupv2 level {} @{CAPTURE_SET} meta l4proto tcp tcp dport 443 counter redirect to :{}\n\
             }}",
            self.cgroup_level,
            self.redirect_port
        )
        .expect("writing to String cannot fail");
        writeln!(
            ruleset,
            "chain deny_quic {{\n\
             type filter hook output priority filter; policy accept;\n\
             socket cgroupv2 level {} @{CAPTURE_SET} meta l4proto udp udp dport 443 counter reject\n\
             }}\n\
             }}",
            self.cgroup_level
        )
        .expect("writing to String cannot fail");
        Ok(ruleset)
    }

    pub fn reconciliation_ruleset(&self, cgroups: &[Self]) -> Result<String> {
        self.ensure_compatible(cgroups)?;
        let mut ruleset = clear_capture_set_ruleset();
        if !cgroups.is_empty() {
            let elements = cgroups
                .iter()
                .map(|spec| format!("\"{}\"", spec.cgroup_path))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                ruleset,
                "add element inet {CAPTURE_TABLE} {CAPTURE_SET} {{ {elements} }}"
            )
            .expect("writing to String cannot fail");
        }
        Ok(ruleset)
    }

    pub fn cgroup_path(&self) -> &str {
        &self.cgroup_path
    }

    pub fn redirect_port(&self) -> u16 {
        self.redirect_port
    }

    fn ensure_compatible(&self, cgroups: &[Self]) -> Result<()> {
        if cgroups.iter().any(|spec| {
            spec.cgroup_level != self.cgroup_level || spec.redirect_port != self.redirect_port
        }) {
            bail!("all capture scopes must use the same cgroup depth and redirect port");
        }
        Ok(())
    }
}

pub fn original_destination(stream: &TcpStream) -> io::Result<SocketAddr> {
    match stream.local_addr()? {
        SocketAddr::V4(_) => Ok(SocketAddr::V4(rustix::net::sockopt::ip_original_dst(
            stream,
        )?)),
        SocketAddr::V6(_) => Ok(SocketAddr::V6(rustix::net::sockopt::ipv6_original_dst(
            stream,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::net::{TcpListener, TcpStream};

    use super::{
        CAPTURE_SET, CAPTURE_TABLE, CaptureSpec, clear_capture_set_ruleset, original_destination,
    };

    #[test]
    fn renders_exact_cgroup_ancestors_tcp_redirect_and_udp_denial() {
        let spec = CaptureSpec::new(
            Path::new("/user.slice/user-1000.slice/app.slice/claude.scope"),
            15001,
        )
        .unwrap();
        let ruleset = spec.ruleset();

        assert!(ruleset.contains(&format!("table inet {CAPTURE_TABLE}")));
        assert!(ruleset.contains("typeof socket cgroupv2 level 4"));
        assert!(
            ruleset
                .contains("elements = { \"user.slice/user-1000.slice/app.slice/claude.scope\" }")
        );
        assert!(ruleset.contains(&format!(
            "socket cgroupv2 level 4 @{CAPTURE_SET} meta l4proto tcp"
        )));
        assert!(ruleset.contains("tcp dport 443 counter redirect to :15001"));
        assert!(ruleset.contains("udp dport 443 counter reject"));
        assert!(!ruleset.contains("policy drop"));
    }

    #[test]
    fn rejects_root_relative_and_unsafe_capture_inputs() {
        assert!(CaptureSpec::new(Path::new("/"), 15001).is_err());
        assert!(CaptureSpec::new(Path::new("user.slice/app.scope"), 15001).is_err());
        assert!(CaptureSpec::new(Path::new("/user.slice/app.scope"), 0).is_err());
        assert!(CaptureSpec::new(Path::new("/user.slice/bad\"scope"), 15001).is_err());
    }

    #[test]
    fn atomically_reconciles_all_cgroup_elements() {
        let first = CaptureSpec::new(Path::new("/user.slice/first.scope"), 15001).unwrap();
        let second = CaptureSpec::new(Path::new("/user.slice/second.scope"), 15001).unwrap();
        assert_eq!(
            first
                .reconciliation_ruleset(&[first.clone(), second])
                .unwrap(),
            format!(
                "flush set inet {CAPTURE_TABLE} {CAPTURE_SET}\n\
                 add element inet {CAPTURE_TABLE} {CAPTURE_SET} {{ \"user.slice/first.scope\", \"user.slice/second.scope\" }}\n"
            )
        );
        assert_eq!(
            first.reconciliation_ruleset(&[]).unwrap(),
            clear_capture_set_ruleset()
        );
    }

    #[test]
    fn rejects_mixed_depths_and_redirect_ports() {
        let spec = CaptureSpec::new(Path::new("/user.slice/first.scope"), 15001).unwrap();
        let different_depth =
            CaptureSpec::new(Path::new("/user.slice/app.slice/second.scope"), 15001).unwrap();
        let different_port =
            CaptureSpec::new(Path::new("/user.slice/second.scope"), 16001).unwrap();

        assert!(spec.reconciliation_ruleset(&[different_depth]).is_err());
        assert!(spec.reconciliation_ruleset(&[different_port]).is_err());
    }

    #[tokio::test]
    async fn reads_destination_from_ipv4_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _client = TcpStream::connect(address).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        assert_eq!(original_destination(&accepted).unwrap(), address);
    }
}
