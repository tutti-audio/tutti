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
//!
//! The process's end is watched by one task on the executor's own thread, the
//! exit task. The guard's drop does not poll the process: it waits for an
//! event, the exit task having ended (the process reaped, or told to stop).

use crate::{IpcConfig, IpcThread, ServerWatch, Wake};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::{Executor, Task, monotonic_ns, sleep_until_ns};
use tutti_plugin_ipc_ffi::{ExitWait, Reaped};
use tutti_plugin_types::PluginFailure;
use tutti_rt::LockExt;

/// How long the exit task waits on the OS between looks at its stop flag.
/// Only bounds how long a dropped guard waits for the task when the process was
/// not the one ending: the end of the process wakes the wait at once.
const EXIT_SLICE: Duration = Duration::from_millis(100);

/// How long a killed server is waited for before its reaping is handed to
/// another thread: a process in uninterruptible sleep outlasts its kill, and
/// the thread that dropped the guard does not wait on it.
const REAP_PATIENCE: Duration = Duration::from_millis(250);

/// What the guard and its exit task share: the guard's two instructions to the
/// task, and the task's one answer.
#[derive(Debug, Default)]
struct ExitFlow {
    /// The guard ends the server: its end is no death, so the task does not
    /// declare it.
    ending: AtomicBool,
    /// The task ends at its next look, whether or not the process has.
    stop: AtomicBool,
    /// The task has ended: set last, by every way out of it.
    done: AtomicBool,
    /// Notified when `done` is set.
    gate: Wake,
}

impl ExitFlow {
    /// Waits up to `timeout` for the task to end: `true` when it has.
    fn wait_done(&self, timeout: Duration) -> bool {
        self.gate.wait_until(Wake::deadline_in(timeout), || {
            self.done.load(Ordering::Acquire)
        })
    }
}

/// Sets [`ExitFlow::done`] when the task ends, however it ends (a panic
/// included), so the guard's wait for it always comes.
struct Finish(Arc<ExitFlow>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        self.0.gate.notify();
    }
}

/// The server's process and what watches it: present for a guard that owns a
/// subprocess, absent for a test guard.
struct Launched {
    /// Shared with the exit task, which asks it for the exit status. Never
    /// locked on the audio thread.
    process: Arc<Mutex<Option<Reaped>>>,
    flow: Arc<ExitFlow>,
    /// The exit task. Dropped last: it has ended by then.
    exit: Task<()>,
    /// Where a server that outlasts its kill is reaped. The task never holds a
    /// clone: it would keep the executor alive for good.
    executor: Executor,
}

