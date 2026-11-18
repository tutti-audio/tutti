//! The bell on unix: a flag plus a non-blocking socket pair whose read end
//! the main loop's `poll` watches ([`Bell::fd`]). A ring writes one byte, so
//! the wait ends at once.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_plugin_ipc_ffi::FdId;

/// The server's bell. See the module docs.
#[derive(Clone)]
pub(crate) struct Bell {
    shared: Arc<Shared>,
}

struct Shared {
    rung: AtomicBool,
    tx: UnixStream,
    rx: UnixStream,
}

impl Bell {
    /// A bell, built on the thread that owns the server.
    ///
    /// # Errors
    ///
    /// The OS refused the socket pair.
    pub(crate) fn new() -> std::io::Result<Self> {
        let (tx, rx) = UnixStream::pair()?;
        tx.set_nonblocking(true)?;
        rx.set_nonblocking(true)?;
        Ok(Self {
            shared: Arc::new(Shared {
                rung: AtomicBool::new(false),
                tx,
                rx,
            }),
        })
    }

    /// Makes the main loop's wait end. Any thread, the audio thread included.
    pub(crate) fn ring(&self) {
        self.shared.rung.store(true, Ordering::Release);
        // A full socket already holds a byte the loop has not read: the wait
        // ends either way.
        let _ = (&self.shared.tx).write(&[1]);
    }

    /// Whether it rang since the last [`take`](Self::take).
    pub(crate) fn rung(&self) -> bool {
        self.shared.rung.load(Ordering::Acquire)
    }

    /// Whether it rang since the last call, clearing it.
    pub(crate) fn take(&self) -> bool {
        self.shared.rung.swap(false, Ordering::AcqRel)
    }

    /// The descriptor the main loop's wait watches for readability.
    pub(crate) fn fd(&self) -> FdId {
        FdId::new(self.shared.rx.as_raw_fd())
    }

    /// The descriptor, borrowed, for a wait that is also on another.
    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.shared.rx.as_fd()
    }

    /// Waits up to `timeout` (`None`: for ever) for a ring and clears it:
    /// whether one came. The wait of a thread with nothing else to wait on.
    pub(crate) fn wait(&self, timeout: Option<std::time::Duration>) -> bool {
        let rung = loop {
            match tutti_plugin_ipc_ffi::wait_readable(self.as_fd(), timeout) {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other.unwrap_or(true),
            }
        };
        self.clear();
        rung
    }

    /// Clears a ring this thread has woken for: the flag and the byte.
    pub(crate) fn clear(&self) {
        if self.take() {
            self.drain();
        }
    }

    /// Reads the bytes [`ring`](Self::ring) wrote, so the descriptor is quiet
    /// until the next ring.
    pub(crate) fn drain(&self) {
        let mut scratch = [0u8; 64];
        while matches!((&self.shared.rx).read(&mut scratch), Ok(n) if n > 0) {}
    }
}

#[cfg(test)]
mod tests {
    use super::Bell;

    /// Whether the bell's descriptor is readable within `wait`.
    fn readable(bell: &Bell, wait: std::time::Duration) -> bool {
        use std::os::fd::AsFd;
        tutti_plugin_ipc_ffi::wait_readable(bell.shared.rx.as_fd(), Some(wait)).expect("poll")
    }

    /// **A ring ends a wait on the bell's descriptor at once, and a drain
    /// quiets it**: the wait the main loop makes returns on a ring from
    /// another thread, and not again until the next.
    #[test]
    fn a_ring_from_another_thread_ends_a_wait_on_the_descriptor() {
        let bell = Bell::new().expect("a bell");
        assert!(!readable(&bell, std::time::Duration::ZERO));
        let ringer = bell.clone();
        std::thread::spawn(move || ringer.ring())
            .join()
            .expect("ringer");
        assert!(readable(&bell, std::time::Duration::from_secs(600)));
        bell.drain();
        assert!(!readable(&bell, std::time::Duration::ZERO));
        assert!(bell.rung(), "looking does not clear it");
        assert!(bell.take());
        assert!(!bell.take());
    }
}
