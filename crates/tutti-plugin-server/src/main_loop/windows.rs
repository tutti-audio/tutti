//! The Windows backend of the main loop: a named pipe has no `poll`, so the wait
//! is `PeekNamedPipe` with a back-off. A native loop
//! (`MsgWaitForMultipleObjectsEx` over overlapped pipe I/O, which also pumps the
//! Win32 message queue a floating editor needs) replaces the wait, without
//! touching the loop's logic.

use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::{IpcResult, Wakeups};

#[path = "windows_bell.rs"]
mod bell;

pub(crate) use bell::Bell;

pub(crate) type Listener = interprocess::local_socket::Listener;
pub(crate) type Stream = interprocess::local_socket::Stream;

/// What a slab is received over on `stream`: nothing, as its name is in the frame.
pub(crate) fn connection(_: &Stream) -> tutti_plugin_ipc_ffi::Connection<'_> {
    tutti_plugin_ipc_ffi::Connection::detached()
}

/// The reactor. A named pipe has no descriptor to add the plugin's to, so it
/// holds nothing.
#[derive(Debug, Default)]
pub(crate) struct Native(());

impl Native {
    /// [`wait_readable`] on `stream`. A plugin's descriptors are not waited on
    /// here: they are POSIX's (CLAP) and Linux's (VST3's run loop), so no
    /// plugin registers one on this platform, and `ready` stays empty. Its
    /// timers bound `timeout` in the loop.
    ///
    /// # Errors
    ///
    /// Never on this backend.
    pub(super) fn wait_on(
        &mut self,
        stream: &Stream,
        _wakeups: &Wakeups,
        timeout: std::time::Duration,
        bell: &Bell,
        _ready: &mut Vec<Ready>,
    ) -> IpcResult<bool> {
        wait_readable(stream, Some(timeout), bell)
    }
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
) -> IpcResult<bool> {
    use std::os::windows::io::AsHandle;
    use std::time::Duration;
    use tutti_platform::Instant;

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
