//! The Windows backend of the main loop: a named pipe has no `poll`, so the wait
//! is `PeekNamedPipe` with a back-off, and the host watchdog is a thread asking
//! the OS whether the recorded host still runs. A native loop
//! (`MsgWaitForMultipleObjectsEx` over overlapped pipe I/O, which also pumps the
//! Win32 message queue a floating editor needs) replaces the wait and the pump
//! together, without touching the loop's logic.

use crate::transport::Transport;
use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::{BridgeResult, MainThreadMarker, Wakeups};

pub(crate) type Listener = interprocess::local_socket::Listener;
pub(crate) type Stream = interprocess::local_socket::Stream;

/// The reactor's window-system half. Nothing to pump here.
#[derive(Debug, Default)]
pub(crate) struct Native(());

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`) or `timeout`
    /// passes (`Ok(false)`), then pumps the window system's events.
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
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        ready.clear();
        let readable = transport.wait_readable(timeout)?;
        self.pump(main);
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: MainThreadMarker) {}
}

/// Waits up to `timeout` for `stream` to hold bytes: `Ok(true)` when a read now
/// finds a frame begun (or the host gone), `Ok(false)` on a timeout.
///
/// # Errors
///
/// Never on this backend: a failed peek is "go and read".
/// A named pipe has no `poll`: ask it how many bytes it holds
/// (`PeekNamedPipe`, which never consumes any), spinning politely at first
/// (a block's request arrives within a block period) and napping longer
/// the quieter the pipe has been, never past `timeout`. **A failed peek is
/// "go and read"**, never a verdict: the read is the authority on a closed
/// pipe.
pub(crate) fn wait_readable(stream: &Stream, timeout: std::time::Duration) -> BridgeResult<bool> {
    use std::os::windows::io::AsHandle;
    use std::time::{Duration, Instant};

    /// Polls served by `yield_now` before any sleeping starts.
    const SPIN_POLLS: u32 = 64;
    /// The longest nap: an idle server wakes this often.
    const LONGEST_NAP: Duration = Duration::from_millis(1);

    let interprocess::local_socket::Stream::NamedPipe(pipe) = stream;
    let deadline = Instant::now() + timeout;
    let mut empty = 0u32;
    loop {
        // A plugin rang the bell: the loop has something to send.
        if crate::bell::bell().rung() {
            return Ok(false);
        }
        match tutti_plugin_ipc_ffi::pipe_bytes_available(pipe.as_handle()) {
            Ok(0) => {}
            Ok(_) | Err(_) => return Ok(true),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        empty = empty.saturating_add(1);
        if empty <= SPIN_POLLS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(LONGEST_NAP.min(left));
        }
    }
}

/// The spawning host's process id, recorded once by [`record_parent_pid`]:
/// `None` when neither the host's statement nor the process table names one.
static HOST_PID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

/// How often the watch on the host asks whether it still runs. Also the delay
/// between the host's death and this server's exit while an accept blocks.
const HOST_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Starts the watch on a host that dies while the accept after this blocks, and
/// refuses the connection at once when the host is already gone.
///
/// `interprocess` gives no timed accept and a named pipe no `poll`, so the
/// accept cannot be interleaved with a check: the returned [`Watching`] is a
/// thread that asks the OS whether the host still runs every [`HOST_POLL`] and
/// ends this process when it does not, until the accept returns and the guard
/// drops. A host that dies after it connected is the pipe's EOF, read by the
/// loop.
///
/// A server whose host is unknown (no statement in the environment and no
/// parent in the process table) cannot watch it: that is logged by name, and
/// the accept waits as it did before, with only the host's connection to end
/// it.
pub(crate) fn wait_for_connection(_listener: &Listener) -> BridgeResult<super::Watching> {
    let Some(host) = HOST_PID.get().copied().flatten() else {
        tracing::warn!(
            "the host's process id is unknown: this server cannot notice a host that dies \
             before it connects"
        );
        return Ok(super::Watching::nothing());
    };
    if !tutti_plugin_ipc_ffi::process_exists(host) {
        tracing::info!("host process is gone before it connected; exiting rather than waiting");
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "host process exited before connecting",
        )
        .into());
    }
    let spawned = tutti_rt::spawn_named("host-watchdog", None, move |stop| {
        while !stop.wait(HOST_POLL) {
            if !tutti_plugin_ipc_ffi::process_exists(host) {
                tracing::info!("host process is gone while the server waits to accept; exiting");
                // Terminated outright: a plugin library's detach must not run on an
                // orphan whose other threads are killed mid-call.
                tutti_plugin_ipc_ffi::terminate_process(1);
            }
        }
    });
    match spawned {
        // The watch runs until the guard drops, which stops and joins it.
        Ok(watch) => Ok(super::Watching {
            _watch: Some(watch),
        }),
        Err(e) => {
            tracing::warn!("the host watchdog cannot start: {e}");
            Ok(super::Watching::nothing())
        }
    }
}

/// Records the spawning host's process id: the host's own statement in
/// [`HOST_PID_ENV`](super::HOST_PID_ENV), else the parent in the process table.
///
/// Windows does not reparent orphans, so the table's parent stays whatever it
/// was, pointing at a dead process, which is why [`process_exists`] is the
/// question asked of it. A process id can be reused, so a host that died and
/// whose id was recycled reads as running: that errs toward waiting, the safe
/// direction. **"Cannot determine" means running**, on every path: only a
/// positive answer is a death.
///
/// [`process_exists`]: tutti_plugin_ipc_ffi::process_exists
pub(crate) fn record_parent_pid() {
    HOST_PID.get_or_init(|| {
        std::env::var(super::HOST_PID_ENV)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&pid| pid > 0)
            .or_else(tutti_plugin_ipc_ffi::parent_process_id)
    });
}
