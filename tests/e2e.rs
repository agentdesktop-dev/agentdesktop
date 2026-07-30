use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Connector(Child);

impl Drop for Connector {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[tokio::test]
async fn fails_closed_when_agent_gateway_is_unavailable() {
    let listen = unused_address();
    let upstream = unused_address();
    let _connector = Connector(
        Command::new(env!("CARGO_BIN_EXE_agentgateway-edge-connector"))
            .args([
                "--listen",
                &listen.to_string(),
                "--upstream",
                &format!("http://{upstream}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let client = reqwest::Client::new();
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client
                .post(format!("http://{listen}/v1/messages"))
                .send()
                .await
            {
                Ok(response) => break response,
                Err(error) if error.is_connect() => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected connector error: {error}"),
            }
        }
    })
    .await
    .expect("connector did not start");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers()["x-agentgateway-edge-error"],
        "upstream-unavailable"
    );
    assert_eq!(
        response.text().await.unwrap(),
        "agent gateway unavailable\n"
    );
}
