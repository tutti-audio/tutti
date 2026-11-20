//! [`watch_host`]: a task that runs a function when a named process ends, and
//! [`watch_spawner`]: the same for the process that spawned this one.

use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

use tutti_platform::{Executor, Task, monotonic_ns, sleep_until_ns};

use super::platform::Imp;

/// How often the watch asks whether the host still runs on a platform where
/// the OS gives no wake for another process's end, or refused one, and the
/// longest the OS wait runs before the task is polled again: a dropped
/// [`Task`] ends the watch within this.
const NAP: Duration = Duration::from_millis(250);

/// Pauses the dedicated thread for `duration`: the platform's blocking sleep.
fn pause(duration: Duration) {
    let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
    sleep_until_ns(monotonic_ns().saturating_add(nanos));
}

/// Runs `on_gone` once the process `host` has ended, on a thread of the
/// `executor`'s of its own named `host-watch`, for a server whose host dying must
/// end it wherever it is blocked. The task runs until the host ends or the
/// returned [`Task`] is dropped, which cancels the watch within a quarter of a
/// second; [`detach`](Task::detach) keeps it for the life of the executor.
///
/// The wait is the OS's (a pidfd on Linux, a kqueue event on macOS, the process
/// handle on Windows), so nothing polls. Where it gives none, or refused one,
/// the task asks [`process_exists`](crate::process_exists) every 250 ms. A
/// `host` that has ended already runs `on_gone` at once.
///
/// # Errors
///
/// The thread could not be started.
pub fn watch_host(
    executor: &Executor,
    host: u32,
    on_gone: impl FnOnce() + Send + 'static,
) -> std::io::Result<Task<()>> {
    watch(executor, host, true, pause, on_gone)
}

/// [`watch_host`] over its two choices: whether to ask the OS for a wake, and how
/// the poll pauses between asks.
fn watch(
    executor: &Executor,
    host: u32,
    os_wake: bool,
    mut nap: impl FnMut(Duration) + Send + 'static,
    on_gone: impl FnOnce() + Send + 'static,
) -> std::io::Result<Task<()>> {
    executor.spawn_dedicated("host-watch", None, async move {
        let imp = if os_wake { Imp::of_pid(host) } else { None };
        loop {
            match &imp {
                // Ended: the wait says "may have ended" for a signal that
                // interrupted it, so the end is read again before it is believed.
                Some(imp) => {
                    if imp.wait(NAP) && imp.wait(Duration::ZERO) {
                        break;
                    }
                }
                None => {
                    if !crate::process_exists(host) {
                        break;
                    }
                    nap(NAP);
                }
            }
            yield_now().await;
        }
        on_gone();
    })
}

/// Returns to the executor once, so that a cancelled [`Task`] is seen between
/// two waits.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

/// The environment variable through which a host states its own PID to the
/// process it spawns (a plugin server, a probe), which watches the host's end.
///
/// A child cannot just ask the kernel: a host that dies during the child's
/// startup is reparented before the child's first instruction, so a `getppid()`
/// sample names the reaper (under `PR_SET_CHILD_SUBREAPER`: systemd user
/// services, Docker `--init`, Flatpak, Snap) rather than the host. Stating it at
/// spawn fixes the value before the child exists, and [`host_pid`] weighs it
/// against the parent.
pub const HOST_PID_ENV: &str = "TUTTI_PLUGIN_HOST_PID";

/// What a spawned process knows of the host that spawned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// The host runs: the process to watch. It is the parent, or, where a wrapper
    /// stands between host and child (a shell script, `strace`, a sandbox), a
    /// running process the host stated.
    Running(u32),
    /// The host stated its pid and no such process runs: it ended.
    Gone,
    /// No host is stated and no parent is known (a process started by hand, under
    /// init).
    Unknown,
}

/// The host of this process, from [`HOST_PID_ENV`] and the parent process.
#[must_use]
pub fn host_pid() -> Host {
    host_given(
        std::env::var(HOST_PID_ENV).ok().as_deref(),
        crate::parent_process_id(),
        crate::process_exists,
    )
}

/// [`host_pid`] over its sources: the statement, the parent, and whether a pid
/// runs. A statement that is no process id past init's counts as none, and a
/// process with none watches its parent. A statement is the host while it names
/// the parent or a process that runs: a wrapper between host and child makes the
/// parent another process, and ending the child for that would end every child
/// behind a wrapper. A pid the OS has since given to another process is watched
/// as the host, so that child is left unwatched: bounded by the host's own
/// deadline, never ended by mistake.
fn host_given(stated: Option<&str>, parent: Option<u32>, exists: impl FnOnce(u32) -> bool) -> Host {
    let stated = stated
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&pid| pid > 1);
    match (stated, parent) {
        (Some(stated), Some(parent)) if stated == parent => Host::Running(parent),
        (Some(stated), _) if exists(stated) => Host::Running(stated),
        (Some(_), _) => Host::Gone,
        (None, Some(parent)) => Host::Running(parent),
        (None, None) => Host::Unknown,
    }
}

