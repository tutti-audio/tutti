//! The control stream's Windows side: a named pipe, which has no receive
//! timeout and no `poll`, so every wait here asks the pipe how many bytes it
//! holds and naps between asks. A pipe has no inheritable end to hand a child, so
//! the host names an unguessable pipe in the server's command line, and the
//! server listens on it.

use super::control::{ControlStream, back_off};
use crate::ProcessGuard;
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericNamespaced, Stream, ToNsName as _};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_ipc_ffi::{Connection, LaneWire, Reaped, Spawn};
use tutti_plugin_types::{IpcError, IpcResult};

/// How long to wait between connect attempts.
///
/// Short enough that a fast spawn is not rounded up to something a user
/// notices, long enough that a slow one does not spin the CPU. At this interval
/// the common case returns in single-digit milliseconds.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(2);

/// The server a host has spawned, as the host reaches it: the name of the pipe
/// the server listens on.
pub(crate) struct Rendezvous {
    name: String,
}

/// Spawns `command` with the name of a fresh pipe as its only argument, for the
/// server to listen on.
///
/// # Errors
///
/// The OS's random source cannot be read, or the spawn is refused.
pub(crate) fn spawn(mut command: Command) -> IpcResult<(Reaped, Rendezvous)> {
    let name = tutti_plugin_ipc_ffi::unique_name("tutti-bridge")
        .map_err(|e| IpcError::Io(std::io::Error::other(e)))?;
    command.arg(&name);
    let server =
        tutti_plugin_ipc_ffi::spawn(command, Spawn::Leader).map_err(crate::error::spawn_failed)?;
    Ok((server, Rendezvous { name }))
}

impl Rendezvous {
    /// Connects as soon as the server is listening, rather than after a fixed wait.
    ///
    /// A connect fails immediately while nothing listens, so retrying is how the
    /// host learns the pipe is up. The name is unique per launch, so a successful
    /// connect can only be *this* server.
    ///
    /// The server's watch is asked between attempts, and the pause between them
    /// is a wait on it ([`ServerWatch::wait_dead`](crate::ServerWatch::wait_dead)):
    /// a server that died on startup (a missing dynamic library, an immediate
    /// panic) never listens, and the wait ends at its death and not at the end of a
    /// retry period, so the host reports a failure whose cause is known at once
    /// and not after the full `patience`. No event says "the pipe is listening", so
    /// the attempts themselves are the poll.
    ///
    /// # Errors
    ///
    /// The server ended before it listened ([`IpcError::ServerExited`]), or the
    /// last connect failure once `patience` is spent.
    pub(crate) fn connect(
        self,
        server: &ProcessGuard,
        patience: Duration,
    ) -> IpcResult<ControlStream> {
        let deadline = Instant::now() + patience;
        loop {
            match self.connect_once() {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    if let Some(cause) = server.watch().cause() {
                        return Err(IpcError::ServerExited { cause });
                    }
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    server.watch().wait_dead(CONNECT_RETRY_INTERVAL);
                }
            }
        }
    }

    /// One attempt to connect, with [`SEND_TIMEOUT`](super::control::SEND_TIMEOUT)
    /// on writes where the pipe has one (it has none: arming is best-effort, and
    /// a hung server's writes end when the server is ended and the pipe closes).
    fn connect_once(&self) -> IpcResult<ControlStream> {
        let name = self.name.clone().to_ns_name::<GenericNamespaced>()?;
        let stream = Stream::connect(name)?;
        let _ = stream.set_send_timeout(Some(super::control::SEND_TIMEOUT));
        Ok(stream)
    }
}

/// A lane the host is opening: the name of the pipe the server listens on.
pub(crate) struct PendingLane {
    wire: LaneWire,
}

impl PendingLane {
    /// A fresh lane, under a fresh name.
    ///
    /// # Errors
    ///
    /// The operating system's random source cannot be read.
    pub(crate) fn new() -> IpcResult<Self> {
        Ok(Self {
            wire: LaneWire::fresh()?,
        })
    }

    /// What the `OpenLane` frame carries of the lane: the pipe's name.
    pub(crate) fn wire(&self) -> LaneWire {
        self.wire.clone()
    }

