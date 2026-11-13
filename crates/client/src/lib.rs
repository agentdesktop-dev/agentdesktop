use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Request, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

#[cfg(unix)]
type LocalStream = UnixStream;
#[cfg(windows)]
type LocalStream = NamedPipeClient;

fn connect_error(endpoint: &Path) -> String {
    format!(
        "connect to {}\nCheck that the Agentdesktop daemon is running.",
        endpoint.display()
    )
}

#[cfg(unix)]
async fn connect(endpoint: &Path) -> anyhow::Result<LocalStream> {
    UnixStream::connect(endpoint)
        .await
        .with_context(|| connect_error(endpoint))
}

/// The one-line refusal for a pipe that is not owned by this user or by
/// SYSTEM: printed verbatim by the wrap launchers via `run --wait`, so it
/// stays a single line.
#[cfg_attr(not(windows), allow(dead_code))]
fn foreign_server_error(endpoint: &Path) -> String {
    format!(
        "refusing to use {}: the pipe is not owned by your user or SYSTEM",
        endpoint.display()
    )
}

/// The owner-SID acceptance rule: the pipe object's owner must be
/// the caller's own user SID or SYSTEM (`S-1-5-18`, the system daemon).
/// Pure and platform-independent so the comparison is unit-tested with
/// injected SIDs; the Windows connect path feeds it the pipe object's owner
/// read via `GetSecurityInfo`.
#[cfg_attr(not(windows), allow(dead_code))]
fn server_owner_allowed(owner_sid: &str, own_sid: &str) -> bool {
    /// `S-1-5-18` (LocalSystem): the system daemon.
    const SYSTEM_SID: &str = "S-1-5-18";
    owner_sid == own_sid || owner_sid == SYSTEM_SID
}

#[cfg(windows)]
async fn connect(endpoint: &Path) -> anyhow::Result<LocalStream> {
    // CreateFile returns ERROR_PIPE_BUSY when all server instances are in use.
    // A fresh instance should become available as soon as the daemon accepts
    // one of them, so follow the retry pattern recommended by Tokio.
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        // SECURITY_IDENTIFICATION (with SECURITY_SQOS_PRESENT, which Tokio
        // adds) keeps the server from impersonating this client's token.
        // Tokio's default, kept explicit: the daemon pipe names are
        // predictable, so the server at the other end is untrusted until
        // verified below.
        match ClientOptions::new()
            .security_qos_flags(windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION)
            .open(endpoint.as_os_str())
        {
            Ok(client) => {
                server_identity::verify(&client, endpoint)?;
                return Ok(client);
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| connect_error(endpoint));
            }
        }
    }
}

/// Verifies, before any request bytes are written, that the pipe is owned by
/// this user or by SYSTEM. The per-user pipe name is predictable, so on a
/// multi-user host another local user could create a pipe under this user's
/// name before the real daemon binds it; the daemon's own pipe ACL protects
/// the daemon from foreign clients, never a client from a foreign server.
///
/// The check reads the PIPE OBJECT's owner SID (`GetSecurityInfo` on the
/// opened handle), not the server process's token: opening a SYSTEM
/// process or its token is denied to a standard user, so a process-token
/// read would refuse every standard-user call against the system daemon.
/// `READ_CONTROL` arrives with the `GENERIC_READ` the open already needs,
/// so exactly the clients the pipe's ACL admits can read the owner. A pipe
/// squatted by another local user is owned by that user and refused.
#[cfg(windows)]
mod server_identity {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use anyhow::{Context, bail};
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::{
        Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT},
        GetTokenInformation, OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The pipe object's owner SID must satisfy [`super::server_owner_allowed`]
    /// (the caller's own user SID, or SYSTEM). Anything else, including an
    /// unreadable owner, is refused with a single line before any request is
    /// written (fail closed).
    pub(super) fn verify(client: &NamedPipeClient, endpoint: &Path) -> anyhow::Result<()> {
        let context = || format!("verify the owner of the pipe at {}", endpoint.display());
        let owner_sid = pipe_owner_sid(client).with_context(context)?;
        // SAFETY: GetCurrentProcess returns the process pseudo-handle,
        // which is not closed.
        let own_sid = process_user_sid(unsafe { GetCurrentProcess() }).with_context(context)?;
        if !super::server_owner_allowed(&owner_sid, &own_sid) {
            bail!("{}", super::foreign_server_error(endpoint));
        }
        Ok(())
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by this module.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The owner SID of the pipe object behind the opened client handle.
    fn pipe_owner_sid(client: &NamedPipeClient) -> anyhow::Result<String> {
        let mut owner: *mut c_void = std::ptr::null_mut();
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        // SAFETY: a connected client pipe handle (GENERIC_READ includes
        // READ_CONTROL), null out-parameters for the unrequested parts, and
        // a descriptor out-parameter the call allocates.
        let status = unsafe {
            GetSecurityInfo(
                client.as_raw_handle() as HANDLE,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32))
                .context("read the pipe owner");
        }
        // SAFETY: on success `owner` points into `descriptor`, a single
        // LocalAlloc block the caller frees; the SID is read before the free.
        let sid = unsafe { sid_string(owner) };
        // SAFETY: the descriptor was allocated by GetSecurityInfo.
        unsafe { LocalFree(descriptor) };
        sid
    }

