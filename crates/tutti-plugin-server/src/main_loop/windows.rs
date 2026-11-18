//! The Windows backend of the main loop: a named pipe has no `poll`, so the wait
//! is `PeekNamedPipe` with a back-off. A native loop
//! (`MsgWaitForMultipleObjectsEx` over overlapped pipe I/O, which also pumps the
//! Win32 message queue a floating editor needs) replaces the wait and the pump
//! together, without touching the loop's logic.

use crate::bell::Bell;
use crate::transport::Transport;
use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::{BridgeResult, MainThreadMarker, Wakeups};

pub(crate) type Listener = interprocess::local_socket::Listener;
pub(crate) type Stream = interprocess::local_socket::Stream;

/// The reactor's window-system half. Nothing to pump here.
#[derive(Debug, Default)]
pub(crate) struct Native(());

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`), the `bell`
    /// rings or `timeout` passes (`Ok(false)`), then pumps the window system's
    /// events.
    ///
    /// The socket's wait is the transport's ([`wait_readable`] for a real
    /// socket), so a test's scripted transport drives the same loop. A plugin's
    /// descriptors are not waited on here: they are POSIX's (CLAP) and Linux's
    /// (VST3's run loop), so no plugin registers one on this platform, and
    /// `ready` stays empty. Its timers bound `timeout` in the loop.
    ///
    /// # Errors
    ///
    /// The transport's wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut dyn Transport,
        main: MainThreadMarker,
        timeout: std::time::Duration,
        _wakeups: &Wakeups,
        bell: &Bell,
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        ready.clear();
        let readable = transport.wait_readable(Some(timeout), bell)?;
        self.pump(main);
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: MainThreadMarker) {}
}

/// Waits up to `timeout` (`None`: for ever) for `stream` to hold bytes:
/// `Ok(true)` when a read now finds a frame begun (or the host gone),
/// `Ok(false)` on a timeout or when `wake` rang first.
///
/// A named pipe has no `poll`: ask it how many bytes it holds
/// (`PeekNamedPipe`, which never consumes any), spinning politely at first
/// (a block's request arrives within a block period) and napping longer
/// the quieter the pipe has been, never past `timeout`. **A failed peek is
/// "go and read"**, never a verdict: the read is the authority on a closed
/// pipe. The pipe's handle is overlapped, so a wait the OS wakes
/// (`WaitForMultipleObjects` over a pending read and an event) can replace
/// the nap without touching the loop's logic.
///
/// # Errors
///
/// Never on this backend.
pub(crate) fn wait_readable(
    stream: &Stream,
    timeout: Option<std::time::Duration>,
    wake: &Bell,
) -> BridgeResult<bool> {
    use std::os::windows::io::AsHandle;
    use std::time::{Duration, Instant};

    /// Polls served by `yield_now` before any sleeping starts.
    const SPIN_POLLS: u32 = 64;
    /// The longest nap: an idle server wakes this often.
    const LONGEST_NAP: Duration = Duration::from_millis(1);

    let interprocess::local_socket::Stream::NamedPipe(pipe) = stream;
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut empty = 0u32;
    loop {
        if wake.rung() {
            return Ok(false);
        }
        match tutti_plugin_ipc_ffi::pipe_bytes_available(pipe.as_handle()) {
            Ok(0) => {}
            Ok(_) | Err(_) => return Ok(true),
        }
        let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Ok(false);
        }
        empty = empty.saturating_add(1);
        if empty <= SPIN_POLLS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(left.map_or(LONGEST_NAP, |left| LONGEST_NAP.min(left)));
        }
    }
}
