//! The control stream's Unix side: a Unix domain socket, which has a kernel
//! receive timeout and `poll`.

use super::control::ControlStream;
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Name, Stream, ToFsName as _};
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Converts a host-chosen socket path into the local-socket name for this
/// platform.
///
/// The host dials and the server binds with this one function, so both ends
/// name the endpoint the same way. The path is the socket's filesystem path.
///
/// # Errors
///
/// Returns an I/O error when the path is not a valid local-socket name.
// `GenericNamespaced` is not used: on Linux it maps to the abstract namespace,
// but on every other Unix it maps to `SpecialDirUdSocket`, which interprocess
// itself deprecates, and it would move macOS off the filesystem path that works
// there.
pub fn socket_name(socket: &Path) -> std::io::Result<Name<'static>> {
    socket.to_owned().to_fs_name::<GenericFilePath>()
}

/// The longest socket path every Unix takes: `sun_path` is 104 bytes on macOS
/// and the BSDs and 108 on Linux, and the path needs a terminating NUL in it.
/// One figure for all, so a path that fits on Linux fits on macOS too.
const SOCKET_PATH_MAX: usize = 103;

/// `name` in the first of `temp`, `$XDG_RUNTIME_DIR` and `/tmp` where the whole
/// path fits [`SOCKET_PATH_MAX`]. `/tmp` always fits a name
/// [`unique_socket_path`](crate::unique_socket_path) makes.
///
/// `$XDG_RUNTIME_DIR` comes before `/tmp` because it is the user's own (mode
/// 0700); `temp` comes first because it is the one the user chose.
pub fn socket_path_in(temp: &Path, name: &str) -> PathBuf {
    let fits = |p: &PathBuf| p.as_os_str().len() <= SOCKET_PATH_MAX;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute());
    std::iter::once(temp.to_path_buf())
        .chain(runtime)
        .chain(std::iter::once(PathBuf::from("/tmp")))
        .map(|dir| dir.join(name))
        .find(fits)
        .unwrap_or_else(|| temp.join(name))
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
    let (tx, rx) = UnixStream::pair()?;
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
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::traits::Listener as _;

    /// **The control wait ends on a ring or on a byte from the peer, and says
    /// which**: nothing ends a zero wait, a ring is cleared by the wait that
    /// reads it, and a byte stays until it is read.
    #[test]
    fn the_control_wait_ends_on_a_ring_or_a_byte_and_says_which() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("wait.sock");
        let name = crate::transport::control::socket_name(&path).expect("a name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listen");
        let host = crate::transport::control::connect(&path).expect("connect");
        let mut peer = listener.accept().expect("accept");
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

    /// A temporary directory too deep for a socket address moves the socket
    /// to a short one, which a listener binds and a client connects to; one
    /// that fits keeps it. A deep `TMPDIR` (a worktree's scratch directory,
    /// macOS's `/var/folders/…/T/`) failed the plugin server's launch with
    /// "local socket name length exceeds capacity of sun_path".
    #[test]
    fn a_deep_temporary_directory_moves_the_socket() {
        use interprocess::local_socket::ListenerOptions;
        use interprocess::local_socket::traits::Listener as _;

        let base = tempfile::tempdir().expect("a temporary directory");
        let deep = base.path().join("d".repeat(SOCKET_PATH_MAX));
        std::fs::create_dir_all(&deep).expect("the deep directory");
        let name = format!(
            "{}.sock",
            crate::config::unique_name("tutti-bridge").expect("a random source")
        );

        let path = socket_path_in(&deep, &name);
        assert!(!path.starts_with(&deep), "{path:?}");
        assert!(path.as_os_str().len() <= SOCKET_PATH_MAX, "{path:?}");

        let listener = ListenerOptions::new()
            .name(crate::transport::control::socket_name(&path).expect("a name"))
            .create_sync()
            .expect("the moved path binds");
        let connect = path.clone();
        let client = std::thread::spawn(move || crate::transport::control::connect(&connect));
        let _peer = listener.accept().expect("a peer");
        client
            .join()
            .expect("no panic")
            .expect("the client connects");
        drop(listener);
        let _ = std::fs::remove_file(&path);

        let root = std::path::Path::new("/");
        assert_eq!(socket_path_in(root, &name), root.join(&name));
    }
}
