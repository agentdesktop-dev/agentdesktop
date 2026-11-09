//! Fleet connection admission and idle handling.
//!
//! Client certificates are optional on the fleet listener, so anyone can finish
//! the TLS handshake. Without more, a client could then hold a connection open
//! for free: by never sending the HTTP/2 preface (nothing times out before it),
//! or by sending it and answering keepalive PINGs without ever making a request.
//! Enough such connections would fill the connection caps and lock devices out.
//!
//! So each connection's request activity is tracked. A connection is closed if
//! it makes no request soon after it is accepted, or stays idle too long after
//! its last request finishes. A connection with a request in progress, including
//! an open `Connect` stream, is never idle. When the total cap is reached, the
//! longest-idle connection is closed to make room instead of refusing the new
//! one, so idle connections cannot crowd out devices that are using theirs.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures_core::Stream;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::oneshot,
    time::{Instant, Sleep},
};
use tonic::{
    body::Body,
    codegen::{Service, http},
    transport::server::{Connected, TcpConnectInfo, TlsConnectInfo},
};
use tower_layer::Layer;

use super::FleetLimits;

/// Open fleet connections and their request activity, shared by the listener
/// (which admits, times out and evicts connections) and the request layer (which
/// marks connections busy).
#[derive(Clone)]
pub struct Connections {
    shared: Arc<Shared>,
}

struct Shared {
    max_connections: usize,
    connections_per_address: usize,
    first_request: Duration,
    idle: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    open: HashMap<SocketAddr, Arc<Activity>>,
    per_address: HashMap<IpAddr, usize>,
    /// Connections in the order they became idle. An entry is stale once its
    /// connection makes a request or closes; `Activity::idle_epoch` tells.
    idle: VecDeque<(SocketAddr, u64)>,
}

/// One connection's request activity.
struct Activity {
    in_flight: AtomicUsize,
    /// Advanced each time the connection becomes idle.
    idle_epoch: AtomicU64,
    /// Whether the connection has made any request.
    used: AtomicBool,
    /// When the connection was accepted, or last started or finished a request.
    last_active: Mutex<Instant>,
    /// Closes the connection when it is evicted.
    evict: Mutex<Option<oneshot::Sender<()>>>,
    /// Wakes the connection so it recomputes its idle deadline when its last
    /// request finishes.
    waker: Mutex<Option<Waker>>,
}

impl Activity {
    fn touch(&self) {
        *self
            .last_active
            .lock()
            .expect("connection activity lock poisoned") = Instant::now();
    }

    /// When an idle connection is due to close, or `None` while it is busy.
    fn idle_deadline(&self, shared: &Shared) -> Option<Instant> {
        if self.in_flight.load(Ordering::Acquire) > 0 {
            return None;
        }
        let allowed = if self.used.load(Ordering::Acquire) {
            shared.idle
        } else {
            shared.first_request
        };
        Some(
            *self
                .last_active
                .lock()
                .expect("connection activity lock poisoned")
                + allowed,
        )
    }
}

impl State {
    /// Closes the longest-idle connection. Returns false when every open
    /// connection has a request in progress.
    fn evict_idle(&mut self) -> bool {
        while let Some((peer, epoch)) = self.idle.pop_front() {
            let Some(activity) = self.open.get(&peer) else {
                continue;
            };
            if activity.in_flight.load(Ordering::Acquire) > 0
                || activity.idle_epoch.load(Ordering::Acquire) != epoch
            {
                continue;
            }
            if let Some(evict) = activity
                .evict
                .lock()
                .expect("connection eviction lock poisoned")
                .take()
            {
                let _ = evict.send(());
                return true;
            }
        }
        false
    }

    fn mark_idle(&mut self, peer: SocketAddr, epoch: u64) {
        self.idle.push_back((peer, epoch));
        // Drop stale entries once they dominate, so the queue stays bounded by
        // the number of open connections.
        if self.idle.len() > 2 * self.open.len() + 64 {
            let open = &self.open;
            self.idle.retain(|(peer, epoch)| {
                open.get(peer).is_some_and(|activity| {
                    activity.in_flight.load(Ordering::Acquire) == 0
                        && activity.idle_epoch.load(Ordering::Acquire) == *epoch
                })
            });
        }
    }
}

