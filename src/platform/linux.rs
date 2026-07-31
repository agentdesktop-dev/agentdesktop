use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use tokio::net::TcpStream;

pub const CAPTURE_TABLE: &str = "agentgateway_edge";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSpec {
    cgroup_components: Vec<String>,
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
            cgroup_components: cgroup_components.into_iter().map(str::to_owned).collect(),
            redirect_port,
        })
    }

    pub fn ruleset(&self) -> String {
        let selector = self
            .cgroup_components
            .iter()
            .enumerate()
            .map(|(index, component)| {
                format!("socket cgroupv2 level {} \"{component}\"", index + 1)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut ruleset = format!(
            "table inet {CAPTURE_TABLE} {{\n\
             \tcomment \"Agent Gateway Edge Connector ephemeral capture\"\n"
        );
        writeln!(
            ruleset,
            "\tchain redirect_tcp {{\n\
             		type nat hook output priority -100; policy accept;\n\
             		{selector} meta l4proto tcp tcp dport 443 counter redirect to :{}\n\
             \t}}",
            self.redirect_port
        )
        .expect("writing to String cannot fail");
        writeln!(
            ruleset,
            "\tchain deny_quic {{\n\
             \t\ttype filter hook output priority filter; policy accept;\n\
             		{selector} meta l4proto udp udp dport 443 counter reject\n\
             \t}}\n\
             }}"
        )
        .expect("writing to String cannot fail");
        ruleset
    }

    pub fn installation_ruleset(&self, replace_existing: bool) -> String {
        if replace_existing {
            remove_ruleset() + &self.ruleset()
        } else {
            self.ruleset()
        }
    }
}

pub fn remove_ruleset() -> String {
    format!("delete table inet {CAPTURE_TABLE}\n")
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

    use super::{CAPTURE_TABLE, CaptureSpec, original_destination, remove_ruleset};

    #[test]
    fn renders_exact_cgroup_ancestors_tcp_redirect_and_udp_denial() {
        let spec = CaptureSpec::new(
            Path::new("/user.slice/user-1000.slice/app.slice/claude.scope"),
            15001,
        )
        .unwrap();
        let ruleset = spec.ruleset();

        assert!(ruleset.contains(&format!("table inet {CAPTURE_TABLE}")));
        assert!(ruleset.contains("socket cgroupv2 level 1 \"user.slice\""));
        assert!(ruleset.contains("socket cgroupv2 level 4 \"claude.scope\""));
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
    fn removal_only_destroys_the_owned_table() {
        assert_eq!(
            remove_ruleset(),
            format!("delete table inet {CAPTURE_TABLE}\n")
        );
    }

    #[test]
    fn replacement_is_one_delete_and_create_transaction() {
        let spec = CaptureSpec::new(Path::new("/user.slice/claude.scope"), 15001).unwrap();
        let ruleset = spec.installation_ruleset(true);

        assert!(ruleset.starts_with(&format!("delete table inet {CAPTURE_TABLE}\n")));
        assert_eq!(
            ruleset
                .matches(&format!("table inet {CAPTURE_TABLE}"))
                .count(),
            2
        );
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
