//! [`NetworkLink`]: a [`NetworkEndpoint`] over a `std::net::UdpSocket`, run by a
//! socket task on an [`Executor`].

use futures_core::Stream;
use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tutti_platform::{Executor, Instant, Task};

use crate::{Event, MAX_DATAGRAM_BYTES, MAX_QUEUED_EVENTS, NetworkEndpoint};

/// The longest the socket task blocks in one read: when the endpoint has no
/// deadline of its own, and the bound on how late a stop is seen.
const IDLE_WAIT_MAX: Duration = Duration::from_millis(100);

/// The shortest the socket task blocks in one read: a socket read timeout of
/// zero means forever.
const WAIT_MIN: Duration = Duration::from_millis(1);

/// The name of the socket task's thread.
const THREAD_NAME: &str = "tutti-midi-network";

/// Why a [`NetworkLink`]'s socket task ended: the socket's error, which the
/// task could not carry on past.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the network link's socket failed ({kind:?}): {message}")]
pub struct LinkEnded {
    kind: io::ErrorKind,
    message: String,
}

impl LinkEnded {
    fn new(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    /// The kind of the socket's error.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }
}

struct State<E> {
    endpoint: E,
    /// Events taken from the endpoint and not yet read, at most
    /// [`MAX_QUEUED_EVENTS`].
    events: VecDeque<Event>,
    /// Events that found `events` full.
    lost: u64,
    /// Datagrams dropped because the OS refused them for their destination.
    sends_failed: u64,
    /// The futures waiting for an event, each woken by the next one or the
    /// end, under the id its [`NetworkEvents`] holds.
    wakers: Vec<(u64, Waker)>,
    next_id: u64,
    ended: Option<LinkEnded>,
    stop: bool,
    /// When the socket task next looks at the endpoint's clock at the latest.
    armed: Instant,
}

impl<E: NetworkEndpoint> State<E> {
    /// Sends what the endpoint queued. A datagram the OS refuses for its own
    /// destination is dropped and counted ([`is_per_destination`]); the rest
    /// still go.
    ///
    /// # Errors
    ///
    /// The first error that is the socket's, not a destination's; what the
    /// endpoint still holds stays queued.
    fn flush(&mut self, socket: &UdpSocket) -> io::Result<()> {
        while let Some(transmit) = self.endpoint.poll_transmit() {
            match socket.send_to(&transmit.payload, transmit.to) {
                Ok(_) => {}
                Err(e) if is_per_destination(e.kind()) => self.sends_failed += 1,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Ends the link for good after `error` came from the socket: the stream
    /// ends and the task stops at its next turn. The wakers are woken by the
    /// caller once it holds no lock.
    #[must_use = "wake them after releasing the lock"]
    fn fail(&mut self, error: &io::Error) -> Vec<Waker> {
        self.ended.get_or_insert_with(|| LinkEnded::new(error));
        self.stop = true;
        self.take_wakers()
    }

    /// Moves the endpoint's events into the queue; the wakers returned are
    /// woken by the caller once it holds no lock.
    #[must_use = "wake them after releasing the lock"]
    fn collect(&mut self) -> Vec<Waker> {
        let mut any = false;
        while let Some(event) = self.endpoint.poll_event() {
            any = true;
            if self.events.len() < MAX_QUEUED_EVENTS {
                self.events.push_back(event);
            } else {
                self.lost += 1;
            }
        }
        if any { self.take_wakers() } else { Vec::new() }
    }
}

impl<E> State<E> {
    fn take_wakers(&mut self) -> Vec<Waker> {
        std::mem::take(&mut self.wakers)
            .into_iter()
            .map(|(_, waker)| waker)
            .collect()
    }

    /// Removes the waker registered under `id`; the caller drops it once it
    /// holds no lock, since a waker's drop can run its executor's code.
    #[must_use = "drop it after releasing the lock"]
    fn forget(&mut self, id: u64) -> Option<Waker> {
        let at = self.wakers.iter().position(|(held, _)| *held == id)?;
        Some(self.wakers.swap_remove(at).1)
    }
}

/// Whether a send error is its destination's rather than the socket's: the
/// peer's host or network cannot be reached, its port refuses, or its address
/// cannot be used. UDP drops such a datagram and the link carries on; any
/// other kind (a permission, a closed descriptor, a buffer) is the socket's
/// and ends the link.
const fn is_per_destination(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::AddrNotAvailable
    )
}

struct Shared<E> {
    state: Mutex<State<E>>,
}

impl<E> Shared<E> {
    /// The state's guard whether or not a panicking holder poisoned it: every
    /// holder leaves the state consistent where it can panic (the endpoint's
    /// own calls).
    fn state(&self) -> MutexGuard<'_, State<E>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A [`NetworkEndpoint`] running over a UDP socket: a socket task reads
/// datagrams, keeps the endpoint's clock and sends what the endpoint queues,
/// and the application reads the endpoint's [`Event`]s from [`events`](Self::events).
///
/// Fed by the link's socket task, which runs on the [`Executor`]: nothing to
/// drive. The task has a thread of its own that the executor starts, and it
/// holds the endpoint only while it feeds it. The application hands the
/// endpoint UMP, or ends a Session, through [`with`](Self::with), which sends
/// what the call queued before it returns.
///
/// Drop a link before the executor it runs on: the executor's own stop waits
/// out its bound for a thread that is still reading.
///
/// The link's queue holds at most [`MAX_QUEUED_EVENTS`] events: one that finds
/// it full is counted in [`events_lost`](Self::events_lost) and dropped, never
/// waited on.
///
/// Dropping the link stops the task: the thread leaves its read within
/// 100 ms of the drop (at once when the poke it is sent lands)
/// and closes its socket handle. Dropping the link first sends a Bye
/// ([`NetworkEndpoint::close_all`]) to every Session it holds.
pub struct NetworkLink<E: NetworkEndpoint + Send + 'static> {
    shared: Arc<Shared<E>>,
    /// The caller's handle: it sends what [`with`](Self::with) queued and
    /// pokes the task. The task reads through its own clone.
    socket: UdpSocket,
    /// Held for its drop, which cancels the task.
    #[expect(dead_code, reason = "held for its drop, which cancels the task")]
    task: Task<()>,
}

impl<E: NetworkEndpoint + Send + 'static> core::fmt::Debug for NetworkLink<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetworkLink")
            .field("local_addr", &self.socket.local_addr())
            .finish_non_exhaustive()
    }
}

impl<E: NetworkEndpoint + Send + 'static> NetworkLink<E> {
    /// A link over `socket`, which is the one source address the peer sees: a
    /// Host's listening port, a Client's own. The socket task starts on a
    /// thread of its own from `executor`.
    ///
    /// # Errors
    ///
    /// The error from duplicating the socket, or the OS's refusal to start the
    /// thread.
    pub fn spawn(executor: &Executor, socket: UdpSocket, endpoint: E) -> io::Result<Self> {
        let task_socket = socket.try_clone()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                endpoint,
                events: VecDeque::new(),
                lost: 0,
                sends_failed: 0,
                wakers: Vec::new(),
                next_id: 0,
                ended: None,
                stop: false,
                armed: Instant::now(),
            }),
        });
        // The guard is in the future from the start: a future dropped before
        // its first poll still ends the stream.
        let ending = Ending {
            shared: shared.clone(),
            cause: None,
        };
        let task = executor.spawn_dedicated(THREAD_NAME, None, run(ending, task_socket))?;
        Ok(Self {
            shared,
            socket,
            task,
        })
    }

    /// A link over a new socket bound to `addr`; see [`spawn`](Self::spawn).
    ///
    /// # Errors
    ///
    /// The error from binding, or from [`spawn`](Self::spawn).
    pub fn bind(executor: &Executor, addr: impl ToSocketAddrs, endpoint: E) -> io::Result<Self> {
        Self::spawn(executor, UdpSocket::bind(addr)?, endpoint)
    }

    /// The address the socket is bound to.
    ///
    /// # Errors
    ///
    /// The error from the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Runs `f` on the endpoint at the current time, on the caller's thread,
    /// then sends what it queued. The socket task is told of an earlier
    /// deadline the call set.
    ///
    /// `f` runs under the link's lock: calling one of the link's own methods
    /// from inside it deadlocks.
    ///
    /// A datagram the OS refuses for its destination is dropped and counted in
    /// [`sends_failed`](Self::sends_failed), as UDP does. An error that is the
    /// socket's ends the link: the stream ends and
    /// [`failure`](Self::failure) says why.
    pub fn with<R>(&self, f: impl FnOnce(&mut E, Instant) -> R) -> R {
        let mut state = self.shared.state();
        let now = Instant::now();
        let result = f(&mut state.endpoint, now);
        let mut wakers = match state.flush(&self.socket) {
            Ok(()) => Vec::new(),
            Err(error) => state.fail(&error),
        };
        wakers.extend(state.collect());
        let earlier = state.endpoint.deadline().is_some_and(|at| at < state.armed);
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
        if earlier {
            self.poke();
        }
        result
    }

    /// The link's events as a [`Stream`], the oldest first.
    ///
    /// It ends (`None`) once the socket task has ended and the queue is
    /// empty; [`failure`](Self::failure) says why.
    pub fn events(&self) -> NetworkEvents<'_, E> {
        NetworkEvents {
            link: self,
            id: None,
        }
    }

    /// Why the socket task ended, or `None` while it runs.
    #[must_use]
    pub fn failure(&self) -> Option<LinkEnded> {
        self.shared.state().ended.clone()
    }

    /// The events queued now, taken as iterated; never blocks.
    pub fn pending(&self) -> PendingEvents<'_, E> {
        PendingEvents { link: self }
    }

    /// How many events were dropped before the application read them: those
    /// that found the link's queue full, and those the endpoint dropped
    /// ([`NetworkEndpoint::events_lost`]).
    #[must_use]
    pub fn events_lost(&self) -> u64 {
        let state = self.shared.state();
        state.lost + state.endpoint.events_lost()
    }

    /// How many datagrams were dropped because the OS refused them for their
    /// destination (the peer's host or network is unreachable, its port
    /// refuses, its address is unusable).
    #[must_use]
    pub fn sends_failed(&self) -> u64 {
        self.shared.state().sends_failed
    }

    /// Wakes the socket task from its read with a zero-length datagram from
    /// the socket to itself. A poke that cannot be sent is not an error: the
    /// task looks at the clock within 100 ms anyway.
    fn poke(&self) {
        if let Ok(local) = self.socket.local_addr() {
            let _ = self.socket.send_to(&[], poke_addr(local));
        }
    }
}