/// The `plugin-server` child process and its bridge thread, owned as one.
pub struct ProcessGuard {
    launched: Option<Launched>,
    watch: Arc<ServerWatch>,
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
    /// [`watch`](Self::watch) by a task the OS wakes, on a thread of the
    /// `executor`'s of its own. The guard keeps a clone of `executor` to hand
    /// a server that outlasts its kill to its blocking pool.
    ///
    /// # Errors
    ///
    /// The exit task cannot start: a server whose end nobody watches is not
    /// run, and `process` is killed and reaped as it drops.
    pub fn launched(
        process: Reaped,
        config: &IpcConfig,
        executor: &Executor,
    ) -> std::io::Result<Self> {
        let watch = Arc::new(ServerWatch::new());
        let wait = process.exit_wait();
        let process = Arc::new(Mutex::new(Some(process)));
        let flow = Arc::new(ExitFlow::default());
        let exit = executor.spawn_dedicated("plugin-exit", None, {
            let process = Arc::clone(&process);
            let watch = Arc::clone(&watch);
            let flow = Arc::clone(&flow);
            async move { declare_exit(&process, &wait, &watch, &flow) }
        })?;
        Ok(Self {
            launched: Some(Launched {
                process,
                flow,
                exit,
                executor: executor.clone(),
            }),
            watch,
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
        self.launched
            .as_ref()?
            .process
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
        if let Some(launched) = &self.launched {
            kill(&launched.process);
        }
    }

    /// Test-only: create a guard without a real subprocess. Used by the
    /// served-node and fork tests of `tutti-plugin` over its mock `plugin-server`, and by
    /// this crate's own for `Ipc`.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(socket_path: PathBuf) -> Self {
        Self {
            launched: None,
            watch: Arc::new(ServerWatch::new()),
            _bridge_thread: None,
            socket_path,
            shutdown: Duration::ZERO,
        }
    }
}

impl Drop for ProcessGuard {
    /// Ends the server and waits for the exit task, which is the one that
    /// learns of the process's end:
    ///
    /// 1. The task is told the end is the guard's, and then the bridge is told
    ///    to stop (it says `Shutdown` to the server as it ends, and the server
    ///    destroys its plugin before it exits). The task is told first, so the
    ///    end that follows is never declared a death: the request ends in a
    ///    `SeqCst` fence and the task loads the flag after another, so an end
    ///    read after the request cannot miss the flag (a store-load pair,
    ///    fenced).
    /// 2. The drop waits for the task to end, which it does when the process
    ///    is reaped, for the shutdown timeout (none for a server nobody told to
    ///    shut down). A server still running is killed and waited for once
    ///    more, for `REAP_PATIENCE`.
    /// 3. The task is told to stop, and the drop waits for it to end: at its
    ///    next look, within `EXIT_SLICE` (an OS wake or a nap, whichever the
    ///    platform gives). A process the kill did not end is then handed to the
    ///    executor's blocking pool to be reaped there.
    ///
    /// Every wait is on the exit task's end, an event; nothing polls.
    fn drop(&mut self) {
        let shutting_down = self._bridge_thread.is_some();
        if let Some(launched) = &self.launched {
            launched.flow.ending.store(true, Ordering::Release);
        }
        fence(Ordering::SeqCst);
        if let Some(bridge) = &self._bridge_thread {
            bridge.request_stop();
        }
        if let Some(launched) = self.launched.take() {
            let Launched {
                process,
                flow,
                exit,
                executor,
            } = launched;
            // A server nobody told to shut down (a failed launch) has no
            // plugin to wait for.
            let grace = if shutting_down {
                self.shutdown
            } else {
                Duration::ZERO
            };
            let mut reaped = reaped_by(&flow, &process, grace);
            if !reaped {
                kill(&process);
                reaped = reaped_by(&flow, &process, REAP_PATIENCE);
            }
            flow.stop.store(true, Ordering::Release);
            // Ends within a slice of the stop, or at once when it has ended.
            flow.wait_done(Duration::MAX);
            drop(exit);
            let left = process.lock_recover().take();
            if let (false, Some(process)) = (reaped, left) {
                reap_elsewhere(process, &executor);
            }
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Kills the server in `process` and its group, without waiting for it.
fn kill(process: &Mutex<Option<Reaped>>) {
    if let Some(process) = process.lock_recover().as_mut() {
        let _ = process.kill_all();
    }
}

/// Whether the server has been reaped once `timeout` is over: the exit task
/// ends when it reaps the process, so the wait is for its end, and the status
/// is then read once more (a task that ended on an unreadable status reaped
/// nothing).
fn reaped_by(flow: &ExitFlow, process: &Mutex<Option<Reaped>>, timeout: Duration) -> bool {
    flow.wait_done(timeout)
        && process
            .lock_recover()
            .as_mut()
            .is_some_and(|process| matches!(process.try_reap(), Ok(Some(_))))
}

/// Reaps a killed `process` on the `executor`'s blocking pool, which ends with
/// it: the reap is a `wait`, which a process in uninterruptible sleep never
/// returns from.
fn reap_elsewhere(process: Reaped, executor: &Executor) {
    reap_started_by(process, |reap| executor.unblock(reap));
}

/// [`reap_elsewhere`] over how the reap is started. Work that is refused
/// leaves the zombie to this process's end ([`Reaped::leave`]): the thread
/// that dropped the guard does not wait on it.
fn reap_started_by(
    process: Reaped,
    start: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<Task<()>>,
) {
    let slot = Arc::new(Mutex::new(Some(process)));
    let reap = {
        let slot = Arc::clone(&slot);
        move || drop(slot.lock_recover().take())
    };
    match start(Box::new(reap)) {
        Ok(task) => task.detach(),
        // The refused work was dropped unrun, and the process is still in the slot.
        Err(_) => {
            if let Some(process) = slot.lock_recover().take() {
                process.leave();
            }
        }
    }
}

/// Pauses the exit task's thread for `slice`: what an OS with no wake for a
/// process's end waits between looks.
fn pause(slice: Duration) {
    let nanos = i64::try_from(slice.as_nanos()).unwrap_or(i64::MAX);
    sleep_until_ns(monotonic_ns().saturating_add(nanos));
}

/// The exit task: declares the process's end to `watch`. It waits on the OS in
/// slices until the process has ended (and then reads its status), or `flow`
/// says stop, and sets `flow`'s `done` as it ends.
fn declare_exit(
    process: &Mutex<Option<Reaped>>,
    wait: &ExitWait,
    watch: &ServerWatch,
    flow: &Arc<ExitFlow>,
) {
    let _finish = Finish(Arc::clone(flow));
    while !flow.stop.load(Ordering::Acquire) {
        if !wait.wait(EXIT_SLICE, pause) {
            continue;
        }
        let status = match process.lock_recover().as_mut() {
            Some(process) => process.try_reap(),
            None => return,
        };
        match status {
            Ok(Some(status)) => {
                // Not a death the guard caused: its drop sets `ending` first.
                fence(Ordering::SeqCst);
                if !flow.ending.load(Ordering::Acquire) {
                    watch.declare(PluginFailure::ServerExited { status });
                }
                return;
            }
            // Woken and not ended: a platform that wakes by sleeping, or a
            // wake that raced the status. Ask again.
            Ok(None) => pause(Duration::from_millis(2)),
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
    use std::process::{Command, Stdio};

    /// A hang guard for a wait on an event: far past anything the OS takes.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    fn guard_over(mut command: Command) -> ProcessGuard {
        let child = command.spawn().expect("the child spawns");
        ProcessGuard::launched(
            Reaped::new(child),
            &IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source")),
            &Executor::new(),
        )
        .expect("the exit task starts")
    }

    fn sleeper() -> Command {
        let mut sleeper = Command::new("sleep");
        sleeper.arg("60");
        sleeper
    }

    fn flow_of(guard: &ProcessGuard) -> Arc<ExitFlow> {
        Arc::clone(&guard.launched.as_ref().expect("a launched guard").flow)
    }

    fn exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    }

    /// **A process that ends sets the watch once, with its status as the
    /// cause**, with no bridge reading a socket: the end of the process is a
    /// death signal of its own. The exit task ends with the declaration, so it
    /// declares nothing again.
    #[test]
    fn a_process_that_exits_sets_the_watch_once_with_its_status() {
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
            flow_of(&guard).wait_done(HANG_GUARD),
            "the exit task outlived the death it declared"
        );
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
        let guard = guard_over(sleeper());
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

    /// **A guard's own kill is not a death**: dropping a guard whose child is
    /// alive ends the process, and the watch (still held by a bridge) is not
    /// set by it.
    #[test]
    fn a_guard_that_drops_does_not_declare_the_end_it_causes() {
        let guard = guard_over(sleeper());
        let watch = Arc::clone(guard.watch());
        // The drop kills, reaps and waits for the exit task, so the watch is
        // settled when it returns.
        drop(guard);
        assert!(!watch.is_dead(), "{:?}", watch.cause());
    }

    /// **A drop returns once the exit task has ended**: the task's end, its
    /// latch, is set when the drop is over, and the child is reaped.
    #[test]
    fn a_drop_returns_once_the_exit_task_has_ended() {
        let guard = guard_over(sleeper());
        let pid = guard.pid().expect("a launched guard has a pid");
        let flow = flow_of(&guard);
        assert!(!flow.done.load(Ordering::Acquire), "the task is watching");
        drop(guard);
        assert!(
            flow.done.load(Ordering::Acquire),
            "the drop returned before the task ended"
        );
        assert!(!exists(pid), "the child {pid} was not reaped");
    }

    /// **A wait for the server's reaping gives up at its timeout and not
    /// before**, and one that is reaped is seen at once: a server that runs is
    /// not reaped by a wait that has run out, and is once it is killed.
    #[test]
    fn a_wait_for_the_reaping_gives_up_at_its_timeout_and_not_before() {
        let guard = guard_over(sleeper());
        let launched = guard.launched.as_ref().expect("a launched guard");
        assert!(
            !reaped_by(&launched.flow, &launched.process, Duration::ZERO),
            "a running server is not reaped"
        );
        kill(&launched.process);
        assert!(
            reaped_by(&launched.flow, &launched.process, HANG_GUARD),
            "the killed server is reaped by the exit task"
        );
    }

    /// **A reap that cannot be started leaves the child**: the child is killed
    /// and given up unreaped ([`Reaped::leave`]), and nothing waits for it. A
    /// reaping that waits would have reaped it, and a reaped child no longer
    /// answers `kill -0` (an unreaped one still does).
    #[test]
    fn a_refused_reap_abandons_the_child_without_waiting() {
        let child = sleeper().spawn().expect("sleep spawns");
        let pid = child.id();
        let process = Reaped::new(child);
        let wait = process.exit_wait();

        reap_started_by(process, |_reap| Err(std::io::Error::other("refused")));

        // The first wait only says it may have ended; the second returns when
        // the OS signals the kill's end.
        for _ in 0..2 {
            assert!(wait.wait(HANG_GUARD, |_| ()));
        }
        assert!(exists(pid), "the abandoned child was reaped");
    }

    /// **A reap that is started reaps the child**: the work it is handed ends
    /// the process, and the child is gone.
    #[test]
    fn a_started_reap_reaps_the_child() {
        let child = sleeper().spawn().expect("sleep spawns");
        let pid = child.id();
        let executor = Executor::new();

        reap_started_by(Reaped::new(child), |reap| {
            reap();
            Ok(executor.spawn(async {}))
        });

        assert!(!exists(pid), "the child {pid} was not reaped");
    }
}
