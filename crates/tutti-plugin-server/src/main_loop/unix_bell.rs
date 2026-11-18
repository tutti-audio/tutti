//! [`Bell`]: how a plugin's call on any thread makes the main loop run now.
//!
//! The main loop sleeps in one wait over the control socket, the plugin's own
//! descriptors and the next tick. A plugin's `request_process` may be called
//! from its audio thread, which can neither take a lock nor write the control
//! socket, yet its answer (`ProcessRequested`) must leave the server at once,
//! not on the next tick. [`Bell::ring`] sets a flag and makes the platform's
//! wait end; it takes no lock, allocates nothing and never waits. The flag is
//! the one record of the request: the main loop looks at it with
//! [`Bell::rung`] after each wait, and the pass that tells the host takes it
//! with [`Bell::take`]. A `Bell` is built once on the main thread, with the
//! session, and a plugin's wake holds a clone.
//!
//! On unix the bell is a flag plus a non-blocking socket pair whose read end
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
    ///
    /// # Errors
    ///
    /// The OS refused the wait.
    pub(crate) fn wait(&self, timeout: Option<std::time::Duration>) -> std::io::Result<bool> {
        let rung = loop {
            match tutti_plugin_ipc_ffi::wait_readable(self.as_fd(), timeout) {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        self.clear();
        Ok(rung)
    }

    /// Clears a ring this thread has woken for: the flag and the byte. The byte
    /// is read whether or not the flag was set: a ring stores the flag and then
    /// writes the byte, so a clear between the two leaves a byte with no flag,
    /// which a wait that left it would return on at once, for ever.
    pub(crate) fn clear(&self) {
        self.take();
        self.drain();
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

    /// **A byte left with no flag wakes one wait and not the next.** A ring
    /// stores the flag and then writes the byte; a clear between the two takes
    /// the flag and finds no byte, and the byte lands after. That state is made
    /// here in order, and the wait that follows must read the byte: a wait that
    /// left it would be readable for ever, a spin.
    #[test]
    fn a_byte_left_without_its_flag_wakes_one_wait_and_not_the_next() {
        use std::io::Write as _;
        let bell = Bell::new().expect("a bell");
        bell.shared
            .rung
            .store(true, std::sync::atomic::Ordering::Release);
        bell.clear();
        (&bell.shared.tx).write_all(&[1]).expect("the late byte");
        assert!(!bell.rung(), "the flag is gone, the byte is not");
        assert!(bell.wait(Some(std::time::Duration::ZERO)).expect("a wait"));
        assert!(
            !bell.wait(Some(std::time::Duration::ZERO)).expect("a wait"),
            "the byte was read"
        );
        assert!(!readable(&bell, std::time::Duration::ZERO));
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