    /// Nothing is sent beside the frame: the name is in it.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn send_end(&self, _: Connection<'_>) -> std::io::Result<()> {
        Ok(())
    }

    /// Connects to the pipe the server listens on, which it does before it says
    /// the lane is open.
    ///
    /// # Errors
    ///
    /// The pipe cannot be connected to.
    pub(crate) fn stream(self) -> IpcResult<ControlStream> {
        let name = self
            .wire
            .name()
            .to_owned()
            .to_ns_name::<GenericNamespaced>()?;
        let stream = Stream::connect(name)?;
        let _ = stream.set_send_timeout(Some(super::control::SEND_TIMEOUT));
        Ok(stream)
    }
}

/// The two ends of a broker connection, host's first, for the mock servers of
/// the tests: a fresh pipe, connected.
///
/// # Panics
///
/// The pipe cannot be created or connected.
#[cfg(any(test, feature = "testing"))]
#[cfg_attr(
    not(feature = "testing"),
    expect(
        unreachable_pub,
        reason = "exported with feature `testing`; the crate's tests use it too"
    )
)]
pub fn broker_pair() -> (ControlStream, ControlStream) {
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::traits::Listener as _;
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "tutti_broker_pair-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    );
    let name = || {
        name.clone()
            .to_ns_name::<GenericNamespaced>()
            .expect("a pipe name")
    };
    let listener = ListenerOptions::new()
        .name(name())
        .create_sync()
        .expect("a pipe to listen on");
    let host = Stream::connect(name()).expect("the host's end connects");
    let server = listener.accept().expect("the server's end accepts");
    (host, server)
}

/// Bytes the named pipe can hand over right now, without blocking or consuming.
///
/// This is the whole Windows deadline mechanism. `interprocess` refuses
/// `set_recv_timeout` on a named pipe unconditionally (`no_timeouts()` in its
/// source), so a blocked `read` there returns only when the peer speaks or hangs
/// up, and the read loop can consult its deadline only *between* syscalls.
/// Asking first turns the blocking read into one that is known not to block.
///
/// **`Ok(0)` means "nothing yet", never end of stream.** That distinction is the
/// entire reason this is `PeekNamedPipe` and not `set_nonblocking`, which
/// reports a quiet pipe and a dead one identically; see [`Wakeup::arm`].
///
/// A closed peer makes this *fail* (`ERROR_BROKEN_PIPE`), and callers must treat
/// any failure as "go read and find out" rather than as a verdict: the real
/// `read` is the authority on EOF, and this must never be able to manufacture
/// one. That is what keeps a peek that goes wrong from becoming a false crash
/// report, which is the exact bug the `PIPE_NOWAIT` attempt shipped.
fn bytes_available(stream: &ControlStream) -> std::io::Result<u32> {
    // `local_socket::Stream` has exactly one variant on Windows, so this
    // destructure is irrefutable. Written as a `let` rather than a `match` for
    // that reason: a wildcard arm would be unreachable code.
    let Stream::NamedPipe(pipe) = stream;
    tutti_plugin_ipc_ffi::pipe_bytes_available(pipe.as_handle())
}

/// Whether `stream` has something to read now, asked without blocking or
/// consuming. **A failed ask is "yes"**: the read is the authority on a closed
/// or broken stream, and this must never be able to hide one.
fn has_pending(stream: &ControlStream) -> bool {
    bytes_available(stream).map_or(true, |n| n > 0)
}

/// A second handle on a control stream that only *waits* for it to have
/// something to read, from a thread of its own: it never reads, so the thread
/// that does keeps every byte and its frame cursor.
///
/// The duplicated handle is peeked with the [`back_off`] naps, because a named
/// pipe has no `poll`. Holding it keeps the pipe open: drop it promptly when the
/// stream's owner is done.
pub(crate) struct Watch {
    handle: OwnedHandle,
}

impl Watch {
    /// A watch on `stream`.
    ///
    /// # Errors
    ///
    /// The OS refuses to duplicate the handle.
    pub(crate) fn of(stream: &ControlStream) -> std::io::Result<Self> {
        let Stream::NamedPipe(pipe) = stream;
        Ok(Self {
            handle: pipe.as_handle().try_clone_to_owned()?,
        })
    }

