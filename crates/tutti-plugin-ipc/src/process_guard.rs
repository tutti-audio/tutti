//! Subprocess lifetime guard.
//!
//! [`ProcessGuard`] owns the `plugin-server` child process and its
//! bridge thread. `Drop` kills the subprocess and removes the socket
//! file. `tutti-plugin` holds it behind an `Arc` in the node of a served
//! plugin and in the plugin's handle — the subprocess dies when the LAST Arc
//! drops (i.e., after both the graph has released the node *and* every
//! user-held handle has dropped).

use crate::{BridgeConfig, BridgeThread, Reaped, ServerWatch};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tutti_plugin_ipc_ffi::ExitWait;
use tutti_plugin_types::PluginFailure;

/// How long the exit thread waits on the OS between looks at its stop flag.
/// Only bounds how long a dropped guard waits to join it when the process was
/// not the one ending: the end of the process wakes the wait at once.
const EXIT_SLICE: Duration = Duration::from_millis(100);

/// The `plugin-server` child process and its bridge thread, owned as one.
pub struct ProcessGuard {
    /// Shared with the exit thread, which asks it for the exit status. Never
    /// locked on the audio thread.
    process: Arc<Mutex<Option<Reaped>>>,
    watch: Arc<ServerWatch>,
    exit_thread: Option<ExitThread>,
    _bridge_thread: Option<BridgeThread>,
    config: BridgeConfig,
}

/// The thread that declares the process's end to the [`ServerWatch`].
struct ExitThread {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for ProcessGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessGuard")
            .field("pid", &self.pid())
            .finish_non_exhaustive()
    }
}

impl ProcessGuard {
    /// Own the launched `process`; it is killed and reaped when the guard
    /// drops ([`Reaped`]). Its end, however it comes, is declared to
    /// [`watch`](Self::watch) by a thread the OS wakes.
    pub fn launched(process: Reaped, config: BridgeConfig) -> Self {
        let watch = Arc::new(ServerWatch::new());
        let wait = ExitWait::of(&process);
        let process = Arc::new(Mutex::new(Some(process)));
        let stop = Arc::new(AtomicBool::new(false));
        let spawned = std::thread::Builder::new()
            .name("plugin-exit".to_string())
            .spawn({
                let process = Arc::clone(&process);
                let watch = Arc::clone(&watch);
                let stop = Arc::clone(&stop);
                move || declare_exit(&process, &wait, &watch, &stop)
            });
        let exit_thread = match spawned {
            Ok(handle) => Some(ExitThread { stop, handle }),
            Err(e) => {
                // The stream's end still declares a death, only later.
                tracing::warn!("the server's exit cannot be watched: {e}");
                None
            }
        };
        Self {
            process,
            watch,
            exit_thread,
            _bridge_thread: None,
            config,
        }
    }

    /// The latch every signal of this server's death sets: its process ending,
    /// its stream ending, the host ending it. Share it with the bridge, so the
    /// stream's end sets the same latch.
    pub fn watch(&self) -> &Arc<ServerWatch> {
        &self.watch
    }

    /// The OS process id of the guarded `plugin-server`, or `None` for a
    /// test guard that owns no subprocess.
    pub fn pid(&self) -> Option<u32> {
        self.process
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|process| process.id())
    }

    /// Hand over the bridge thread once it exists; it shuts down with the
    /// process.
    pub fn attach(&mut self, bridge_thread: BridgeThread) {
        self._bridge_thread = Some(bridge_thread);
    }

    /// Kills the server without waiting for it, so the bridge thread reads an
    /// EOF and the socket closes. [`Drop`] reaps it. Not for the audio thread
    /// (a lock and a syscall).
    pub fn terminate(&self) {
        let mut process = self.process.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(process) = process.as_mut() {
            let _ = process.kill();
        }
    }

    /// Test-only: create a guard without a real subprocess. Used by
    /// `handle_over` in `tutti-plugin`'s mock-server tests, and by this
    /// crate's own for `Ipc`.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(config: BridgeConfig) -> Self {
        Self {
            process: Arc::new(Mutex::new(None)),
            watch: Arc::new(ServerWatch::new()),
            exit_thread: None,
            _bridge_thread: None,
            config,
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // Stopped first, so the thread reads the end it is about to cause as
        // the guard's and declares nothing.
        if let Some(exit) = &self.exit_thread {
            exit.stop.store(true, Ordering::Release);
        }
        // Killed and reaped as it drops ([`Reaped`]), before the socket goes.
        let process = self
            .process
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(process);
        if let Some(exit) = self.exit_thread.take() {
            let _ = exit.handle.join();
        }
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}

