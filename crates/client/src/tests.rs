use std::path::Path;

use super::connect_error;

#[test]
fn connection_error_suggests_checking_the_daemon() {
    let endpoint = Path::new("/tmp/agentdesktop.sock");

    assert_eq!(
        connect_error(endpoint),
        "connect to /tmp/agentdesktop.sock\nCheck that the Agentdesktop daemon is running."
    );
}

#[cfg(unix)]
mod unix {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde::{Serialize, Serializer};
    use serde_json::{Value, json};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::time::timeout;

    use crate::{connect_error, get, post_json};

    const TIMEOUT: Duration = Duration::from_secs(5);
    static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);

    struct Socket {
        endpoint: PathBuf,
    }

    impl Socket {
        fn new() -> Self {
            // Keep paths short even when macOS's default temporary directory is long.
            for _ in 0..100 {
                let directory = Path::new("/tmp").join(format!(
                    "adc-{}-{}",
                    std::process::id(),
                    NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&directory) {
                    Ok(()) => {
                        return Self {
                            endpoint: directory.join("s"),
                        };
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create socket directory: {error}"),
                }
            }
            panic!("could not allocate an isolated socket directory");
        }

        async fn exchange<T>(
            &self,
            response: &[u8],
            client: impl Future<Output = anyhow::Result<T>>,
        ) -> (ObservedRequest, anyhow::Result<T>) {
            let listener = UnixListener::bind(&self.endpoint).expect("bind test socket");
            timeout(TIMEOUT, async {
                tokio::join!(serve(&listener, response), client)
            })
            .await
            .expect("local HTTP exchange timed out")
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.endpoint);
            let _ = std::fs::remove_dir(self.endpoint.parent().unwrap());
        }
    }

    struct ObservedRequest {
        request_line: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    async fn read_more(stream: &UnixStream, bytes: &mut Vec<u8>) {
        let mut buffer = [0; 1024];
        loop {
            stream.readable().await.expect("request readable");
            match stream.try_read(&mut buffer) {
                Ok(count) => {
                    assert_ne!(count, 0, "request ended before its declared length");
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() <= 16 * 1024, "test request too large");
                    return;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
                Err(error) => panic!("read request: {error}"),
            }
        }
    }

    async fn serve(listener: &UnixListener, mut response: &[u8]) -> ObservedRequest {
        let (stream, _) = listener.accept().await.expect("accept test client");
        let mut bytes = Vec::new();
        let header_end = loop {
            read_more(&stream, &mut bytes).await;
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let mut lines = std::str::from_utf8(&bytes[..header_end])
            .expect("HTTP headers")
            .split("\r\n");
        let request_line = lines.next().unwrap().to_owned();
        let headers: BTreeMap<_, _> = lines
            .take_while(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line.split_once(':').expect("header separator");
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        assert!(!headers.contains_key("transfer-encoding"));
        let length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().expect("content length"))
            .unwrap_or(0);
        // Read the request frame, not EOF: the client needs the response to finish.
        while bytes.len() < header_end + length {
            read_more(&stream, &mut bytes).await;
        }
        assert_eq!(bytes.len(), header_end + length);
        while !response.is_empty() {
            stream.writable().await.expect("response writable");
            match stream.try_write(response) {
                Ok(count) => {
                    assert_ne!(count, 0, "response write made no progress");
                    response = &response[count..];
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
                Err(error) => panic!("write response: {error}"),
            }
        }
        ObservedRequest {
            request_line,
            headers,
            body: bytes[header_end..].to_vec(),
        }
    }

    #[tokio::test]
    async fn get_sends_empty_request_and_decodes_json() {
        let socket = Socket::new();
        let (request, result) = socket
            .exchange(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
                get::<Value>(&socket.endpoint, "/v1/status?detail=true"),
            )
            .await;

        assert_eq!(request.request_line, "GET /v1/status?detail=true HTTP/1.1");
        assert_eq!(request.headers["host"], "localhost");
        assert!(!request.headers.contains_key("content-type"));
        assert!(request.body.is_empty());
        assert_eq!(result.unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn post_sends_json_and_accepts_empty_or_non_json_success() {
        for response in [
            b"HTTP/1.1 204 No Content\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json".as_slice(),
        ] {
            let socket = Socket::new();
            let value = json!({"name": "caf\u{e9}", "enabled": true});
            let (request, result) = socket
                .exchange(
                    response,
                    post_json(&socket.endpoint, "/v1/settings?apply=true", &value),
                )
                .await;

            result.unwrap();
            assert_eq!(
                request.request_line,
                "POST /v1/settings?apply=true HTTP/1.1"
            );
            assert_eq!(request.headers["host"], "localhost");
            assert_eq!(request.headers["content-type"], "application/json");
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                value
            );
        }
    }

    #[tokio::test]
    async fn non_success_status_and_lossy_body_errors_match() {
        let response = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 8\r\n\r\ndenied \xff";
        let socket = Socket::new();
        let (_, get_result) = socket
            .exchange(response, get::<Value>(&socket.endpoint, "/denied"))
            .await;
        let socket = Socket::new();
        let (_, post_result) = socket
            .exchange(response, post_json(&socket.endpoint, "/denied", &true))
            .await;

        let expected = "daemon returned 403 Forbidden: denied \u{fffd}";
        assert_eq!(get_result.unwrap_err().to_string(), expected);
        assert_eq!(post_result.unwrap_err().to_string(), expected);
    }

    #[tokio::test]
    async fn malformed_get_json_has_decode_context() {
        let socket = Socket::new();
        let (_, result) = socket
            .exchange(
                b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json",
                get::<Value>(&socket.endpoint, "/malformed"),
            )
            .await;

        assert_eq!(result.unwrap_err().to_string(), "decode daemon response");
    }

    #[tokio::test]
    async fn interrupted_responses_keep_transport_context_for_both_methods() {
        for (response, expected) in [
            (b"".as_slice(), "send request"),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort".as_slice(),
                "read response",
            ),
            (
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 20\r\n\r\nshort".as_slice(),
                "read response",
            ),
        ] {
            let socket = Socket::new();
            let (_, get_result) = socket
                .exchange(response, get::<Value>(&socket.endpoint, "/interrupted"))
                .await;
            let socket = Socket::new();
            let (_, post_result) = socket
                .exchange(response, post_json(&socket.endpoint, "/interrupted", &true))
                .await;

            assert_eq!(get_result.unwrap_err().to_string(), expected);
            assert_eq!(post_result.unwrap_err().to_string(), expected);
        }
    }

    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    #[tokio::test]
    async fn connection_failures_precede_request_building_and_serialization() {
        let socket = Socket::new();
        let expected = connect_error(&socket.endpoint);
        let get_error = timeout(TIMEOUT, get::<Value>(&socket.endpoint, "invalid uri"))
            .await
            .expect("GET timed out")
            .unwrap_err();
        let post_error = timeout(
            TIMEOUT,
            post_json(&socket.endpoint, "invalid uri", &Unserializable),
        )
        .await
        .expect("POST timed out")
        .unwrap_err();

        assert_eq!(get_error.to_string(), expected);
        assert_eq!(post_error.to_string(), expected);
    }

    #[tokio::test]
    async fn post_serialization_failure_precedes_invalid_uri() {
        let socket = Socket::new();
        let _listener = UnixListener::bind(&socket.endpoint).expect("bind test socket");
        let error = timeout(
            TIMEOUT,
            post_json(&socket.endpoint, "invalid uri", &Unserializable),
        )
        .await
        .expect("POST timed out")
        .unwrap_err();

        assert_eq!(error.to_string(), "encode request body");
    }
}
