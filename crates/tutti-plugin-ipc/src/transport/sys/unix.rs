//! The control stream's Unix side: a Unix domain socket, which has a kernel
//! receive timeout and `poll`. The host makes the socket pair and hands the
//! server its end as the standard input it is spawned with, so nothing names the
//! connection.

use super::control::ControlStream;
use crate::ProcessGuard;
use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::Stream as _;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tutti_plugin_ipc_ffi::{Connection, LaneWire, Reaped, Spawn};
use tutti_plugin_types::IpcResult;
use tutti_rt::Mutex;

/// The server a host has spawned, as the host reaches it: the host's end of the
/// socket pair, already connected.
pub(crate) struct Rendezvous(ControlStream);

/// Held across making a socket pair and, for the server's connection, spawning the
/// server that is given its end: macOS sets close-on-exec after `socketpair`, not
/// with it, so a spawn that ran between the two would hand a server both ends of
/// another plugin's lane. **It covers every socket pair this crate makes in the
/// host** (the connection, the audio lane, the control wait's bell). The slab's
/// memfd and `shm_open` are close-on-exec from the call. A spawn by another part
/// of the process is outside it.
static SPAWN: Mutex<()> = Mutex::new(());

/// Makes a socket pair under [`SPAWN`].
fn pair() -> std::io::Result<(UnixStream, UnixStream)> {
    let _spawning = SPAWN.lock();
    UnixStream::pair()
}

/// Spawns `command` with the server's end of a fresh socket pair as its standard
/// input (the only descriptor it inherits besides the other two standard ones),
/// and returns the server and the host's end, under the send timeout.
///
/// # Errors
///
/// The OS refused the socket pair or the spawn.
pub(crate) fn spawn(mut command: Command) -> IpcResult<(Reaped, Rendezvous)> {
    let _spawning = SPAWN.lock();
    let (host, server) = UnixStream::pair()?;
    command.stdin(Stdio::from(OwnedFd::from(server)));
    let server =
        tutti_plugin_ipc_ffi::spawn(command, Spawn::Leader).map_err(crate::error::spawn_failed)?;
    // The command's copy of the server's end closed in the spawn: the server's is
    // the last one, so its end of the process is the host's end of stream.
    let host = Stream::UdSocket(OwnedFd::from(host).into());
    let _ = host.set_send_timeout(Some(super::control::SEND_TIMEOUT));
    Ok((server, Rendezvous(host)))
}

impl Rendezvous {
    /// The host's end of the connection, which exists from the spawn.
    ///
    /// # Errors
    ///
    /// Never on unix.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn connect(
        self,
        _server: &ProcessGuard,
        _patience: Duration,
    ) -> IpcResult<ControlStream> {
        Ok(self.0)
    }
}

/// What a slab is sent over on `stream`: the socket's descriptor.
pub(crate) fn connection(stream: &ControlStream) -> Connection<'_> {
    let Stream::UdSocket(socket) = stream;
    socket.as_fd()
}

/// A lane the host is opening: both ends of a fresh socket pair, until the
/// server's is sent and the host's becomes the lane's stream.
pub(crate) struct PendingLane {
    wire: LaneWire,
    host: OwnedFd,
    server: OwnedFd,
}

impl PendingLane {
    /// A fresh lane.
    ///
    /// # Errors
    ///
    /// The OS refused the socket pair.
    pub(crate) fn new() -> IpcResult<Self> {
        let (host, server) = pair()?;
        let (host, server) = (OwnedFd::from(host), OwnedFd::from(server));
        Ok(Self {
            wire: LaneWire::fresh()?,
            host,
            server,
        })
    }

    /// What the `OpenLane` frame carries of the lane.
    pub(crate) fn wire(&self) -> LaneWire {
        self.wire
    }

    /// Sends the server's end over `broker`, after the frame that announces it.
    pub(crate) fn send_end(&self, broker: Connection<'_>) -> std::io::Result<()> {
        tutti_plugin_ipc_ffi::send_lane(broker, self.server.as_fd())
    }

    /// The host's end of the lane, once the server has said it holds its own.
    ///
    /// # Errors
    ///
    /// Never on unix.
    pub(crate) fn stream(self) -> IpcResult<ControlStream> {
        let stream = Stream::UdSocket(self.host.into());
        let _ = stream.set_send_timeout(Some(super::control::SEND_TIMEOUT));
        Ok(stream)
    }
}

/// The two ends of a broker connection, host's first, for the mock servers of
/// the tests: a socket pair, with the host's end under the send timeout
/// of a launched server's connection.
///
/// # Panics
///
/// The OS refuses the socket pair.
#[cfg(any(test, feature = "testing"))]
pub fn broker_pair() -> (ControlStream, ControlStream) {
    let (host, server) = UnixStream::pair().expect("a socket pair");
    let host = Stream::UdSocket(OwnedFd::from(host).into());
    let _ = host.set_send_timeout(Some(super::control::SEND_TIMEOUT));
    (host, Stream::UdSocket(OwnedFd::from(server).into()))
}

/// A second handle on a control stream that only *waits* for it to have
/// something to read, from a thread of its own: it never reads, so the thread
/// that does keeps every byte and its frame cursor.
///
/// The duplicated descriptor is `poll`ed, so the wait ends the instant a byte
/// (or a hang-up) arrives. Holding it keeps the socket open: drop it promptly
/// when the stream's owner is done.
pub struct Watch {
    fd: OwnedFd,
}

