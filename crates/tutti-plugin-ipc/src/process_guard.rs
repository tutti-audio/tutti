//! Subprocess lifetime guard.
//!
//! [`ProcessGuard`] owns the `plugin-server` child process and its
//! bridge thread. `Drop` asks the server to shut down, gives it
//! [`Timeouts::shutdown`](tutti_plugin_types::Timeouts::shutdown) to end by
//! itself (its plugin is destroyed in that), then kills it and removes the
//! socket file. `tutti-plugin` holds it behind an `Arc` in the node of a served
//! plugin and in the plugin's handle — the subprocess dies when the LAST Arc
//! drops (i.e., after both the graph has released the node *and* every
//! user-held handle has dropped).

use crate::{IpcConfig, IpcThread, ServerWatch};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_platform::{StopToken, ThreadHandle};
use tutti_plugin_ipc_ffi::{ExitWait, Reaped};
use tutti_plugin_types::{Deadline, PluginFailure};
use tutti_rt::LockExt;

/// How long the exit thread waits on the OS between looks at its stop flag.
/// Only bounds how long a dropped guard waits to join it when the process was
/// not the one ending: the end of the process wakes the wait at once.
const EXIT_SLICE: Duration = Duration::from_millis(100);

/// How long a wait for the server's end sleeps between looks at its deadline:
/// only the most a wait overshoots it, as the end of the process wakes the wait
/// at once.
const END_SLICE: Duration = Duration::from_millis(20);

/// How long a killed server is waited for before its reaping is handed to
/// another thread: a process in uninterruptible sleep outlasts its kill, and
/// the thread that dropped the guard does not wait on it.
const REAP_PATIENCE: Duration = Duration::from_millis(250);

/// The `plugin-server` child process and its bridge thread, owned as one.
pub struct ProcessGuard {
    /// Shared with the exit thread, which asks it for the exit status. Never
    /// locked on the audio thread.
    process: Arc<Mutex<Option<Reaped>>>,
    /// The OS's wake for the process's end; `None` for a test guard.
    wait: Option<Arc<ExitWait>>,
    watch: Arc<ServerWatch>,
    exit_thread: Option<ThreadHandle<()>>,
    _bridge_thread: Option<IpcThread>,
    /// The control socket's file, removed when the guard drops.
    socket_path: PathBuf,
    /// How long a server told to shut down has to end before it is killed.
    shutdown: Duration,
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
    ///
    /// # Errors
    ///
    /// The exit thread cannot start: a server whose end nobody watches is not
    /// run, and `process` is killed and reaped as it drops.
    pub fn launched(process: Reaped, config: &IpcConfig) -> std::io::Result<Self> {
        let watch = Arc::new(ServerWatch::new());
        let wait = Arc::new(process.exit_wait());
        let process = Arc::new(Mutex::new(Some(process)));
        let spawned = tutti_platform::spawn_named("plugin-exit", None, {
            let process = Arc::clone(&process);
            let wait = Arc::clone(&wait);
            let watch = Arc::clone(&watch);
            move |stop: &StopToken| declare_exit(&process, &wait, &watch, stop)
        });
        Ok(Self {
            process,
            wait: Some(wait),
            watch,
            exit_thread: Some(spawned?),
            _bridge_thread: None,
            socket_path: config.socket_path.clone(),
            shutdown: config.timeouts.shutdown,
        })
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
            .lock_recover()
            .as_ref()
            .map(|process| process.id())
    }

    /// Hand over the bridge thread once it exists; it shuts down with the
    /// process.
    pub fn attach(&mut self, bridge_thread: IpcThread) {
        self._bridge_thread = Some(bridge_thread);
    }

    /// Kills the server without waiting for it, so the bridge thread reads an
    /// EOF and the socket closes. [`Drop`] reaps it. Not for the audio thread
    /// (a lock and a syscall).
    pub fn terminate(&self) {
        let mut process = self.process.lock_recover();
        if let Some(process) = process.as_mut() {
            let _ = process.kill_all();
        }
    }