impl Connections {
    pub(super) fn new(limits: &FleetLimits) -> Self {
        Self {
            shared: Arc::new(Shared {
                max_connections: limits.max_connections,
                connections_per_address: limits.connections_per_address,
                first_request: limits.first_request,
                idle: limits.idle_connection,
                state: Mutex::default(),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .expect("connection registry lock poisoned")
    }

    /// Accepted connections, admitted or closed by the caps.
    pub fn incoming<S>(&self, incoming: S) -> LimitedIncoming<S> {
        LimitedIncoming {
            incoming,
            connections: self.clone(),
        }
    }

    /// A layer that marks a connection busy while a request (or stream) is open.
    pub fn track_requests(&self) -> TrackRequests {
        TrackRequests {
            connections: self.clone(),
        }
    }

    fn admit(&self, stream: TcpStream) -> Option<LimitedConnection> {
        let peer = stream.peer_addr().ok()?;
        let mut state = self.state();
        if state.open.len() >= self.shared.max_connections && !state.evict_idle() {
            tracing::debug!(
                "fleet connection limit reached and no connection is idle; closing connection"
            );
            return None;
        }
        let count = state.per_address.entry(peer.ip()).or_default();
        if *count >= self.shared.connections_per_address {
            tracing::debug!(address = %peer.ip(), "per-address fleet connection limit reached; closing connection");
            return None;
        }
        *count += 1;
        let now = Instant::now();
        let (evict, evicted) = oneshot::channel();
        let activity = Arc::new(Activity {
            in_flight: AtomicUsize::new(0),
            idle_epoch: AtomicU64::new(0),
            used: AtomicBool::new(false),
            last_active: Mutex::new(now),
            evict: Mutex::new(Some(evict)),
            waker: Mutex::new(None),
        });
        state.open.insert(peer, activity.clone());
        state.mark_idle(peer, 0);
        drop(state);
        Some(LimitedConnection {
            stream,
            peer,
            activity,
            connections: self.clone(),
            evicted: Some(evicted),
            timer: Box::pin(tokio::time::sleep_until(now + self.shared.first_request)),
            closed: false,
        })
    }

    fn release(&self, peer: SocketAddr, activity: &Arc<Activity>) {
        let mut state = self.state();
        if state
            .open
            .get(&peer)
            .is_some_and(|open| Arc::ptr_eq(open, activity))
        {
            state.open.remove(&peer);
        }
        if let Some(count) = state.per_address.get_mut(&peer.ip()) {
            *count -= 1;
            if *count == 0 {
                state.per_address.remove(&peer.ip());
            }
        }
    }

    fn start_request(&self, peer: SocketAddr) -> Option<RequestGuard> {
        let activity = self.state().open.get(&peer)?.clone();
        activity.in_flight.fetch_add(1, Ordering::AcqRel);
        activity.used.store(true, Ordering::Release);
        activity.touch();
        Some(RequestGuard {
            connections: self.clone(),
            peer,
            activity,
        })
    }

    #[cfg(test)]
    pub(super) fn open_count(&self) -> usize {
        self.state().open.len()
    }
}

/// Accepted connections, closed on accept when over the caps.
pub struct LimitedIncoming<S> {
    incoming: S,
    connections: Connections,
}

impl<S> Stream for LimitedIncoming<S>
where
    S: Stream<Item = io::Result<TcpStream>> + Unpin,
{
    type Item = io::Result<LimitedConnection>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.incoming).poll_next(context) {
                Poll::Ready(Some(Ok(stream))) => {
                    // Over a cap: dropping the stream closes it; wait for the next one.
                    if let Some(connection) = self.connections.admit(stream) {
                        return Poll::Ready(Some(Ok(connection)));
                    }
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// An accepted fleet connection. It holds its slots until it closes, and fails
/// its next read once it is evicted or has been idle too long.
pub struct LimitedConnection {
    stream: TcpStream,
    peer: SocketAddr,
    activity: Arc<Activity>,
    connections: Connections,
    evicted: Option<oneshot::Receiver<()>>,
    timer: Pin<Box<Sleep>>,
    closed: bool,
}

impl LimitedConnection {
    /// Returns an error once the connection should close, registering wakeups
    /// for eviction and the idle deadline otherwise.
    fn check_open(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fleet connection closed",
            ));
        }
        if let Some(evicted) = self.evicted.as_mut()
            && Pin::new(evicted).poll(context).is_ready()
        {
            self.evicted = None;
            self.closed = true;
            tracing::debug!(peer = %self.peer, "evicted idle fleet connection to make room");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "idle fleet connection evicted",
            ));
        }
        {
            let mut waker = self
                .activity
                .waker
                .lock()
                .expect("connection waker lock poisoned");
            if !waker
                .as_ref()
                .is_some_and(|waker| waker.will_wake(context.waker()))
            {
                *waker = Some(context.waker().clone());
            }
        }
        loop {
            let now = Instant::now();
            let target = match self.activity.idle_deadline(&self.connections.shared) {
                Some(deadline) if deadline <= now => {
                    self.closed = true;
                    tracing::debug!(peer = %self.peer, "closing idle fleet connection");
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "idle fleet connection closed",
                    ));
                }
                Some(deadline) => deadline,
                // Busy: check again after an idle period.
                None => now + self.connections.shared.idle,
            };
            if self.timer.deadline() != target {
                self.timer.as_mut().reset(target);
            }
            if self.timer.as_mut().poll(context).is_pending() {
                return Ok(());
            }
        }
    }
}

