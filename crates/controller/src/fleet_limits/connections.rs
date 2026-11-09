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
//!
//! A request other than `Connect` is in progress only while its handler runs,
//! not while its response is sent, and a `Connect` stream only while its
//! session lasts. Otherwise a client could keep a connection busy forever by
//! granting no HTTP/2 flow-control window, so that the server can never send
//! the response body and hyper never drops it.

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

use super::{FleetLimits, ThrottledWarning, address_key, is_connect};

static TOTAL_CAP: ThrottledWarning = ThrottledWarning::new();
static PER_ADDRESS_CAP: ThrottledWarning = ThrottledWarning::new();
static EVICTIONS: ThrottledWarning = ThrottledWarning::new();

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
    /// Open connections by `address_key`.
    per_address: HashMap<IpAddr, usize>,
    /// Connections in the order they became idle. An entry is stale once its
    /// connection makes a request or closes; `Activity::idle_epoch` tells.
    idle: VecDeque<(SocketAddr, u64)>,
}

/// One connection's request activity.
pub(super) struct Activity {
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
        let (activity, evicted) = self.register(peer)?;
        Some(LimitedConnection {
            stream,
            peer,
            activity,
            connections: self.clone(),
            evicted: Some(evicted),
            timer: Box::pin(tokio::time::sleep_until(
                Instant::now() + self.shared.first_request,
            )),
            closed: false,
        })
    }

    /// Registers a connection from `peer` as idle, or returns `None` when the
    /// caps refuse it. The receiver resolves if the connection is evicted.
    pub(super) fn register(
        &self,
        peer: SocketAddr,
    ) -> Option<(Arc<Activity>, oneshot::Receiver<()>)> {
        let mut state = self.state();
        // The per-address cap comes first, so an address over it cannot close
        // another client's idle connection only to be refused itself.
        let address = address_key(peer.ip());
        if state
            .per_address
            .get(&address)
            .is_some_and(|count| *count >= self.shared.connections_per_address)
        {
            if let Some(events) = PER_ADDRESS_CAP.event(Instant::now()) {
                tracing::warn!(
                    %address,
                    events,
                    "closed new fleet connections over the per-address connection limit"
                );
            }
            return None;
        }
        if state.open.len() >= self.shared.max_connections && !state.evict_idle() {
            if let Some(events) = TOTAL_CAP.event(Instant::now()) {
                tracing::warn!(
                    events,
                    "closed new fleet connections at the connection limit with no connection idle"
                );
            }
            return None;
        }
        *state.per_address.entry(address).or_default() += 1;
        let (evict, evicted) = oneshot::channel();
        let activity = Arc::new(Activity {
            in_flight: AtomicUsize::new(0),
            idle_epoch: AtomicU64::new(0),
            used: AtomicBool::new(false),
            last_active: Mutex::new(Instant::now()),
            evict: Mutex::new(Some(evict)),
            waker: Mutex::new(None),
        });
        state.open.insert(peer, activity.clone());
        state.mark_idle(peer, 0);
        Some((activity, evicted))
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
        let address = address_key(peer.ip());
        if let Some(count) = state.per_address.get_mut(&address) {
            *count -= 1;
            if *count == 0 {
                state.per_address.remove(&address);
            }
        }
    }

    fn start_request(&self, peer: SocketAddr) -> Option<RequestGuard> {
        // Counted under the registry lock, which `evict_idle` holds, so a
        // connection is never evicted once its request has started.
        let activity = {
            let state = self.state();
            let activity = state.open.get(&peer)?.clone();
            activity.in_flight.fetch_add(1, Ordering::AcqRel);
            activity
        };
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
            if let Some(events) = EVICTIONS.event(Instant::now()) {
                tracing::warn!(
                    events,
                    "closed idle fleet connections to make room at the connection limit"
                );
            }
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

/// Marks the connection busy while a request's handler runs, and for `Connect`
/// while its session lasts (see `SessionBusy`).
struct RequestGuard {
    connections: Connections,
    peer: SocketAddr,
    activity: Arc<Activity>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.activity.touch();
        // Under the registry lock, like `start_request`, so `evict_idle` never
        // sees the connection idle before it is queued with its new epoch.
        let idle = {
            let mut state = self.connections.state();
            let idle = self.activity.in_flight.fetch_sub(1, Ordering::AcqRel) == 1;
            if idle {
                let epoch = self.activity.idle_epoch.fetch_add(1, Ordering::AcqRel) + 1;
                state.mark_idle(self.peer, epoch);
            }
            idle
        };
        if idle
            && let Some(waker) = self
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

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
        let guard = remote_addr(&request).and_then(|peer| self.connections.start_request(peer));
        // `Connect` is the device's session, so its connection stays busy past
        // the handler: see `SessionBusy`. Any other request is done once its
        // handler returns.
        let (guard, session) = match guard {
            Some(guard) if is_connect(request.uri().path()) => {
                let session = SessionBusy::new(guard);
                request.extensions_mut().insert(session.clone());
                (None, Some(session))
            }
            guard => (guard, None),
        };
        let response = self.inner.call(request);
        Box::pin(async move {
            let response = response.await;
            drop(guard);
            let response = response?;
            Ok(match session {
                Some(session) => response.map(|body| {
                    Body::new(TrackedBody {
                        inner: body,
                        _release: session.release_on_drop(),
                    })
                }),
                None => response,
            })
        })
    }
}

/// Keeps a `Connect` stream's connection busy. The connection goes idle when
/// the first release is dropped: the response body holds one, and the handler
/// can claim another from the request extensions (`SessionBusy::of`) and hold
/// it in the session's task. Without the handler's release, a client that
/// grants no HTTP/2 flow-control window could end the session and still keep
/// the connection busy, because hyper keeps a response body it cannot send.
/// If every handle is dropped without a release, the connection goes idle too.
#[derive(Clone)]
pub struct SessionBusy {
    guard: Arc<Mutex<Option<RequestGuard>>>,
}

impl SessionBusy {
    fn new(guard: RequestGuard) -> Self {
        Self {
            guard: Arc::new(Mutex::new(Some(guard))),
        }
    }

    /// The handle for `request`'s connection, when it is a tracked `Connect`.
    pub fn of<T>(request: &tonic::Request<T>) -> Option<Self> {
        request.extensions().get::<Self>().cloned()
    }

    /// Marks the connection idle when the returned value is dropped, unless
    /// another release already has.
    pub fn release_on_drop(&self) -> ReleaseOnDrop {
        ReleaseOnDrop {
            guard: self.guard.clone(),
        }
    }
}

/// Releases a `SessionBusy` when dropped.
pub struct ReleaseOnDrop {
    guard: Arc<Mutex<Option<RequestGuard>>>,
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        // Taken out first, so the guard's own drop (which takes the registry
        // lock) runs without this lock held.
        let guard = self
            .guard
            .lock()
            .expect("connection session lock poisoned")
            .take();
        drop(guard);
    }
}

/// A `Connect` response body; dropping it releases its connection's session.
struct TrackedBody {
    inner: Body,
    _release: ReleaseOnDrop,
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