    fn process_user_sid(process: HANDLE) -> anyhow::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: valid process handle and out-parameter.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("open the process token");
        }
        let token = Handle(token);
        let mut length = 0u32;
        // SAFETY: a size query; the null buffer is allowed with length 0.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut length);
        }
        if length == 0 {
            bail!("TokenUser size query failed");
        }
        let mut buffer = vec![0u8; length as usize];
        // SAFETY: the buffer is `length` bytes, as the size query asked for.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                length,
                &mut length,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("read the token user");
        }
        // SAFETY: GetTokenInformation(TokenUser) fills a TOKEN_USER at the
        // buffer start; the SID it names points into `buffer`, which
        // outlives the call.
        unsafe { sid_string((*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
    }

    /// The string form of a SID pointer (`ConvertSidToStringSidW`).
    ///
    /// # Safety
    /// `sid` must point at a valid SID that outlives the call.
    unsafe fn sid_string(sid: *mut c_void) -> anyhow::Result<String> {
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: the caller guarantees `sid`; `text` is a valid out-parameter.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(std::io::Error::last_os_error()).context("convert SID to string");
        }
        // SAFETY: a null-terminated UTF-16 string allocated by the system.
        Ok(unsafe {
            let mut len = 0usize;
            while *text.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast::<c_void>());
            sid
        })
    }
}

pub async fn get<T>(endpoint: &Path, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    request(endpoint, "GET", path).await
}

/// Sends a POST without a request body and decodes the JSON response.
pub async fn post<T>(endpoint: &Path, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    request(endpoint, "POST", path).await
}

