//! A walkthrough of the intercept listener, end to end on loopback. It needs
//! no admin rights, no keychain and no Docker. Read it as the demo:
//!
//! ```text
//! cargo test -p agentdesktop-agent intercept::walkthrough -- --nocapture
//! ```
//!
//! The parts:
//! - a fake gateway (plain HTTP on loopback) that records what it receives and
//!   serves a stream, an upload and a WebSocket echo, standing in for
//!   agentgateway and Anthropic;
//! - a separate TLS origin with its own CA, standing in for a host that is not
//!   intercepted;
//! - the intercept listener, with a freshly generated intercept CA;
//! - clients that trust only the CA they are given, the way Claude Desktop
//!   trusts the intercept CA once it is in the System keychain.
//!
//! What it does not show: Claude Desktop itself, the macOS trust store, the
//! `egressProxyUrl` pin, or a real agentgateway. Those were covered by the
//! manual acceptance run.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use super::{
    CredentialSource, GatewayTarget, IDENTITY_HEADER, InterceptCa, InterceptConfig, Interceptor,
    serve,
};

const IDENTITY: &str = "walkthrough-gateway-jwt";
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn step(text: &str) {
    println!("\n== {text}");
}

fn note(text: impl std::fmt::Display) {
    println!("   {text}");
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

struct StaticCredential;

#[async_trait::async_trait]
impl CredentialSource for StaticCredential {
    async fn credential(&self) -> Result<Option<String>, String> {
        Ok(Some(IDENTITY.to_owned()))
    }
}

/// What the fake gateway saw on one request.
#[derive(Clone, Debug)]
struct Seen {
    version: String,
    host: String,
    path: String,
    identity: Vec<String>,
    cookie: Option<String>,
    authorization: Option<String>,
}

/// A gateway on loopback that records requests and counts the connections it
/// accepted. Plain HTTP/1.1, or TLS with HTTP/2 and HTTP/1.1 by ALPN.
struct FakeGateway {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicUsize>,
}

type FixtureBody = BoxBody<Bytes, Infallible>;

fn full(text: impl Into<Bytes>) -> FixtureBody {
    Full::new(text.into()).boxed()
}

impl FakeGateway {
    async fn start(tls: Option<Arc<ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let connections: Arc<AtomicUsize> = Arc::default();
        let (record, count) = (seen.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let record = record.clone();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let record = record.clone();
                        async move { Ok::<_, Infallible>(gateway_route(request, &record).await) }
                    });
                    match tls {
                        None => {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .with_upgrades()
                                .await;
                        }
                        Some(config) => {
                            let Ok(tls) =
                                tokio_rustls::TlsAcceptor::from(config).accept(stream).await
                            else {
                                return;
                            };
                            let _ = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection_with_upgrades(TokioIo::new(tls), service)
                            .await;
                        }
                    }
                });
            }
        });
        Self {
            address,
            seen,
            connections,
        }
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the gateway saw a request")
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

