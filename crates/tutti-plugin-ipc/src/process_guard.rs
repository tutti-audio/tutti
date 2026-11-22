//! Subprocess lifetime guard.
//!
//! [`ProcessGuard`] owns the `plugin-server` child process and its
//! bridge thread. `Drop` asks the server to shut down, gives it
//! [`Timeouts::shutdown`](tutti_plugin_types::Timeouts::shutdown) to end by
//! itself (its plugin is destroyed in that), then kills it. `tutti-plugin` holds it behind an `Arc` in the node of a served
//! plugin and in the plugin's handle — the subprocess dies when the LAST Arc
//! drops (i.e., after both the graph has released the node *and* every
//! user-held handle has dropped).
//!
//! The process's end is watched by one task on the executor's own thread, the
//! exit task. The guard's drop does not poll the process: it waits for an
//! event, the exit task having ended (the process reaped, or told to stop).

use crate::{IpcConfig, IpcThread, ServerWatch};
use futures_lite::future::yield_now;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::time::Duration;
use tutti_platform::{
    Executor, Instant, Task, Unjoined, WeakExecutor, join_within, monotonic_ns, sleep_until_ns,
};
use tutti_plugin_ipc_ffi::{ExitWait, Reaped};
use tutti_plugin_types::{Deadline, IpcError, PluginFailure};
use tutti_rt::Mutex;

/// How long the exit task waits on the OS between looks at its stop flag.
/// Only bounds how long a dropped guard waits for the task when the process was
/// not the one ending: the end of the process wakes the wait at once.
const EXIT_SLICE: Duration = Duration::from_millis(100);

/// How long a dropped guard waits for the exit task to end once told to stop: it ends at
/// its next look, within one `EXIT_SLICE`, so a few slices is a task stuck in the OS.
const EXIT_STOP_PATIENCE: Duration = Duration::from_millis(500);

/// How long a killed server is waited for before its reaping is handed to
/// another thread: a process in uninterruptible sleep outlasts its kill, and
/// the thread that dropped the guard does not wait on it.
const REAP_PATIENCE: Duration = Duration::from_millis(250);

/// How long a wait for the server's end sleeps between looks at its deadline
/// on a platform with no OS wake; the end of the process wakes the wait at once
/// elsewhere.
const END_SLICE: Duration = Duration::from_millis(20);

/// What the guard and its exit task share: the guard's two instructions to the
/// task, and the task's one answer.
#[derive(Debug, Default)]
struct ExitFlow {
    /// The guard ends the server: its end is no death, so the task does not
    /// declare it.
    ending: AtomicBool,
    /// The task ends at its next look, whether or not the process has.
    stop: AtomicBool,
}

/// Waits up to `timeout` for the exit task in `exit` to end: `true` when it has (its
/// output, a panic, or the executor's shutdown all end it), and then `exit` is empty.
/// A task still running stays in `exit`.
fn ended_within(exit: &mut Option<Task<()>>, timeout: Duration) -> bool {
    match exit.take() {
        None => true,
        Some(task) => match join_within(task, timeout) {
            Ok(_) => true,
            Err(unjoined) => {
                *exit = Some(unjoined.into_task());
                false
            }
        },
    }
}

/// The server's process and what watches it: present for a guard that owns a
/// subprocess, absent for a test guard.
struct Launched {
    /// Shared with the exit task, which asks it for the exit status. Never
    /// locked on the audio thread.
    process: Arc<Mutex<Option<Reaped>>>,
    flow: Arc<ExitFlow>,
    /// The OS's wake for the process's end, shared with the exit task: the
    /// guard waits on it itself once the task is gone.
    wait: Arc<ExitWait>,
    /// The exit task, until it is known to have ended: the guard's drop waits on it,
    /// so every way out of the task (its end, a panic, the executor's shutdown)
    /// resolves it.
    exit: Option<Task<()>>,
    /// Where a server that outlasts its kill is reaped, reached while the
    /// executor lives. No task holds even this: it is upgraded for one call.
    executor: WeakExecutor,
}

