#[cfg(unix)]
use std::net::{SocketAddr, TcpListener};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
struct Connector(Child);

#[cfg(unix)]
impl Drop for Connector {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
impl Connector {
    fn shutdown(&mut self) {
        let status = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        self.0.wait().unwrap();
    }
}

#[cfg(unix)]
fn unused_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn exits_when_owned_local_gateway_exits() {
    let fixture_dir = std::env::temp_dir().join(format!("agentdesktop-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let gateway = fixture_dir.join("agentgateway");
    let gateway_config = fixture_dir.join("config.yaml");
    std::fs::write(&gateway, "#!/bin/sh\nexit 23\n").unwrap();
    std::fs::set_permissions(&gateway, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&gateway_config, "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
        .args([
            "serve",
            "--mode",
            "standalone",
            "--listen",
            &unused_address().to_string(),
            "--upstream",
            "http://127.0.0.1:4000",
            "--gateway-binary",
            gateway.to_str().unwrap(),
            "--gateway-config",
            gateway_config.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    std::fs::remove_dir_all(fixture_dir).unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("local Agent Gateway exited during startup with exit status: 23")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn waits_for_owned_local_gateway_before_listening() {
    let fixture_dir =
        std::env::temp_dir().join(format!("agentdesktop-readiness-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let gateway = fixture_dir.join("agentgateway");
    let gateway_config = fixture_dir.join("config.yaml");
    std::fs::write(&gateway, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&gateway, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&gateway_config, "").unwrap();

    let listen = unused_address();
    let status_listen = unused_address();
    let upstream = unused_address();
    let mut connector = Connector(
        Command::new(env!("CARGO_BIN_EXE_agentdesktop"))
            .args([
                "serve",
                "--mode",
                "standalone",
                "--listen",
                &listen.to_string(),
                "--status-listen",
                &status_listen.to_string(),
                "--upstream",
                &format!("http://{upstream}"),
                "--gateway-binary",
                gateway.to_str().unwrap(),
                "--gateway-config",
                gateway_config.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(tokio::net::TcpStream::connect(listen).await.is_err());

    let _gateway_listener = tokio::net::TcpListener::bind(upstream).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tokio::net::TcpStream::connect(listen).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connector did not listen after Agent Gateway became reachable");

    connector.shutdown();
    std::fs::remove_dir_all(fixture_dir).unwrap();
}