async fn gateway_route(
    mut request: Request<Incoming>,
    record: &Mutex<Vec<Seen>>,
) -> Response<FixtureBody> {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let seen = Seen {
        version: format!("{:?}", request.version()),
        // HTTP/2 carries the host as :authority, which hyper puts in the URI.
        host: header("host")
            .or_else(|| {
                request
                    .uri()
                    .authority()
                    .map(|authority| authority.to_string())
            })
            .unwrap_or_default(),
        path: request.uri().path().to_owned(),
        identity: request
            .headers()
            .get_all(IDENTITY_HEADER)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect(),
        cookie: header("cookie"),
        authorization: header("authorization"),
    };
    let websocket_key = header("sec-websocket-key").unwrap_or_default();
    let path = seen.path.clone();
    record.lock().unwrap().push(seen);
    match path.as_str() {
        "/sse" => {
            // Three events, 250 ms apart: a client that gets them all at once
            // is reading a buffered body.
            let (sender, receiver) =
                tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(4);
            tokio::spawn(async move {
                for index in 0..3 {
                    let event = Frame::data(Bytes::from(format!("data: event {index}\n\n")));
                    if sender.send(Ok(event)).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
            let mut response = Response::new(
                StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed(),
            );
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            response
        }
        "/upload" => {
            let mut body = request.into_body();
            let mut total = 0usize;
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.unwrap().into_data() {
                    total += data.len();
                }
            }
            Response::new(full(total.to_string()))
        }
        "/ws" => {
            let key = websocket_key;
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut stream = TokioIo::new(upgrade.await.unwrap());
                // Echo text frames until the client goes away.
                while let Some((_, payload)) = read_frame(&mut stream).await {
                    if stream.write_all(&frame(&payload, false)).await.is_err() {
                        return;
                    }
                }
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-accept", ws_accept(&key))
                .body(full(""))
                .unwrap()
        }
        _ => Response::new(full("hello from the gateway")),
    }
}

/// A TLS server configuration for "localhost", signed by a CA of its own, and
/// that CA. `alpn` lists the protocols it offers.
fn localhost_tls(alpn: &[&[u8]]) -> (Arc<ServerConfig>, CertificateDer<'static>) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_der = ca_cert.der().clone();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone(), ca_der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
    (Arc::new(config), ca_der)
}

/// A TLS origin for "localhost" with its own CA: a host that is not intercepted.
async fn start_other_origin() -> (SocketAddr, CertificateDer<'static>) {
    let (config, ca_der) = localhost_tls(&[b"http/1.1"]);
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(|_request| async {
                    Ok::<_, Infallible>(Response::new(full("hello from the other origin")))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    (address, ca_der)
}

/// A TLS server that presents one fixed certificate: used to show what a
/// verifier does with leaves signed by the intercept CA.
async fn serve_fixed_certificate(key: Arc<CertifiedKey>) -> SocketAddr {
    #[derive(Debug)]
    struct Fixed(Arc<CertifiedKey>);
    impl ResolvesServerCert for Fixed {
        fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Fixed(key)));
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    address
}

fn plain_gateway(address: SocketAddr) -> GatewayTarget {
    GatewayTarget::http(address.ip().to_string(), address.port())
}

async fn start_listener(
    ca: &Arc<InterceptCa>,
    gateway: GatewayTarget,
    idle_reset: Option<Duration>,
) -> (SocketAddr, Arc<Interceptor>) {
    let interceptor = Interceptor::new(
        InterceptConfig {
            hosts: vec!["claude.ai".to_owned(), "api.anthropic.com".to_owned()],
            gateway,
            idle_reset,
        },
        ca.clone(),
        Arc::new(StaticCredential),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, interceptor.clone()));
    (address, interceptor)
}

/// An HTTPS client that sends everything through `proxy` and trusts only `root`.
fn client(proxy: SocketAddr, root: &CertificateDer<'static>) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
        .tls_certs_only([reqwest::Certificate::from_der(root).unwrap()])
        .build()
        .unwrap()
}

fn tls_connector(root: &CertificateDer<'static>, alpn: &[&[u8]]) -> tokio_rustls::TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// Send `CONNECT target` and return the status code and, on 200, the stream.
async fn connect_through(proxy: SocketAddr, target: &str) -> (u16, TcpStream) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let status = read_status(&mut stream).await;
    (status, stream)
}

/// Read an HTTP/1.1 response head and return its status code.
async fn read_status<S: AsyncRead + Unpin>(stream: &mut S) -> u16 {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn ws_accept(key: &str) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use sha1::{Digest, Sha1};
    STANDARD.encode(Sha1::digest(format!("{key}{WS_GUID}").as_bytes()))
}

fn frame(payload: &[u8], mask: bool) -> Vec<u8> {
    let mut frame = vec![0x81u8];
    let mask_bit = if mask { 0x80 } else { 0x00 };
    assert!(payload.len() < 126, "short frames only");
    frame.push(mask_bit | payload.len() as u8);
    if mask {
        let key = [0x11u8, 0x22, 0x33, 0x44];
        frame.extend_from_slice(&key);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ key[index % 4]),
        );
    } else {
        frame.extend_from_slice(payload);
    }
    frame
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.ok()?;
    let masked = header[1] & 0x80 != 0;
    let len = usize::from(header[1] & 0x7f);
    let mut key = [0u8; 4];
    if masked {
        stream.read_exact(&mut key).await.ok()?;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[index % 4];
        }
    }
    Some((header[0] & 0x0f, payload))
}