async fn request<T>(endpoint: &Path, method: &str, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();

    if !status.is_success() {
        bail!(
            "daemon returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    serde_json::from_slice(&body).context("decode daemon response")
}

pub async fn post_json<T>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
{
    send_json(endpoint, path, value).await.map(drop)
}

/// Sends a POST with a JSON request body and decodes the JSON response.
pub async fn post_json_decode<T, R>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<R>
where
    T: Serialize,
    R: DeserializeOwned,
{
    let body = send_json(endpoint, path, value).await?;
    serde_json::from_slice(&body).context("decode daemon response")
}

async fn send_json<T>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<Bytes>
where
    T: Serialize,
{
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let body = serde_json::to_vec(value).context("encode request body")?;
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();
    if !status.is_success() {
        bail!(
            "daemon returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    Ok(body)
}

/// A non-2xx response from the daemon's local API.
#[derive(Debug)]
pub struct StatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for StatusError {
    /// `daemon returned <status> <reason>: <body>`, the shape of the other
    /// helpers' errors, so a caller can show it as is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon returned {}", self.status)?;
        if let Some(reason) = hyper::StatusCode::from_u16(self.status)
            .ok()
            .and_then(|status| status.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        write!(f, ": {}", self.body)
    }
}

impl std::error::Error for StatusError {}

/// Sends a request with an optional JSON body and returns the status and the
/// raw response body.
async fn exchange(
    endpoint: &Path,
    method: &str,
    path: &str,
    json: Option<Vec<u8>>,
) -> anyhow::Result<(hyper::StatusCode, Bytes)> {
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if json.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = request.body(Full::new(Bytes::from(json.unwrap_or_default())))?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();
    Ok((status, body))
}

fn status_error(status: hyper::StatusCode, body: &Bytes) -> anyhow::Error {
    StatusError {
        status: status.as_u16(),
        body: String::from_utf8_lossy(body).into_owned(),
    }
    .into()
}

/// Sends a POST with a JSON body and decodes the JSON response. A non-2xx
/// response is an error carrying a [`StatusError`], so the caller can map
/// statuses to messages.
pub async fn post_json_response<B, T>(endpoint: &Path, path: &str, body: &B) -> anyhow::Result<T>
where
    B: Serialize,
    T: DeserializeOwned,
{
    let json = serde_json::to_vec(body).context("encode request body")?;
    let (status, body) = exchange(endpoint, "POST", path, Some(json)).await?;
    if !status.is_success() {
        return Err(status_error(status, &body));
    }
    serde_json::from_slice(&body).context("decode daemon response")
}

/// Sends a DELETE; any 2xx is success and the body is not decoded. A non-2xx
/// response is an error carrying a [`StatusError`].
pub async fn delete(endpoint: &Path, path: &str) -> anyhow::Result<()> {
    let (status, body) = exchange(endpoint, "DELETE", path, None).await?;
    if !status.is_success() {
        return Err(status_error(status, &body));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::connect_error;

    /// The refusal for a pipe owned by another user is exactly one line
    /// (the wrap launchers print it verbatim) and names the endpoint.
    #[test]
    fn foreign_server_refusal_is_one_line_and_names_the_endpoint() {
        let line = super::foreign_server_error(Path::new(r"\\.\pipe\agentdesktop-user-alice"));
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.contains(r"\\.\pipe\agentdesktop-user-alice"), "{line}");
        assert!(line.starts_with("refusing"), "{line}");
    }

    /// DV6-6 owner rule with injected SIDs: the caller's own user and SYSTEM
    /// are accepted; another user (a squatted pipe), BUILTIN\Administrators
    /// and the empty string are refused. The user-vs-system cases cover both
    /// deployments: a user daemon's pipe is owned by the user, the system
    /// daemon's by SYSTEM.
    #[test]
    fn a_pipe_owner_is_accepted_only_for_the_callers_user_or_system() {
        const ME: &str = "S-1-5-21-1-2-3-1001";
        assert!(super::server_owner_allowed(ME, ME));
        assert!(super::server_owner_allowed("S-1-5-18", ME));
        // SYSTEM as the CALLER still accepts only its own SID or SYSTEM.
        assert!(super::server_owner_allowed("S-1-5-18", "S-1-5-18"));
        for foreign in [
            "S-1-5-21-1-2-3-1002", // another local user: the squat case
            "S-1-5-32-544",        // BUILTIN\Administrators
            "S-1-5-19",            // LOCAL SERVICE
            "",
        ] {
            assert!(!super::server_owner_allowed(foreign, ME), "{foreign}");
        }
    }

    #[test]
    fn connection_error_suggests_checking_the_daemon() {
        let endpoint = Path::new("/tmp/agentdesktop.sock");

        assert_eq!(
            connect_error(endpoint),
            "connect to /tmp/agentdesktop.sock\nCheck that the Agentdesktop daemon is running."
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_error_display_names_the_status_and_the_body() {
        let error = super::StatusError {
            status: 404,
            body: "no session abc".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("404"), "{text}");
        assert!(text.contains("no session abc"), "{text}");
    }

    #[cfg(target_os = "linux")]
    mod socket {
        use std::{
            convert::Infallible,
            path::PathBuf,
            sync::{Arc, Mutex},
        };

        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::{Request, Response, service::service_fn};
        use hyper_util::rt::TokioIo;
        use serde::{Deserialize, Serialize};
        use tokio::net::UnixListener;

        use crate::{StatusError, delete, post_json_response};

        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Payload {
            value: String,
        }

        /// (method, path, content-type, body)
        type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

        /// A daemon stand-in on a Unix socket that answers every request with
        /// `status` and `body`.
        struct FakeDaemon {
            socket: PathBuf,
            seen: Seen,
            task: tokio::task::JoinHandle<()>,
        }

        impl Drop for FakeDaemon {
            fn drop(&mut self) {
                self.task.abort();
                let _ = std::fs::remove_file(&self.socket);
            }
        }

        fn fake_daemon(name: &str, status: u16, canned: &'static str) -> FakeDaemon {
            let socket = std::env::temp_dir().join(format!(
                "agentdesktop-client-test-{}-{name}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let seen: Seen = Arc::default();
            let record = seen.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let record = record.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(stream),
                                service_fn(move |request: Request<hyper::body::Incoming>| {
                                    let record = record.clone();
                                    async move {
                                        let method = request.method().to_string();
                                        let path = request.uri().path().to_owned();
                                        let content_type = request
                                            .headers()
                                            .get("content-type")
                                            .and_then(|value| value.to_str().ok())
                                            .unwrap_or("")
                                            .to_owned();
                                        let body =
                                            request.into_body().collect().await.unwrap().to_bytes();
                                        record.lock().unwrap().push((
                                            method,
                                            path,
                                            content_type,
                                            String::from_utf8_lossy(&body).into_owned(),
                                        ));
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .body(Full::new(Bytes::from_static(
                                                    canned.as_bytes(),
                                                )))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            FakeDaemon { socket, seen, task }
        }

        #[tokio::test]
        async fn post_json_response_sends_json_and_decodes_the_reply() {
            let daemon = fake_daemon("post-ok", 201, r#"{"value":"created"}"#);
            let reply: Payload = post_json_response(
                &daemon.socket,
                "/v1/things",
                &Payload {
                    value: "asked".into(),
                },
            )
            .await
            .unwrap();
            assert_eq!(
                reply,
                Payload {
                    value: "created".into()
                }
            );
            let seen = daemon.seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, "POST");
            assert_eq!(seen[0].1, "/v1/things");
            assert!(seen[0].2.starts_with("application/json"), "{}", seen[0].2);
            assert_eq!(seen[0].3, r#"{"value":"asked"}"#);
        }

        #[tokio::test]
        async fn post_json_response_carries_a_status_error_for_non_2xx() {
            for (status, body) in [(409u16, "the proxy is not running"), (429, "too many")] {
                let daemon = fake_daemon(&format!("post-{status}"), status, body);
                let error = post_json_response::<_, Payload>(
                    &daemon.socket,
                    "/v1/things",
                    &Payload { value: "x".into() },
                )
                .await
                .expect_err("a non-2xx response is an error");
                let status_error = error
                    .downcast_ref::<StatusError>()
                    .unwrap_or_else(|| panic!("no StatusError in {error:#}"));
                assert_eq!(status_error.status, status);
                assert_eq!(status_error.body, body);
            }
        }

        #[tokio::test]
        async fn delete_succeeds_on_any_2xx_without_decoding_the_body() {
            for (status, body) in [(204u16, ""), (200, "not json at all")] {
                let daemon = fake_daemon(&format!("delete-{status}"), status, body);
                delete(&daemon.socket, "/v1/things/abc").await.unwrap();
                let seen = daemon.seen.lock().unwrap().clone();
                assert_eq!(seen.len(), 1);
                assert_eq!(seen[0].0, "DELETE");
                assert_eq!(seen[0].1, "/v1/things/abc");
            }
        }

        #[tokio::test]
        async fn delete_carries_a_status_error_for_non_2xx() {
            let daemon = fake_daemon("delete-404", 404, "no session abc");
            let error = delete(&daemon.socket, "/v1/things/abc")
                .await
                .expect_err("a 404 is an error");
            let status_error = error
                .downcast_ref::<StatusError>()
                .unwrap_or_else(|| panic!("no StatusError in {error:#}"));
            assert_eq!(status_error.status, 404);
            assert_eq!(status_error.body, "no session abc");
        }
    }
}
