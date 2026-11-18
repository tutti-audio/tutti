//! The unix backend of the main loop: `poll(2)` on the control socket, the
//! plugin's descriptors and the bell. An X11 editor runs its own display
//! connection, so there is no window system to pump; the native loop for the
//! platform that needs one (macOS: `CFRunLoop` with the socket as a source)
//! replaces the wait and the pump together, without touching the loop's logic.

use crate::bell::Bell;
use crate::transport::Transport;
use tutti_plugin_ipc_ffi::{Interest, Ready, Watch};
use tutti_plugin_types::{BridgeResult, MainThreadMarker, Wakeups};

/// The platform's concrete listener and stream, not the crate's dispatch enum.
///
/// Unix needs the concrete stream because only it implements `AsFd`, which the
/// wait polls.
pub(crate) type Listener = interprocess::os::unix::uds_local_socket::Listener;
pub(crate) type Stream = interprocess::os::unix::uds_local_socket::Stream;

/// The reactor: what the loop waits with, and the window system's half (nothing
/// to pump here). It keeps the scratch of the wait, so a steady watch set
/// allocates nothing.
#[derive(Debug, Default)]
pub(crate) struct Native {
    asked: Vec<Watch>,
    got: Vec<Ready>,
}

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`), a descriptor
    /// the plugin asked to have watched (`wakeups`) is ready (into `ready`, the
    /// socket not readable: `Ok(false)`), the `bell` rings, or `timeout` passes
    /// (`Ok(false)`, `ready` empty), then pumps the window system's events. The
    /// socket, the bell and the plugin's descriptors are one `poll(2)`:
    /// whichever is ready first ends the wait the instant it is.
    ///
    /// A transport that is not a socket (a test's script) has no descriptor to
    /// poll: its own wait stands, and no plugin descriptor is watched.
    ///
    /// # Errors
    ///
    /// The transport's wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut dyn Transport,
        main: MainThreadMarker,
        timeout: std::time::Duration,
        wakeups: &Wakeups,
        bell: &Bell,
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        ready.clear();
        let readable = if let Some(stream) = transport.stream() {
            self.wait_on(stream, wakeups, timeout, bell, ready)?
        } else {
            transport.wait_readable(Some(timeout), bell)?
        };
        self.pump(main);
        Ok(readable)
    }

    /// The one `poll(2)` over `stream`, the bell and the plugin's descriptors.
    fn wait_on(
        &mut self,
        stream: &Stream,
        wakeups: &Wakeups,
        timeout: std::time::Duration,
        bell: &Bell,
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        use std::os::fd::AsFd;

        self.asked.clear();
        self.asked.extend_from_slice(wakeups.watches());
        self.asked.push(Watch {
            fd: bell.fd(),
            interest: Interest::READ,
        });
        let readable = loop {
            match tutti_plugin_ipc_ffi::poll_wait(
                Some(stream.as_fd()),
                &self.asked,
                Some(timeout),
                &mut self.got,
            ) {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if self.got.iter().any(|r| r.fd == bell.fd()) {
            bell.drain();
        }
        ready.extend(self.got.iter().filter(|r| r.fd != bell.fd()).copied());
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: MainThreadMarker) {}
}

/// Waits up to `timeout` (`None`: for ever) for `stream` to have something to
/// read: `Ok(true)` when a read now finds a frame begun (or the host gone, which
/// the read reports as the error it is), `Ok(false)` on a timeout or when
/// `wake` rang first. It returns **the instant** either happens.
///
/// # Errors
///
/// The OS refuses the wait.
pub(crate) fn wait_readable(
    stream: &Stream,
    timeout: Option<std::time::Duration>,
    wake: &Bell,
) -> BridgeResult<bool> {
    use std::os::fd::AsFd;

    loop {
        match tutti_plugin_ipc_ffi::wait_readable_either(stream.as_fd(), wake.as_fd(), timeout) {
            // A signal during the wait is not a failure; wait again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
            Ok(readable) => {
                if readable.second {
                    wake.drain();
                }
                return Ok(readable.first);
            }
        }
    }
}