impl<E: NetworkEndpoint + Send + 'static> Drop for NetworkLink<E> {
    fn drop(&mut self) {
        let wakers = {
            let mut state = self.shared.state();
            state.endpoint.close_all(Instant::now());
            // Nobody is left to tell of a Bye that cannot be sent.
            let _ = state.flush(&self.socket);
            state.stop = true;
            state.take_wakers()
        };
        wakers.into_iter().for_each(Waker::wake);
        self.poke();
    }
}

/// Where a socket bound to `local` receives a datagram from itself: an
/// unspecified bound address is reached through loopback.
fn poke_addr(mut local: SocketAddr) -> SocketAddr {
    match local.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => local.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        IpAddr::V6(ip) if ip.is_unspecified() => local.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        _ => {}
    }
    local
}

/// Ends the link's stream when the socket task is over, however it ends: the
/// socket's error, a stop, a panic in the endpoint, or the executor dropping
/// the task, even before its first poll.
struct Ending<E> {
    shared: Arc<Shared<E>>,
    cause: Option<LinkEnded>,
}

impl<E> Drop for Ending<E> {
    fn drop(&mut self) {
        let cause = self.cause.take().unwrap_or_else(|| LinkEnded {
            kind: io::ErrorKind::Other,
            message: "the socket task stopped".to_owned(),
        });
        let wakers = {
            let mut state = self.shared.state();
            state.ended.get_or_insert(cause);
            state.take_wakers()
        };
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// The socket task: reads datagrams for the endpoint, keeps its clock and
/// sends what it queues, until the link stops it or the socket fails.
async fn run<E: NetworkEndpoint + Send + 'static>(mut ending: Ending<E>, socket: UdpSocket) {
    if let Err(error) = serve(&ending.shared, &socket) {
        ending.cause = Some(LinkEnded::new(&error));
    }
}

fn serve<E: NetworkEndpoint>(shared: &Shared<E>, socket: &UdpSocket) -> io::Result<()> {
    let local = socket.local_addr()?;
    // Room for the largest datagram: a longer one is cut and its trailing
    // commands refused, never read past the buffer.
    let mut buffer = vec![0; 2 * MAX_DATAGRAM_BYTES];
    loop {
        let wait = {
            let mut state = shared.state();
            if state.stop {
                return Ok(());
            }
            let now = Instant::now();
            let wait = state
                .endpoint
                .deadline()
                .map_or(IDLE_WAIT_MAX, |at| at.saturating_duration_since(now))
                .min(IDLE_WAIT_MAX)
                .max(WAIT_MIN);
            state.armed = now + wait;
            wait
        };
        socket.set_read_timeout(Some(wait))?;
        let received = match socket.recv_from(&mut buffer) {
            Ok(received) => Some(received),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                None
            }
            // A previous send reached a port with nobody behind it.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => None,
            Err(e) => return Err(e),
        };
        let mut state = shared.state();
        // The link was dropped while the task read: its Bye is sent, and
        // nothing is handled after it.
        if state.stop {
            return Ok(());
        }
        // A zero-length datagram from the socket's own port is the link's
        // poke: it wakes the read and is not the endpoint's to see.
        if let Some((len, from)) = received
            && !(len == 0 && from.port() == local.port())
        {
            state
                .endpoint
                .on_message(from, &buffer[..len], Instant::now());
        }
        state.endpoint.on_timeout(Instant::now());
        let flushed = state.flush(socket);
        let wakers = state.collect();
        drop(state);
        wakers.into_iter().for_each(Waker::wake);
        flushed?;
    }
}