    /// Waits up to `timeout` for the stream to have something to read.
    /// `true` when it has, or when asking fails (a read is the authority on a
    /// closed or broken stream); `false` on a timeout.
    pub(crate) fn wait_readable(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut empty = 0u32;
        loop {
            match tutti_plugin_ipc_ffi::pipe_bytes_available(self.handle.as_handle()) {
                Ok(0) => {}
                Ok(_) | Err(_) => return true,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            back_off(&mut empty, left);
        }
    }
}

/// The unparking side of a control thread's wait ([`control_unparker`]): a flag the
/// wait looks at between its looks at the pipe. Any thread rings it, the audio
/// thread included: one store.
#[derive(Clone)]
pub(crate) struct ControlUnparker {
    rung: Arc<AtomicBool>,
}

impl ControlUnparker {
    /// Ends the control thread's wait now, or its next one.
    pub(crate) fn unpark(&self) {
        self.rung.store(true, Ordering::Release);
    }
}

/// The waiting side of a control thread's wait ([`control_unparker`]): a named pipe
/// has no `poll`, so it asks the pipe and the flag in turn and naps between
/// asks ([`back_off`]).
pub(crate) struct ControlWait {
    rung: Arc<AtomicBool>,
}

/// What ended a [`ControlWait::wait`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Woken {
    /// The control stream has something to read (or broke: the read says).
    pub stream: bool,
    /// The bell rang.
    pub rung: bool,
}

/// A control thread's bell and its wait.
///
/// # Errors
///
/// Never on this platform.
pub(crate) fn control_unparker() -> std::io::Result<(ControlUnparker, ControlWait)> {
    let rung = Arc::new(AtomicBool::new(false));
    Ok((
        ControlUnparker {
            rung: Arc::clone(&rung),
        },
        ControlWait { rung },
    ))
}

impl ControlWait {
    /// Waits up to `timeout` (`None`: for ever) for `stream` to have something
    /// to read or for the bell to ring. A ring is cleared.
    ///
    /// # Errors
    ///
    /// Never on this platform.
    pub(crate) fn wait(
        &self,
        stream: &ControlStream,
        timeout: Option<Duration>,
    ) -> std::io::Result<Woken> {
        /// The longest nap between looks: a ring or a frame is heard within it.
        const NAP: Duration = Duration::from_millis(1);
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut empty = 0u32;
        loop {
            let woken = Woken {
                rung: self.rung.swap(false, Ordering::AcqRel),
                stream: has_pending(stream),
            };
            if woken.rung || woken.stream {
                return Ok(woken);
            }
            let left = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(woken);
                    }
                    left
                }
                None => NAP,
            };
            back_off(&mut empty, left.min(NAP));
        }
    }
}

/// How a blocked read is made to return so the deadline can be re-checked:
/// `PeekNamedPipe`. Nothing wakes a blocked read on a Windows named pipe, so
/// this end never blocks in the first place: it asks how many bytes are
/// buffered and only reads when the answer is "some". The waiting happens in
/// `back_off`.
pub(super) struct Wakeup;

impl Wakeup {
    /// The peek, with no receive timeout tried first: `interprocess` refuses
    /// one on a named pipe unconditionally, and it refuses with an error built
    /// from a message string, so asking would allocate on every receive.
    ///
    /// **Do not reach for `set_nonblocking` here.** `interprocess`
    /// implements it as `PIPE_NOWAIT`, and in that mode `ReadFile` reports
    /// *success with zero bytes* when nothing is available. The read loop
    /// reads `Ok(0)` as `UnexpectedEof`, which `crashed_on_eof` turns into
    /// `ProcessCrashed`, so a peer that is merely quiet is reported as a
    /// peer that died. Microsoft says the same of the flag itself:
    /// PIPE_NOWAIT exists for LAN Manager 2.0 compatibility and "should not
    /// be used to achieve asynchronous input and output".
    ///
    /// `PeekNamedPipe` answers a different question, "is there data" rather
    /// than "give me data now", and so cannot confuse silence with death.
    pub(super) fn arm(_: &mut ControlStream, _: Duration) -> Self {
        Self
    }

    /// Whether a blocking `read` on this stream would park the thread now.
    ///
    /// It answers conservatively: any failure to peek reports `false`,
    /// handing the decision to the real read. A peek must never be able to
    /// *invent* a reason not to read.
    pub(super) fn would_park(&self, stream: &ControlStream) -> bool {
        matches!(bytes_available(stream), Ok(0))
    }

    /// Nothing to put back: peeking leaves no state on the handle.
    pub(super) fn disarm(self, _: &mut ControlStream) {}
}

/// What a slab is sent over on `stream`: nothing, as its name is in the frame.
pub(crate) fn connection(_: &ControlStream) -> Connection<'_> {
    Connection::detached()
}