/// [`watch_host`] on the host that spawned this process ([`host_pid`]): runs
/// `on_gone` on a thread of the `executor`'s when it ends, or at once if it
/// already has. Returns `None`, watching nothing, when the host is unknown.
///
/// # Errors
///
/// The thread could not be started.
pub fn watch_spawner(
    executor: &Executor,
    on_gone: impl FnOnce() + Send + 'static,
) -> std::io::Result<Option<Task<()>>> {
    match host_pid() {
        Host::Running(host) => watch_host(executor, host, on_gone).map(Some),
        // Nothing polls here: the thread only runs the function.
        Host::Gone => executor
            .spawn_dedicated("host-watch", None, async move { on_gone() })
            .map(Some),
        Host::Unknown => Ok(None),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    /// The most a test waits for an event: a guard against a hang.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// **The OS wakes a watch on a running process at its end.** The watch is
    /// registered before the process is killed, so the wake is the OS's and not
    /// a look afterwards: it outlasts the test harness's limit if it never comes.
    #[test]
    fn the_os_wakes_a_watch_on_a_running_process_at_its_end() {
        let mut child = Command::new("sleep").arg("600").spawn().expect("sleep");
        let watch = Imp::of_pid(child.id()).expect("a running process can be watched");
        child.kill().expect("kill");
        assert!(
            watch.wait(Duration::from_secs(600)),
            "its end wakes the watch"
        );
        child.wait().expect("reaped");
    }

    /// A child that runs until it is killed.
    fn running() -> std::process::Child {
        Command::new("sleep").arg("600").spawn().expect("sleep")
    }

    /// Drops `executor`, which joins the watch's thread, and counts the messages
    /// still in `ended`: how many more times `on_gone` ran.
    fn runs_after(executor: Executor, ended: &mpsc::Receiver<()>) -> usize {
        drop(executor);
        ended.iter().count()
    }

    /// **The end of the host runs the function once.** The child is reaped, so
    /// the function runs whether the task registered its watch before the end or
    /// found the process gone. The executor's drop joins the watch's thread and
    /// drops the sender, so the receiver sees every run.
    #[test]
    fn the_end_of_the_host_runs_the_function_once() {
        let executor = Executor::new();
        let mut child = running();
        let (gone, ended) = mpsc::channel();
        let task = watch_host(&executor, child.id(), move || {
            gone.send(()).expect("the test listens");
        })
        .expect("the watch starts");

        child.kill().expect("kill");
        child.wait().expect("reaped");
        ended.recv_timeout(HANG_GUARD).expect("its end is learned");
        drop(task);
        assert_eq!(runs_after(executor, &ended), 0, "the function ran again");
    }

    /// **A host that ended before the watch began runs the function too.**
    #[test]
    fn a_host_that_ended_before_the_watch_runs_the_function() {
        let executor = Executor::new();
        let mut child = Command::new("true").spawn().expect("true");
        let pid = child.id();
        child.wait().expect("reaped");
        let (gone, ended) = mpsc::channel();
        let task = watch_host(&executor, pid, move || {
            gone.send(()).expect("the test listens");
        })
        .expect("the watch starts");
        ended.recv_timeout(HANG_GUARD).expect("its end is learned");
        drop(task);
        assert_eq!(runs_after(executor, &ended), 0);
    }

    /// **Without an OS wake the poll ends when the host ends.** The pause is
    /// injected: its first call is where the host ends, so the watch sees the
    /// process gone at its next ask and runs the function once, after one pause.
    #[test]
    fn the_poll_without_a_wake_ends_when_the_host_ends() {
        let executor = Executor::new();
        let mut child = running();
        let pid = child.id();
        let pauses = Arc::new(AtomicUsize::new(0));
        let (gone, ended) = mpsc::channel();
        let task = watch(
            &executor,
            pid,
            false,
            {
                let pauses = Arc::clone(&pauses);
                move |_| {
                    pauses.fetch_add(1, Ordering::SeqCst);
                    child.kill().expect("kill");
                    child.wait().expect("reaped");
                }
            },
            move || gone.send(()).expect("the test listens"),
        )
        .expect("the watch starts");
        ended.recv_timeout(HANG_GUARD).expect("its end is learned");
        drop(task);
        assert_eq!(runs_after(executor, &ended), 0);
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
    }

    /// **A dropped task ends the watch without running the function**, even
    /// while the host runs: the task is polled (its first pause says so) before it
    /// is dropped, and the executor's drop would otherwise join a thread that
    /// never returns.
    #[test]
    fn a_dropped_watch_never_runs_the_function() {
        let executor = Executor::new();
        let mut child = running();
        let (gone, ended) = mpsc::channel();
        let (polled, started) = mpsc::channel();
        let task = watch(
            &executor,
            child.id(),
            false,
            move |_| {
                let _ = polled.send(());
            },
            move || gone.send(()).expect("the test listens"),
        )
        .expect("the watch starts");
        started.recv_timeout(HANG_GUARD).expect("the watch runs");
        drop(task);
        assert_eq!(runs_after(executor, &ended), 0);
        child.kill().expect("kill");
        child.wait().expect("reaped");
    }

    /// **A stated host is the host while it runs, parent or not**: a wrapper
    /// between host and child makes the parent another process, and only a stated
    /// pid that no process holds is a host that ended. A statement that is no pid
    /// counts as none.
    #[test]
    fn a_stated_host_is_the_host_while_it_runs() {
        let runs = |_: u32| true;
        let gone = |_: u32| false;
        assert_eq!(
            host_given(Some("4242"), Some(4242), gone),
            Host::Running(4242)
        );
        // A wrapper: the parent is not the host, and the host runs.
        assert_eq!(host_given(Some("4242"), Some(7), runs), Host::Running(4242));
        assert_eq!(host_given(Some("4242"), None, runs), Host::Running(4242));
        assert_eq!(host_given(Some("4242"), Some(7), gone), Host::Gone);
        assert_eq!(host_given(Some("4242"), None, gone), Host::Gone);
        assert_eq!(host_given(None, Some(7), gone), Host::Running(7));
        assert_eq!(host_given(None, None, runs), Host::Unknown);
        for unusable in ["", "x", "-5", "4294967296", "1", "0"] {
            assert_eq!(
                host_given(Some(unusable), Some(7), runs),
                Host::Running(7),
                "{unusable:?}"
            );
        }
    }
}