async fn tls_over<S: AsyncRead + AsyncWrite + Unpin>(
    connector: &tokio_rustls::TlsConnector,
    name: &str,
    stream: S,
) -> std::io::Result<tokio_rustls::client::TlsStream<S>> {
    connector
        .connect(ServerName::try_from(name.to_owned()).unwrap(), stream)
        .await
}

#[tokio::test]
async fn walkthrough() {
    step("1. An intercept CA is generated for claude.ai and api.anthropic.com");
    let ca = Arc::new(
        InterceptCa::generate(
            "agentdesktop intercept CA (walkthrough)",
            &["claude.ai".to_owned(), "api.anthropic.com".to_owned()],
        )
        .unwrap(),
    );
    note("Name Constraints: only those names and their subdomains; no IP address");
    let intercept_root = ca.certificate_der().clone();

    step("2. A fake gateway and the intercept listener start on loopback");
    let gateway = FakeGateway::start(None).await;
    let (proxy, interceptor) = start_listener(&ca, plain_gateway(gateway.address), None).await;
    note(format!(
        "gateway http://{}  listener http://{proxy}",
        gateway.address
    ));
    let claude = client(proxy, &intercept_root);

    step("3. A request to claude.ai is decrypted and sent to the gateway with the identity header");
    let response = claude
        .get("https://claude.ai/api/bootstrap")
        .header("cookie", "sessionKey=user-session")
        .header("authorization", "Bearer user-token")
        .header(IDENTITY_HEADER, "Bearer spoofed-by-a-local-process")
        .send()
        .await
        .unwrap();
    let version = response.version();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hello from the gateway");
    assert_eq!(
        version,
        reqwest::Version::HTTP_2,
        "the listener serves HTTP/2 by ALPN"
    );
    let seen = gateway.last();
    note(format!(
        "client spoke {version:?}; the gateway saw Host {}, {} identity header(s), cookie and authorization unchanged",
        seen.host,
        seen.identity.len()
    ));
    assert_eq!(seen.host, "claude.ai", "the original host is kept");
    assert_eq!(seen.version, "HTTP/1.1", "a plain gateway gets HTTP/1.1");
    assert_eq!(seen.path, "/api/bootstrap");
    assert_eq!(
        seen.identity,
        vec![format!("Bearer {IDENTITY}")],
        "the spoofed header is replaced"
    );
    assert_eq!(seen.cookie.as_deref(), Some("sessionKey=user-session"));
    assert_eq!(seen.authorization.as_deref(), Some("Bearer user-token"));

    step("4. A server-sent event stream arrives event by event, not buffered");
    let started = Instant::now();
    let mut stream = claude.get("https://claude.ai/sse").send().await.unwrap();
    let mut arrivals = Vec::new();
    while let Some(chunk) = stream.chunk().await.unwrap() {
        if !chunk.is_empty() {
            arrivals.push((
                started.elapsed(),
                String::from_utf8_lossy(&chunk).trim().to_owned(),
            ));
        }
    }
    for (at, event) in &arrivals {
        note(format!("{:>5} ms  {event}", at.as_millis()));
    }
    assert!(arrivals.len() >= 3, "three separate events");
    let spread = arrivals.last().unwrap().0 - arrivals.first().unwrap().0;
    assert!(
        spread >= Duration::from_millis(400),
        "events 250 ms apart, got {spread:?}"
    );

    step("5. A 4 MiB upload to api.anthropic.com streams through to the gateway");
    let body = vec![b'x'; 4 * 1024 * 1024];
    let received = claude
        .post("https://api.anthropic.com/upload")
        .body(body)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    note(format!(
        "the gateway counted {received} bytes for Host {}",
        gateway.last().host
    ));
    assert_eq!(received, (4 * 1024 * 1024).to_string());
    assert_eq!(gateway.last().host, "api.anthropic.com");

    step("6. A WebSocket on claude.ai is upgraded and relays both ways, and stays open while idle");
    let (status, tunnel) = connect_through(proxy, "claude.ai:443").await;
    assert_eq!(status, 200);
    let mut socket = tls_over(
        &tls_connector(&intercept_root, &[b"http/1.1"]),
        "claude.ai",
        tunnel,
    )
    .await
    .unwrap();
    socket
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: claude.ai\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
              Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();
    assert_eq!(read_status(&mut socket).await, 101);
    for (message, idle) in [
        ("hello", Duration::ZERO),
        ("still here", Duration::from_millis(300)),
    ] {
        tokio::time::sleep(idle).await;
        socket
            .write_all(&frame(message.as_bytes(), true))
            .await
            .unwrap();
        let (_, echoed) = read_frame(&mut socket).await.expect("an echo");
        note(format!(
            "sent {message:?} after {} ms idle, got {:?} back",
            idle.as_millis(),
            String::from_utf8_lossy(&echoed)
        ));
        assert_eq!(echoed, message.as_bytes());
    }

    step("7. A host that is not intercepted is tunnelled, not decrypted");
    let (origin, origin_root) = start_other_origin().await;
    let text = client(proxy, &origin_root)
        .get(format!("https://localhost:{}/", origin.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    note(format!(
        "a client that trusts only the origin's own CA got: {text:?}"
    ));
    assert_eq!(text, "hello from the other origin");
    let refused = client(proxy, &intercept_root)
        .get(format!("https://localhost:{}/", origin.port()))
        .send()
        .await;
    note(
        "a client that trusts only the intercept CA is refused: the listener never minted a certificate",
    );
    assert!(refused.is_err());

    step("8. Requests that try to reach another host through an intercepted tunnel are refused");
    let (_, tunnel) = connect_through(proxy, "claude.ai:443").await;
    let mut tls = tls_over(
        &tls_connector(&intercept_root, &[b"http/1.1"]),
        "claude.ai",
        tunnel,
    )
    .await
    .unwrap();
    tls.write_all(b"GET / HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n")
        .await
        .unwrap();
    let mismatch = read_status(&mut tls).await;
    note(format!(
        "CONNECT claude.ai, then Host: api.anthropic.com -> {mismatch}"
    ));
    assert_eq!(mismatch, 421);
    let (_, tunnel) = connect_through(proxy, "claude.ai:443").await;
    let sni = tls_over(
        &tls_connector(&intercept_root, &[b"http/1.1"]),
        "api.anthropic.com",
        tunnel,
    )
    .await;
    note(format!(
        "CONNECT claude.ai, then TLS for api.anthropic.com -> {}",
        if sni.is_err() {
            "handshake refused"
        } else {
            "accepted"
        }
    ));
    assert!(sni.is_err());
    let (other_port, _) = connect_through(proxy, "claude.ai:8443").await;
    note(format!("CONNECT claude.ai:8443 -> {other_port}"));
    assert_eq!(other_port, 403);
    let mut plain = TcpStream::connect(proxy).await.unwrap();
    plain
        .write_all(b"GET http://claude.ai/ HTTP/1.1\r\nHost: claude.ai\r\n\r\n")
        .await
        .unwrap();
    let not_connect = read_status(&mut plain).await;
    note(format!("plain-HTTP proxy request -> {not_connect}"));
    assert_eq!(not_connect, 405);

    step("9. A verifier rejects names outside the intercept CA's Name Constraints");
    let verifier = tls_connector(&intercept_root, &[]);
    for (name, key) in [
        ("claude.ai", ca.leaf("claude.ai").unwrap()),
        ("example.com", ca.leaf_unchecked("example.com").unwrap()),
        ("127.0.0.1", ca.leaf_unchecked("127.0.0.1").unwrap()),
    ] {
        let server = serve_fixed_certificate(key).await;
        let outcome = tls_over(&verifier, name, TcpStream::connect(server).await.unwrap()).await;
        note(format!(
            "leaf for {name:<12} -> {}",
            match &outcome {
                Ok(_) => "accepted".to_owned(),
                Err(error) => format!("rejected ({error})"),
            }
        ));
        assert_eq!(outcome.is_ok(), name == "claude.ai");
    }
    note("this is rustls-webpki; Electron's verifier and Apple's SecTrust are not tested here");

    step("10. With the gateway down, requests fail (fail closed) and the health shows why");
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (down_proxy, down) = start_listener(&ca, plain_gateway(closed), None).await;
    let before = gateway.requests();
    let response = client(down_proxy, &intercept_root)
        .get("https://claude.ai/api/bootstrap")
        .send()
        .await
        .unwrap();
    let health = down.health().snapshot();
    note(format!(
        "status {}, gateway failures {}, last failure: {}",
        response.status(),
        health.gateway_failures,
        health
            .last_failure
            .as_ref()
            .map_or("none", |(_, message)| message.as_str())
    ));
    assert_eq!(response.status(), 502);
    assert!(health.gateway_failures >= 1 && health.last_failure.is_some());
    assert_eq!(
        gateway.requests(),
        before,
        "nothing else answered in the gateway's place"
    );

    step("11. Pooled gateway connections are dropped after an idle gap (or a sleep)");
    let (idle_proxy, _) = start_listener(
        &ca,
        plain_gateway(gateway.address),
        Some(Duration::from_millis(200)),
    )
    .await;
    let idle_client = client(idle_proxy, &intercept_root);
    let quick = gateway.connections();
    for _ in 0..2 {
        idle_client
            .get("https://claude.ai/")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    }
    let quick = gateway.connections() - quick;
    let gapped = gateway.connections();
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        idle_client
            .get("https://claude.ai/")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    }
    let gapped = gateway.connections() - gapped;
    note(format!(
        "two requests back to back: {quick} new gateway connection(s); two after a gap: {gapped}"
    ));
    assert_eq!(
        quick, 1,
        "back-to-back requests reuse the pooled connection"
    );
    assert_eq!(
        gapped, 2,
        "a gap longer than the idle reset starts a new connection"
    );

    step("12. Over TLS, the gateway connection uses HTTP/2 and keeps the original host");
    let (gateway_tls, gateway_root) = localhost_tls(&[b"h2", b"http/1.1"]);
    let tls_gateway = FakeGateway::start(Some(gateway_tls)).await;
    let mut roots = RootCertStore::empty();
    roots.add(gateway_root).unwrap();
    let target = GatewayTarget::https("localhost", tls_gateway.address.port(), roots).unwrap();
    let (tls_proxy, _) = start_listener(&ca, target, None).await;
    let text = client(tls_proxy, &intercept_root)
        .get("https://claude.ai/over-tls")
        .header("cookie", "sessionKey=user-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let seen = tls_gateway.last();
    note(format!(
        "the TLS gateway saw {} with host {} and {} identity header(s)",
        seen.version,
        seen.host,
        seen.identity.len()
    ));
    assert_eq!(text, "hello from the gateway");
    assert_eq!(seen.version, "HTTP/2.0");
    assert_eq!(seen.host, "claude.ai");
    assert_eq!(seen.identity, vec![format!("Bearer {IDENTITY}")]);
    let (_, tunnel) = connect_through(tls_proxy, "claude.ai:443").await;
    let mut socket = tls_over(
        &tls_connector(&intercept_root, &[b"http/1.1"]),
        "claude.ai",
        tunnel,
    )
    .await
    .unwrap();
    socket
        .write_all(
            b"GET /ws HTTP/1.1\r\nHost: claude.ai\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
              Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();
    assert_eq!(read_status(&mut socket).await, 101);
    socket.write_all(&frame(b"over tls", true)).await.unwrap();
    let (_, echoed) = read_frame(&mut socket).await.expect("an echo");
    note(format!(
        "a WebSocket through the TLS gateway went over {} and echoed {:?}",
        tls_gateway.last().version,
        String::from_utf8_lossy(&echoed)
    ));
    assert_eq!(echoed, b"over tls");
    assert_eq!(
        tls_gateway.last().version,
        "HTTP/1.1",
        "upgrades use HTTP/1.1"
    );

    step("13. The main listener's health");
    let health = interceptor.health().snapshot();
    note(format!(
        "forwarded {}, gateway failures {}, tunnels {}, tunnel failures {}",
        health.forwarded, health.gateway_failures, health.tunnels, health.tunnel_failures
    ));
    assert!(health.forwarded >= 4);
    assert_eq!(health.gateway_failures, 0);
    assert!(health.tunnels >= 2);
}
