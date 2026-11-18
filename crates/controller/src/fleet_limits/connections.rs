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
//! its last request finishes. A connection with a device's request in progress,
//! including an open `Connect` stream, is busy: never idle. When a cap is
//! reached, the longest-idle connection is closed to make room instead of
//! refusing the new one: at the total cap any connection, and at the
//! per-address cap one from the same address. So idle connections cannot crowd
//! out devices that are using theirs. A device that goes away without closing
//! its `Connect` stream keeps that connection busy until HTTP/2 keepalive
//! closes it, so if it reconnects before then, at a cap with nothing idle, it
//! is refused until then.
//!
//! Only a request from a caller with a device certificate makes its connection
//! busy. Anyone can open a request and never send its body, and tonic waits for
//! the body before the handler runs, so an anonymous request would otherwise
//! hold its connection busy until the request timeout, and the next one after
//! it. An anonymous request still counts as activity, and its connection stays
//! subject to the caps, deadlines and request timeout, but it can be evicted.
//!
//! A device's request other than `Connect` is in progress only while its
//! handler runs, not while its response is sent, and a `Connect` stream only
//! while its session lasts. Otherwise a client could keep a connection busy
//! forever by granting no HTTP/2 flow-control window, so that the server can
//! never send the response body and hyper never drops it.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

use super::{FleetLimits, ThrottledWarning, address_key, has_device_certificate, is_connect};

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
    open: HashMap<SocketAddr, Connection>,
    /// Open connections by `address_key`.
    per_address: HashMap<IpAddr, Address>,
    /// Idle connections by idle key, so the first has been idle the longest.
    idle: BTreeMap<u64, SocketAddr>,
    /// The idle key of the next connection to go idle.
    next_idle: u64,
}

/// One address key's open connections.
#[derive(Default)]
struct Address {
    count: usize,
    /// This address's idle connections, keyed as in `State::idle`.
    idle: BTreeMap<u64, SocketAddr>,
}

/// An open connection's entry in the registry.
struct Connection {
    activity: Arc<Activity>,
    /// Its key in the idle maps while it is idle.
    idle: Option<u64>,
    /// Closes the connection. Taken when the connection is evicted; an
    /// evicted connection takes no more requests and is never idle again.
    evict: Option<oneshot::Sender<()>>,
}

/// One connection's request activity, read by the connection itself without
/// the registry lock.
pub(super) struct Activity {
    in_flight: AtomicUsize,
    /// Whether the connection has made any request.
    used: AtomicBool,
    /// When the connection was accepted, or last started or finished a request.
    last_active: Mutex<Instant>,
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
    /// Queues `peer`'s connection as the most recently idle one, unless it
    /// has closed (`activity` is no longer its entry) or been evicted.
    fn mark_idle(&mut self, peer: SocketAddr, activity: &Arc<Activity>) {
        let Some(connection) = self.open.get_mut(&peer) else {
            return;
        };
        if !Arc::ptr_eq(&connection.activity, activity) || connection.evict.is_none() {
            return;
        }
        let key = self.next_idle;
        self.next_idle += 1;
        connection.idle = Some(key);
        self.idle.insert(key, peer);
        if let Some(address) = self.per_address.get_mut(&address_key(peer.ip())) {
            address.idle.insert(key, peer);
        }
    }

    /// Takes `peer`'s connection out of the idle maps, if it is in them.
    fn unqueue(&mut self, peer: SocketAddr) {
        let Some(key) = self
            .open
            .get_mut(&peer)
            .and_then(|connection| connection.idle.take())
        else {
            return;
        };
        self.idle.remove(&key);
        if let Some(address) = self.per_address.get_mut(&address_key(peer.ip())) {
            address.idle.remove(&key);
        }
    }

    /// Closes the longest-idle connection. Returns false when every open
    /// connection has a device request in progress.
    fn evict_idle(&mut self) -> bool {
        let longest_idle = self.idle.first_key_value().map(|(_, peer)| *peer);
        longest_idle.is_some_and(|peer| self.evict(peer))
    }