/// The `plugin-server` child process and its bridge thread, owned as one.
pub struct ProcessGuard {
    launched: Option<Launched>,
    watch: Arc<ServerWatch>,
    _bridge_thread: Option<IpcThread>,
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
    /// `executor`'s of its own. The guard keeps a weak handle to `executor` to hand
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
        let wait = Arc::new(process.exit_wait());
        let process = Arc::new(Mutex::new(Some(process)));
        let flow = Arc::new(ExitFlow::default());
        let exit = executor.spawn_dedicated(
            "plugin-exit",
            None,
            exit_task(
                Arc::clone(&process),
                Arc::clone(&wait),
                Arc::clone(&watch),
                Arc::clone(&flow),
            ),
        )?;
        Ok(Self {
            launched: Some(Launched {
                process,
                flow,
                wait,
                exit: Some(exit),
                executor: executor.downgrade(),
            }),
            watch,
            _bridge_thread: None,
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
            .lock()
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
    pub fn for_test() -> Self {
        Self {
            launched: None,
            watch: Arc::new(ServerWatch::new()),
            _bridge_thread: None,
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
    ///    platform gives), or within `EXIT_STOP_PATIENCE`, after which it is
    ///    detached with a warning. A process the kill did not end is then handed to the
    ///    executor's blocking pool to be reaped there.
    ///
    /// Every wait is on the exit task's end, an event; nothing polls.
    ///
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
                wait,
                mut exit,
                executor,
            } = launched;
            // A server nobody told to shut down (a failed launch) has no
            // plugin to wait for.
            let grace = if shutting_down {
                self.shutdown
            } else {
                Duration::ZERO
            };
            let mut reaped = reaped_within(&mut exit, &process, &wait, grace);
            if !reaped {
                kill(&process);
                reaped = reaped_within(&mut exit, &process, &wait, REAP_PATIENCE);
            }
            flow.stop.store(true, Ordering::Release);
            // Ends within a slice of the stop, or at once when it has ended.
            if let Some(exit) = exit {
                match join_within(exit, EXIT_STOP_PATIENCE) {
                    Ok(_) => {}
                    Err(Unjoined::TimedOut(exit)) => {
                        tracing::warn!("plugin-ipc: the plugin-exit task did not stop; leaving it");
                        self.watch.note_detached();
                        exit.detach();
                    }
                    Err(Unjoined::Driving(exit)) => exit.detach(),
                }
            }
            let left = process.lock().take();
            if let (false, Some(process)) = (reaped, left) {
                reap_elsewhere(process, &executor);
            }
        }
    }
}

/// Kills the server in `process` and its group, without waiting for it.
fn kill(process: &Mutex<Option<Reaped>>) {
    if let Some(process) = process.lock().as_mut() {
        let _ = process.kill_all();
    }
}

/// Whether the server has been reaped once `timeout` is over: the exit task
/// ends when it reaps the process, so the wait is for its end, and the status
/// is then read once more (a task that ended on an unreadable status reaped
/// nothing).
fn reaped_by(
    exit: &mut Option<Task<()>>,
    process: &Mutex<Option<Reaped>>,
    timeout: Duration,
) -> bool {
    ended_within(exit, timeout)
        && process
            .lock()
            .as_mut()
            .is_some_and(|process| matches!(process.try_reap(), Ok(Some(_))))
}

/// Whether the server has been reaped within `timeout`: by the exit task while
/// it runs, else (the executor has shut down and taken the task, or the task
/// ended without reaping) by waiting on the process itself. A server told to
/// shut down is thus given its whole grace to end on its own whether or not
/// the executor outlives the guard.
fn reaped_within(
    exit: &mut Option<Task<()>>,
    process: &Mutex<Option<Reaped>>,
    wait: &ExitWait,
    timeout: Duration,
) -> bool {
    let deadline = Deadline::after(Instant::now(), timeout);
    if exit.is_some() && reaped_by(exit, process, timeout) {
        return true;
    }
    await_end(process, wait, deadline, Instant::now)
}

/// Waits on the OS for the process in `process` to end, until `deadline` as
/// `now` reads it, and reaps it: whether it ended. The deadline is checked after
/// each look, so a wait that has run out still looks once. The OS's wake ends
/// each wait at the process's end; `ExitWait` paces a platform with none.
fn await_end(
    process: &Mutex<Option<Reaped>>,
    wait: &ExitWait,
    deadline: Deadline,
    mut now: impl FnMut() -> Instant,
) -> bool {
    loop {
        match process.lock().as_mut().map(Reaped::try_reap) {
            Some(Ok(Some(_))) => return true,
            Some(Ok(None)) => {}
            // No process, or a status that cannot be read: an end that cannot be awaited.
            _ => return false,
        }
        if deadline.passed(now()) {
            return false;
        }
        let _ = wait.wait(END_SLICE, pause);
    }
}

/// Reaps a killed `process` on the `executor`'s blocking pool, which ends with
/// it: the reap is a `wait`, which a process in uninterruptible sleep never
/// returns from.
fn reap_elsewhere(process: Reaped, executor: &WeakExecutor) {
    reap_started_by(process, |reap| match executor.upgrade() {
        Some(executor) => executor.unblock(reap),
        // An executor that has shut down starts nothing, as one that refuses.
        None => Err(std::io::Error::other("the executor has shut down")),
    });
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
        move || drop(slot.lock().take())
    };
    match start(Box::new(reap)) {
        Ok(task) => task.detach(),
        // The refused work was dropped unrun, and the process is still in the slot.
        Err(_) => {
            if let Some(process) = slot.lock().take() {
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
/// slices until the process has ended (and then reads its status), or the
/// `flow` says stop.
///
/// The task returns to the executor between slices, so an executor that shuts
/// down drops it within one slice (`EXIT_SLICE`) rather than waiting for the
/// process to end.
async fn exit_task(
    process: Arc<Mutex<Option<Reaped>>>,
    wait: Arc<ExitWait>,
    watch: Arc<ServerWatch>,
    flow: Arc<ExitFlow>,
) {
    while !flow.stop.load(Ordering::Acquire) {
        if wait.wait(EXIT_SLICE, pause) {
            let status = match process.lock().as_mut() {
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
                    // Not a death the guard caused: its drop sets `ending` first.
                    fence(Ordering::SeqCst);
                    if !flow.ending.load(Ordering::Acquire) {
                        watch.declare(PluginFailure::StatusUnreadable {
                            source: Arc::new(IpcError::Io(e)),
                        });
                    }
                    return;
                }
            }
        }
        yield_now().await;
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
            &IpcConfig::new(),
            tutti_fixtures::executor(),
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
        let mut guard = guard_over(Command::new("true"));
        assert!(
            guard.watch().wait_dead(HANG_GUARD),
            "the exit was never declared"
        );
        match guard.watch().cause() {
            Some(PluginFailure::ServerExited { status }) => assert!(status.success()),
            other => panic!("expected ServerExited, got {other:?}"),
        }
        let launched = guard.launched.as_mut().expect("a launched guard");
        assert!(
            ended_within(&mut launched.exit, HANG_GUARD),
            "the exit task outlived the death it declared"
        );
        assert!(
            !guard.watch().declare(PluginFailure::StreamClosed),
            "the latch is set already: a later signal does not set it again"
        );
    }

    /// **A server whose exit status cannot be read is declared**, not left
    /// unwatched: the process is reaped behind the guard's back, so the exit
    /// task's status read fails, and the host learns it from the watch.
    #[test]
    fn a_server_whose_status_cannot_be_read_is_declared() {
        let guard = guard_over(sleeper());
        let pid = guard.pid().expect("a launched guard has a pid");
        let launched = guard.launched.as_ref().expect("a launched guard");
        {
            // The exit task waits for this lock once the process has ended, so the
            // reap below is before its status read.
            let _held = launched.process.lock();
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("a pid"))
                .expect("a pid is not zero");
            rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill");
            rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty())
                .expect("the child is reaped by the test");
        }
        assert!(
            guard.watch().wait_dead(HANG_GUARD),
            "the unreadable status was never declared"
        );
        assert!(
            matches!(
                guard.watch().cause(),
                Some(PluginFailure::StatusUnreadable { .. })
            ),
            "{:?}",
            guard.watch().cause()
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

    /// **A drop returns once the exit task has ended**: the task's future is
    /// gone (it no longer holds the flow) when the drop is over, and the child is
    /// reaped.
    #[test]
    fn a_drop_returns_once_the_exit_task_has_ended() {
        let guard = guard_over(sleeper());
        let pid = guard.pid().expect("a launched guard has a pid");
        let flow = flow_of(&guard);
        assert_eq!(Arc::strong_count(&flow), 3, "the task is watching");
        drop(guard);
        assert_eq!(
            Arc::strong_count(&flow),
            1,
            "the drop returned before the task ended"
        );
        assert!(!exists(pid), "the child {pid} was not reaped");
    }

    /// **A wait for the server's reaping gives up at its timeout and not
    /// before**, and one that is reaped is seen at once: a server that runs is
    /// not reaped by a wait that has run out, and is once it is killed.
    #[test]
    fn a_wait_for_the_reaping_gives_up_at_its_timeout_and_not_before() {
        let mut guard = guard_over(sleeper());
        let launched = guard.launched.as_mut().expect("a launched guard");
        assert!(
            !reaped_by(&mut launched.exit, &launched.process, Duration::ZERO),
            "a running server is not reaped"
        );
        kill(&launched.process);
        assert!(
            reaped_by(&mut launched.exit, &launched.process, HANG_GUARD),
            "the killed server is reaped by the exit task"
        );
    }

    /// **An exit task never polled resolves its task when its executor shuts
    /// down**, so a drop that waits for the task's end never waits for one that
    /// never ran.
    #[test]
    fn an_exit_task_dropped_unpolled_resolves_its_task() {
        let mut process = Reaped::new(sleeper().spawn().expect("sleep spawns"));
        let wait = Arc::new(process.exit_wait());
        let executor = Executor::new();
        let task = executor.spawn(exit_task(
            Arc::new(Mutex::new(None)),
            wait,
            Arc::new(ServerWatch::new()),
            Arc::new(ExitFlow::default()),
        ));

        drop(executor);

        assert!(
            matches!(join_within(task, HANG_GUARD), Ok(None)),
            "the task never resolved"
        );
        process.kill_all().expect("the sleeper is killed");
        process.wait().expect("reaped");
    }

    /// **A guard keeps no executor alive**: with every strong handle dropped
    /// while a guard is alive, the executor has shut down (its exit task ends
    /// with it, not with the process), and the guard's drop still kills its
    /// server.
    #[test]
    fn a_live_guard_does_not_keep_the_executor_alive() {
        let executor = Executor::new();
        let weak = executor.downgrade();
        let child = sleeper().spawn().expect("sleep spawns");
        let pid = child.id();
        let guard = ProcessGuard::launched(Reaped::new(child), &IpcConfig::new(), &executor)
            .expect("the exit task starts");

        drop(executor);

        assert!(
            weak.upgrade().is_none(),
            "the guard kept the executor alive"
        );
        drop(guard);
        // Killed: gone, or an unreaped zombie when nothing could reap it.
        let stat = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        let stat = String::from_utf8_lossy(&stat.stdout);
        assert!(
            stat.trim().is_empty() || stat.trim_start().starts_with('Z'),
            "the child {pid} outlived its guard: {stat}"
        );
    }

    /// **A reap after the executor has shut down abandons the child**, as a
    /// reap the executor refuses does: killed, unreaped, nothing waited for.
    #[test]
    fn a_reap_on_a_dead_executor_abandons_the_child() {
        let child = sleeper().spawn().expect("sleep spawns");
        let pid = child.id();
        let process = Reaped::new(child);
        let wait = process.exit_wait();
        let executor = Executor::new();
        let weak = executor.downgrade();
        drop(executor);

        reap_elsewhere(process, &weak);

        for _ in 0..2 {
            assert!(wait.wait(HANG_GUARD, |_| ()));
        }
        assert!(exists(pid), "the abandoned child was reaped");
    }

    /// **A server that ends on its own is reaped within its grace when the
    /// executor is gone**: the exit task has been dropped with the executor, and
    /// the wait for the server's end goes on, on the process itself, instead of
    /// giving up for a kill.
    #[test]
    fn a_server_ending_on_its_own_is_reaped_within_the_grace_without_an_executor() {
        let executor = Executor::new();
        let mut command = Command::new("cat");
        command.stdin(Stdio::piped());
        let mut child = command.spawn().expect("cat spawns");
        let stdin = child.stdin.take();
        let mut guard = ProcessGuard::launched(Reaped::new(child), &IpcConfig::new(), &executor)
            .expect("the exit task starts");
        drop(executor);
        let launched = guard.launched.as_mut().expect("launched");
        assert!(
            ended_within(&mut launched.exit, HANG_GUARD),
            "the exit task is gone"
        );
        let (end, ends) = std::sync::mpsc::channel::<()>();
        let closer = std::thread::spawn(move || {
            ends.recv().expect("the wait begins");
            drop(stdin);
        });

        end.send(()).expect("the closer waits");
        let reaped = reaped_within(
            &mut launched.exit,
            &launched.process,
            &launched.wait,
            HANG_GUARD,
        );

        closer.join().expect("closer");
        assert!(
            reaped,
            "the server was not given its grace to end by itself"
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