    /// Test-only: create a guard without a real subprocess. Used by the
    /// served-node and fork tests of `tutti-plugin` over its mock `plugin-server`, and by
    /// this crate's own for `Ipc`.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(socket_path: PathBuf) -> Self {
        Self {
            process: Arc::new(Mutex::new(None)),
            wait: None,
            watch: Arc::new(ServerWatch::new()),
            exit_thread: None,
            _bridge_thread: None,
            socket_path,
            shutdown: Duration::ZERO,
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // The exit thread and then the bridge are told to stop before the
        // end, so neither declares the end a death: the EOF the end causes is
        // read after the bridge's request. (The request ends in a SeqCst
        // fence and `crash` loads the flag after another, so a failure read
        // after the end cannot miss the stop: a store-load pair, fenced.) The
        // bridge says `Shutdown` to the server as it ends, and the server
        // destroys its plugin before it exits.
        let shutting_down = self._bridge_thread.is_some();
        // The exit thread first: the bridge's stop makes it say `Shutdown`, and
        // a server that ends on that is a clean end this guard asked for. A
        // server that ended before the exit thread was told would be declared
        // dead by it.
        if let Some(exit) = &self.exit_thread {
            exit.token().stop();
        }
        if let Some(bridge) = &self._bridge_thread {
            bridge.request_stop();
        }
        let process = self.process.lock_recover().take();
        if let (Some(mut process), Some(wait)) = (process, &self.wait) {
            // A server nobody told to shut down (a failed launch) has no
            // plugin to wait for.
            let grace = if shutting_down {
                self.shutdown
            } else {
                Duration::ZERO
            };
            if !await_end(
                &mut process,
                wait,
                Deadline::after(Instant::now(), grace),
                Instant::now,
            ) {
                let _ = process.kill_all();
                let reaped = await_end(
                    &mut process,
                    wait,
                    Deadline::after(Instant::now(), REAP_PATIENCE),
                    Instant::now,
                );
                if !reaped {
                    reap_elsewhere(process);
                }
            }
        }
        // Stops (again) and joins.
        drop(self.exit_thread.take());
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Waits on the OS for `process` to end, until `deadline` as `now` reads it,
/// and reaps it: whether it ended. The deadline is checked after each look, so
/// a wait that has run out still looks once.
fn await_end(
    process: &mut Reaped,
    wait: &ExitWait,
    deadline: Deadline,
    mut now: impl FnMut() -> Instant,
) -> bool {
    loop {
        match process.try_reap() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // A status that cannot be read is an end that cannot be awaited.
            Err(_) => return false,
        }
        if deadline.passed(now()) {
            return false;
        }
        let _ = wait.wait(END_SLICE, std::thread::sleep);
    }
}

/// Reaps a killed `process` on a thread of its own, which ends with it. A
/// thread that cannot start leaves the zombie to this process's end: the reap
/// is a `wait`, which a process in uninterruptible sleep never returns from.
fn reap_elsewhere(process: Reaped) {
    let slot = Arc::new(Mutex::new(Some(process)));
    let spawned = tutti_platform::spawn_named("plugin-reap", None, {
        let slot = Arc::clone(&slot);
        move |_: &StopToken| drop(slot.lock_recover().take())
    });
    match spawned {
        Ok(handle) => handle.detach(),
        Err(_) => std::mem::forget(slot.lock_recover().take()),
    }
}

/// Declares the process's end to `watch`: waits on the OS in slices until the
/// process has ended (and then reads its status), or `stop` is set.
fn declare_exit(
    process: &Mutex<Option<Reaped>>,
    wait: &ExitWait,
    watch: &ServerWatch,
    stop: &StopToken,
) {
    while !stop.is_stopped() {
        if !wait.wait(EXIT_SLICE, |slice| {
            let _ = stop.wait(slice);
        }) {
            continue;
        }
        let mut process = process.lock_recover();
        let Some(process) = process.as_mut() else {
            return;
        };
        match process.try_reap() {
            Ok(Some(status)) => {
                // Not a death the guard caused: its drop sets `stop` first.
                if !stop.is_stopped() {
                    watch.declare(PluginFailure::ServerExited { status });
                }
                return;
            }
            // Woken and not ended: a platform that wakes by sleeping, or a
            // wake that raced the status. Ask again at the next slice.
            Ok(None) => {
                if stop.wait(Duration::from_millis(2)) {
                    return;
                }
            }
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

    /// A hang guard for the wait on the watch: far past anything the OS takes.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    fn guard_over(mut command: Command) -> ProcessGuard {
        let child = command.spawn().expect("the child spawns");
        ProcessGuard::launched(
            Reaped::new(child),
            &IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source")),
        )
        .expect("the exit thread starts")
    }

    /// **A process that ends sets the watch once, with its status as the
    /// cause**, with no bridge reading a socket: the end of the process is a
    /// death signal of its own.
    #[test]
    fn a_process_that_exits_sets_the_watch_with_its_status() {
        let guard = guard_over(Command::new("true"));
        assert!(
            guard.watch().wait_dead(HANG_GUARD),
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

        // Nothing reads a socket here, so only the process's end can set the
        // watch; a bound under every deadline the host waits on.
        assert!(
            guard.watch().wait_dead(HANG_GUARD),
            "the kill was never declared"
        );
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
        // The drop kills, reaps and joins the exit thread, so the watch is
        // settled when it returns.
        drop(guard);
        assert!(!watch.is_dead(), "{:?}", watch.cause());
    }

    /// **A server that does not end is waited on until the deadline and no
    /// longer**, and one that ends is seen at once: the deadline is a rule over
    /// the `now` it is asked, here a clock that jumps a second per look, so the
    /// wait ends by the rule and not by a real second passing.
    #[test]
    fn a_wait_for_the_end_gives_up_at_its_deadline_and_not_before() {
        let mut command = Command::new("sleep");
        command.arg("60");
        let mut process = Reaped::new(command.spawn().expect("sleep spawns"));
        let wait = process.exit_wait();
        let start = Instant::now();
        let looks = std::cell::Cell::new(0_u32);
        let now = || {
            looks.set(looks.get() + 1);
            start + Duration::from_secs(u64::from(looks.get()))
        };

        let ended = await_end(
            &mut process,
            &wait,
            Deadline::after(start, Duration::from_secs(3)),
            now,
        );

        assert!(!ended, "a sleeping process did not end");
        assert_eq!(looks.get(), 3, "the wait looked until the third second");
        assert!(process.try_reap().expect("status").is_none());

        process.kill_all().expect("the sleeper is killed");
        let wait_for_kill = Deadline::never();
        assert!(await_end(&mut process, &wait, wait_for_kill, Instant::now));
    }
}