/// Declares the process's end to `watch`: waits on the OS in slices until the
/// process has ended (and then reads its status), or `stop` is set.
fn declare_exit(
    process: &Mutex<Option<Reaped>>,
    wait: &ExitWait,
    watch: &ServerWatch,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        if !wait.wait(EXIT_SLICE) {
            continue;
        }
        let mut process = process.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(process) = process.as_mut() else {
            return;
        };
        match process.try_wait() {
            Ok(Some(status)) => {
                // Not a death the guard caused: its drop sets `stop` first.
                if !stop.load(Ordering::Acquire) {
                    watch.declare(PluginFailure::ServerExited { status });
                }
                return;
            }
            // Woken and not ended: a platform that wakes by sleeping, or a
            // wake that raced the status. Ask again at the next slice.
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                tracing::warn!("the server's exit status cannot be read: {e}");
                return;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::time::Instant;

    fn guard_over(mut command: Command) -> ProcessGuard {
        let child = command.spawn().expect("the child spawns");
        ProcessGuard::launched(
            Reaped::new(child),
            BridgeConfig::new().expect("the OS has a random source"),
        )
    }

    /// Waits for the watch to read dead, as long as a test would wait for
    /// anything the OS does.
    fn dead_within(watch: &ServerWatch, limit: Duration) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if watch.is_dead() {
                return Some(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }

    /// **A process that ends sets the watch once, with its status as the
    /// cause**, with no bridge reading a socket: the end of the process is a
    /// death signal of its own.
    #[test]
    fn a_process_that_exits_sets_the_watch_with_its_status() {
        let guard = guard_over(Command::new("true"));
        assert!(
            dead_within(guard.watch(), Duration::from_secs(5)).is_some(),
            "the exit was never declared"
        );
        match guard.watch().cause() {
            Some(PluginFailure::ServerExited { status }) => assert!(status.success()),
            other => panic!("expected ServerExited, got {other:?}"),
        }
        assert!(
            !guard.watch().declare(PluginFailure::StreamClosed),
            "the latch is set already: a later signal does not set it again"
        );
    }

    /// **A process killed while its socket stays open is seen at once**, by
    /// its end and not by a timeout: nothing here reads a socket, and the wake
    /// is the OS's, so the watch reads dead in far less than a poll or a
    /// deadline would take.
    #[test]
    fn a_killed_process_is_declared_dead_promptly_with_its_signal() {
        let guard = guard_over({
            let mut sleeper = Command::new("sleep");
            sleeper.arg("60");
            sleeper
        });
        assert!(!guard.watch().is_dead(), "a running process is alive");
        let pid = guard.pid().expect("a launched guard has a pid");
        let killed = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .expect("kill runs");
        assert!(killed.success());

        let took = dead_within(guard.watch(), Duration::from_secs(5))
            .expect("the kill was never declared");
        assert!(took < Duration::from_secs(1), "declared after {took:?}");
        match guard.watch().cause() {
            Some(PluginFailure::ServerExited { status }) => {
                assert_eq!(status.signal(), Some(9), "SIGKILL: {status:?}");
            }
            other => panic!("expected ServerExited, got {other:?}"),
        }
    }

    /// **A guard's own kill is not a death**: dropping the guard ends the
    /// process, and the watch (still held by a bridge) is not set by it.
    #[test]
    fn a_guard_that_drops_does_not_declare_the_end_it_causes() {
        let guard = guard_over({
            let mut sleeper = Command::new("sleep");
            sleeper.arg("60");
            sleeper
        });
        let watch = Arc::clone(guard.watch());
        drop(guard);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!watch.is_dead(), "{:?}", watch.cause());
    }
}