impl Watch {
    /// A watch on `stream`.
    ///
    /// # Errors
    ///
    /// The OS refuses to duplicate the descriptor.
    pub fn of(stream: &ControlStream) -> std::io::Result<Self> {
        let Stream::UdSocket(socket) = stream;
        Ok(Self {
            fd: socket.as_fd().try_clone_to_owned()?,
        })
    }

    /// Waits up to `timeout` for the stream to have something to read.
    /// `true` when it has, or when asking fails (a read is the authority on a
    /// closed or broken stream); `false` on a timeout.
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        tutti_plugin_ipc_ffi::wait_readable(self.fd.as_fd(), Some(timeout)).unwrap_or(true)
    }
}

/// The unparking side of a control thread's wait ([`control_unparker`]): wakes the
/// wait from any thread, the audio thread included.
///
/// A non-blocking socket pair whose read end the wait polls beside the control
/// stream; an unpark writes one byte, so the wait ends at once. It takes no lock and
/// allocates nothing.
#[derive(Clone)]
pub struct ControlUnparker {
    tx: Arc<UnixStream>,
}

impl ControlUnparker {
    /// Ends the control thread's wait now, or its next one.
    pub fn unpark(&self) {
        // A full socket already holds a byte the thread has not read: its wait
        // ends either way.
        let _ = (&*self.tx).write(&[1]);
    }
}

/// The waiting side of a control thread's wait ([`control_unparker`]).
pub struct ControlWait {
    rx: UnixStream,
}

/// What ended a [`ControlWait::wait`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Woken {
    /// The control stream has something to read (or hung up: the read says).
    pub stream: bool,
    /// The bell rang.
    pub rung: bool,
}

/// A control thread's bell and its wait.
///
/// # Errors
///
/// The OS refused the socket pair.
pub fn control_unparker() -> std::io::Result<(ControlUnparker, ControlWait)> {
    let (tx, rx) = pair()?;
    tx.set_nonblocking(true)?;
    rx.set_nonblocking(true)?;
    Ok((ControlUnparker { tx: Arc::new(tx) }, ControlWait { rx }))
}

impl ControlWait {
    /// Waits up to `timeout` (`None`: for ever) for `stream` to have something
    /// to read or for the bell to ring, in one `poll`, and returns the instant
    /// either happens. A ring is cleared.
    ///
    /// # Errors
    ///
    /// The OS refused the wait.
    pub fn wait(
        &self,
        stream: &ControlStream,
        timeout: Option<Duration>,
    ) -> std::io::Result<Woken> {
        let Stream::UdSocket(socket) = stream;
        let ready = loop {
            match tutti_plugin_ipc_ffi::wait_readable_either(
                socket.as_fd(),
                self.rx.as_fd(),
                timeout,
            ) {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if ready.second {
            let mut scratch = [0u8; 64];
            while matches!((&self.rx).read(&mut scratch), Ok(n) if n > 0) {}
        }
        Ok(Woken {
            stream: ready.first,
            rung: ready.second,
        })
    }
}

/// How a blocked read is made to return so the deadline can be re-checked.
pub(super) enum Wakeup {
    /// `SO_RCVTIMEO`. The kernel blocks for the poll interval before returning
    /// `WouldBlock`, so the read loop must not sleep on top of that.
    Timeout,
    /// No wakeup could be armed. The deadline still holds between syscalls, but
    /// a single blocked read can overshoot it.
    None,
}

impl Wakeup {
    pub(super) fn arm(stream: &mut ControlStream, poll: Duration) -> Self {
        if stream.set_recv_timeout(Some(poll)).is_ok() {
            return Self::Timeout;
        }
        tracing::debug!(
            "no wakeup available on the control stream: a {poll:?} receive \
             timeout could not be set. The total deadline still applies \
             between reads, but a single blocked read may overshoot it"
        );
        Self::None
    }

    /// Whether a blocking `read` on this stream would park the thread now.
    /// Never on Unix: under `Timeout` the kernel bounds the read, and under
    /// `None` nothing can be asked.
    pub(super) fn would_park(&self, _stream: &ControlStream) -> bool {
        false
    }

    /// Put the stream back as it was, undoing only what was actually done:
    /// clearing a timeout that was never set would be a second spurious error.
    pub(super) fn disarm(self, stream: &mut ControlStream) {
        match self {
            Self::Timeout => {
                let _ = stream.set_recv_timeout(None);
            }
            Self::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The control wait ends on a ring or on a byte from the peer, and says
    /// which**: nothing ends a zero wait, a ring is cleared by the wait that
    /// reads it, and a byte stays until it is read.
    #[test]
    fn the_control_wait_ends_on_a_ring_or_a_byte_and_says_which() {
        let (host, mut peer) = super::super::broker_pair();
        let (bell, wait) = control_unparker().expect("a bell");
        let now = Some(Duration::ZERO);

        assert_eq!(wait.wait(&host, now).expect("wait"), Woken::default());
        bell.unpark();
        let rung = Woken {
            stream: false,
            rung: true,
        };
        assert_eq!(wait.wait(&host, now).expect("wait"), rung);
        assert_eq!(
            wait.wait(&host, now).expect("wait"),
            Woken::default(),
            "the ring was cleared"
        );

        peer.write_all(&[1]).expect("a byte");
        let heard = Woken {
            stream: true,
            rung: false,
        };
        assert_eq!(
            wait.wait(&host, Some(Duration::from_secs(60)))
                .expect("wait"),
            heard
        );
        assert_eq!(
            wait.wait(&host, now).expect("wait"),
            heard,
            "unread: still there"
        );
    }
}