/// The events of a [`NetworkLink`] as a [`Stream`]: [`NetworkLink::events`].
///
/// Fed by the link's socket task, which runs on the executor: nothing to
/// drive. The task wakes it when an event is queued or the task ends. It
/// yields the oldest queued event; once the task has ended and the queue is
/// empty it yields `None`, and keeps yielding it. [`NetworkLink::failure`]
/// says why the task ended.
///
/// Several streams of one link share its queue: each event goes to one of them.
#[must_use = "a stream does nothing until it is polled"]
pub struct NetworkEvents<'a, E: NetworkEndpoint + Send + 'static> {
    link: &'a NetworkLink<E>,
    /// The id of the waker this registered, until it is woken, resolved or
    /// dropped.
    id: Option<u64>,
}

impl<E: NetworkEndpoint + Send + 'static> core::fmt::Debug for NetworkEvents<'_, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetworkEvents").finish_non_exhaustive()
    }
}

impl<E: NetworkEndpoint + Send + 'static> Stream for NetworkEvents<'_, E> {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let this = self.get_mut();
        let mut state = this.link.shared.state();
        let ready = match state.events.pop_front() {
            Some(event) => Some(Some(event)),
            None => state.ended.is_some().then_some(None),
        };
        if let Some(output) = ready {
            let old = this.id.take().and_then(|id| state.forget(id));
            drop(state);
            drop(old);
            return Poll::Ready(output);
        }
        // A re-poll replaces this future's own waker; a waker already taken by a
        // wake is registered again under the same id.
        let id = *this.id.get_or_insert_with(|| {
            state.next_id += 1;
            state.next_id
        });
        // The replaced waker is dropped once the lock is released.
        let old = match state.wakers.iter_mut().find(|(held, _)| *held == id) {
            Some((_, waker)) if waker.will_wake(cx.waker()) => None,
            Some((_, waker)) => Some(std::mem::replace(waker, cx.waker().clone())),
            None => {
                state.wakers.push((id, cx.waker().clone()));
                None
            }
        };
        drop(state);
        drop(old);
        Poll::Pending
    }
}

impl<E: NetworkEndpoint + Send + 'static> Drop for NetworkEvents<'_, E> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let old = self.link.shared.state().forget(id);
            drop(old);
        }
    }
}

/// The events of a [`NetworkLink`] queued now, taken as they are read:
/// [`NetworkLink::pending`].
pub struct PendingEvents<'a, E: NetworkEndpoint + Send + 'static> {
    link: &'a NetworkLink<E>,
}

impl<E: NetworkEndpoint + Send + 'static> core::fmt::Debug for PendingEvents<'_, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PendingEvents").finish_non_exhaustive()
    }
}

impl<E: NetworkEndpoint + Send + 'static> Iterator for PendingEvents<'_, E> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        self.link.shared.state().events.pop_front()
    }
}