    /// Closes `address`'s longest-idle connection. Returns false when each of
    /// its connections has a device request in progress.
    fn evict_idle_from(&mut self, address: IpAddr) -> bool {
        let longest_idle = self
            .per_address
            .get(&address)
            .and_then(|address| address.idle.first_key_value())
            .map(|(_, peer)| *peer);
        longest_idle.is_some_and(|peer| self.evict(peer))
    }

    fn evict(&mut self, peer: SocketAddr) -> bool {
        self.unqueue(peer);
        self.open
            .get_mut(&peer)
            .and_then(|connection| connection.evict.take())
            .is_some_and(|evict| {
                let _ = evict.send(());
                true
            })
    }
}

impl Connections {
    pub(super) fn new(limits: &FleetLimits) -> Self {
        Self {
            shared: Arc::new(Shared {
                max_connections: limits.configured.max_connections,
                connections_per_address: limits.configured.connections_per_address,
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

    /// A layer that records each request on its connection, and marks the
    /// connection busy while a device's request (or stream) is open.
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
        let address = address_key(peer.ip());
        let at_address_cap = state
            .per_address
            .get(&address)
            .is_some_and(|connections| connections.count >= self.shared.connections_per_address);
        if at_address_cap {
            // Room comes from the address's own connections only, so a client
            // at its cap never closes another client's connection. The one it
            // replaces frees a slot under the total cap too.
            if !state.evict_idle_from(address) {
                if let Some(closed) = PER_ADDRESS_CAP.event(Instant::now()) {
                    tracing::warn!(
                        latest_address = %address,
                        closed_since_last_warning = closed,
                        "closed new fleet connections at the per-address connection limit with no connection from the address idle"
                    );
                }
                return None;
            }
        } else if state.open.len() >= self.shared.max_connections && !state.evict_idle() {
            if let Some(closed) = TOTAL_CAP.event(Instant::now()) {
                tracing::warn!(
                    closed_since_last_warning = closed,
                    "closed new fleet connections at the connection limit with no connection idle"
                );
            }
            return None;
        }
        state.per_address.entry(address).or_default().count += 1;
        let (evict, evicted) = oneshot::channel();
        let activity = Arc::new(Activity {
            in_flight: AtomicUsize::new(0),
            used: AtomicBool::new(false),
            last_active: Mutex::new(Instant::now()),
            waker: Mutex::new(None),
        });
        // An entry left for this peer address by a connection that has not
        // been released yet is replaced; take it out of the idle maps first.
        state.unqueue(peer);
        state.open.insert(
            peer,
            Connection {
                activity: activity.clone(),
                idle: None,
                evict: Some(evict),
            },
        );
        state.mark_idle(peer, &activity);
        Some((activity, evicted))
    }

    fn release(&self, peer: SocketAddr, activity: &Arc<Activity>) {
        let mut state = self.state();
        if state
            .open
            .get(&peer)
            .is_some_and(|open| Arc::ptr_eq(&open.activity, activity))
        {
            state.unqueue(peer);
            state.open.remove(&peer);
        }
        let address = address_key(peer.ip());
        if let Some(connections) = state.per_address.get_mut(&address) {
            connections.count -= 1;
            if connections.count == 0 {
                state.per_address.remove(&address);
            }
        }
    }

    /// Records a request on `peer`'s connection; a `busy` one keeps the
    /// connection from going idle until the returned guard is dropped.
    pub(super) fn start_request(&self, peer: SocketAddr, busy: bool) -> Option<RequestGuard> {
        // Counted under the registry lock, which eviction holds, so a
        // connection is never evicted once its busy request has started.
        let activity = {
            let mut state = self.state();
            let connection = state.open.get(&peer)?;
            // An evicted connection fails its next read or write, which ends
            // this request too; it is not counted.
            connection.evict.as_ref()?;
            let activity = connection.activity.clone();
            if busy {
                activity.in_flight.fetch_add(1, Ordering::AcqRel);
                state.unqueue(peer);
            }
            activity
        };
        activity.used.store(true, Ordering::Release);
        activity.touch();
        Some(RequestGuard {
            connections: self.clone(),
            peer,
            activity,
            busy,
        })
    }

    #[cfg(test)]
    pub(super) fn open_count(&self) -> usize {
        self.state().open.len()
    }

    /// Entries in the idle map, and in the per-address idle maps.
    #[cfg(test)]
    pub(super) fn idle_entries(&self) -> (usize, usize) {
        let state = self.state();
        let by_address = state.per_address.values().map(|address| address.idle.len());
        (state.idle.len(), by_address.sum())
    }
}

/// Accepted connections, closed on accept when over the caps.
pub struct LimitedIncoming<S> {
    incoming: S,
    connections: Connections,
}

#[cfg(test)]
impl<S> LimitedIncoming<S> {
    pub(super) fn connections(&self) -> &Connections {
        &self.connections
    }
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
/// its next read, write or flush once it is evicted or has been idle too long.
/// Writes are checked too because a client that never reads fills the server's
/// send buffer, and HTTP/2 then stops reading until it can write.
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
            if let Some(closed) = EVICTIONS.event(Instant::now()) {
                tracing::warn!(
                    closed_since_last_warning = closed,
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
            // Busy: no deadline. The guard of the last request to finish wakes
            // this connection (through the waker registered above) when it
            // goes idle.
            let Some(deadline) = self.activity.idle_deadline(&self.connections.shared) else {
                return Ok(());
            };
            if deadline <= Instant::now() {
                self.closed = true;
                tracing::debug!(peer = %self.peer, "closing idle fleet connection");
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "idle fleet connection closed",
                ));
            }
            if self.timer.deadline() != deadline {
                self.timer.as_mut().reset(deadline);
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
        if let Err(error) = self.check_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.check_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check_open(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.stream).poll_flush(context)
    }

    // Unchecked, so a connection being closed can still shut down cleanly.
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

/// A request in progress. A device's request marks its connection busy while
/// its handler runs, and for `Connect` while its session lasts (see
/// `SessionBusy`); any request counts as activity when it starts and ends.
pub(super) struct RequestGuard {
    connections: Connections,
    peer: SocketAddr,
    activity: Arc<Activity>,
    busy: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.activity.touch();
        if !self.busy {
            return;
        }
        // Under the registry lock, like `start_request`, so eviction never
        // sees the connection idle before it is in the idle maps.
        let idle = {
            let mut state = self.connections.state();
            let idle = self.activity.in_flight.fetch_sub(1, Ordering::AcqRel) == 1;
            if idle {
                state.mark_idle(self.peer, &self.activity);
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
        // Only a device's request makes its connection busy; see the module docs.
        let busy = has_device_certificate(&request);
        let guard =
            remote_addr(&request).and_then(|peer| self.connections.start_request(peer, busy));
        // `Connect` is the device's session, so its connection stays busy past
        // the handler: see `SessionBusy`. Any other request is done once its
        // handler returns.
        let (guard, session) = match guard {
            Some(guard) if guard.busy && is_connect(request.uri().path()) => {
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
/// claims another from the request (`SessionBusy::release_for`) and holds it
/// for the session, as `DeviceStream` does. Without the handler's release, a
/// client that grants no HTTP/2 flow-control window could end the session and
/// still keep the connection busy, because hyper keeps a response body it
/// cannot send. If every handle is dropped without a release, the connection
/// goes idle too.
#[derive(Clone)]
pub(super) struct SessionBusy {
    guard: Arc<Mutex<Option<RequestGuard>>>,
}

impl SessionBusy {
    fn new(guard: RequestGuard) -> Self {
        Self {
            guard: Arc::new(Mutex::new(Some(guard))),
        }
    }

    /// A release for `request`'s session, when the request is a tracked
    /// `Connect`.
    pub(super) fn release_for<T>(request: &tonic::Request<T>) -> Option<ReleaseOnDrop> {
        request
            .extensions()
            .get::<Self>()
            .map(Self::release_on_drop)
    }

    /// Marks the connection idle when the returned value is dropped, unless
    /// another release already has.
    fn release_on_drop(&self) -> ReleaseOnDrop {
        ReleaseOnDrop {
            guard: self.guard.clone(),
        }
    }
}

/// Releases a `SessionBusy` when dropped.
pub(super) struct ReleaseOnDrop {
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