impl Drop for LimitedConnection {
    fn drop(&mut self) {
        self.connections.release(self.peer, &self.activity);
    }
}

impl Connected for LimitedConnection {
    // The plain TCP info keeps `Request::remote_addr` and `peer_certs` working,
    // and lets the request layer find this connection by its peer address.
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}

impl AsyncRead for LimitedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.check_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimitedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

/// Marks the connection busy until the request's response has been fully sent
/// (for `Connect`, until the stream ends).
struct RequestGuard {
    connections: Connections,
    peer: SocketAddr,
    activity: Arc<Activity>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.activity.touch();
        if self.activity.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            let epoch = self.activity.idle_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            self.connections.state().mark_idle(self.peer, epoch);
            if let Some(waker) = self
                .activity
                .waker
                .lock()
                .expect("connection waker lock poisoned")
                .take()
            {
                waker.wake();
            }
        }
    }
}

/// Layer that ties each request to its connection's activity.
#[derive(Clone)]
pub struct TrackRequests {
    connections: Connections,
}

impl<S> Layer<S> for TrackRequests {
    type Service = Tracked<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Tracked {
            inner,
            connections: self.connections.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Tracked<S> {
    inner: S,
    connections: Connections,
}

fn remote_addr<B>(request: &http::Request<B>) -> Option<SocketAddr> {
    let extensions = request.extensions();
    extensions
        .get::<TcpConnectInfo>()
        .and_then(TcpConnectInfo::remote_addr)
        .or_else(|| {
            extensions
                .get::<TlsConnectInfo<TcpConnectInfo>>()
                .and_then(|info| info.get_ref().remote_addr())
        })
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<S> Service<http::Request<Body>> for Tracked<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let guard = remote_addr(&request).and_then(|peer| self.connections.start_request(peer));
        let response = self.inner.call(request);
        Box::pin(async move {
            let response = response.await?;
            Ok(match guard {
                Some(guard) => response.map(|body| {
                    Body::new(TrackedBody {
                        inner: body,
                        _guard: guard,
                    })
                }),
                None => response,
            })
        })
    }
}

/// A response body that keeps its connection busy until it is dropped.
struct TrackedBody {
    inner: Body,
    _guard: RequestGuard,
}

impl HttpBody for TrackedBody {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
