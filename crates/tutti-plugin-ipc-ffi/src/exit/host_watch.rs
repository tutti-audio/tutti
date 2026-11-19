//! [`watch_host`]: a thread that runs a function when a named process ends, and
//! [`watch_spawner`]: the same for the process that spawned this one.

use std::time::Duration;

use super::platform::Imp;

/// How often the watch asks whether the host still runs on a platform where
/// the OS gives no wake for another process's end, or refused one.
const NAP: Duration = Duration::from_millis(250);

/// Runs `on_gone` on a thread of its own once the process `host` has ended, for
/// a server whose host dying must end it wherever it is blocked. The thread
/// runs for the life of the process, or until the host ends, and is named
/// `host-watch`.
///
/// The wait is the OS's (a pidfd on Linux, a kqueue event on macOS, the process
/// handle on Windows), so nothing polls. Where it gives none, or refused one,
/// the thread asks [`process_exists`](crate::process_exists) every 250 ms. A
/// `host` that has ended already runs `on_gone` at once.
///
/// # Errors
///
/// The thread could not be started.
pub fn watch_host(host: u32, on_gone: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    // Detached: the watch lives as long as the process, or until the host ends.
    tutti_platform::spawn_named("host-watch", None, move |_| {
        if !Imp::of_pid(host).is_some_and(|imp| imp.wait_ended()) {
            while crate::process_exists(host) {
                std::thread::sleep(NAP);
            }
        }
        on_gone();
    })
    .map(tutti_platform::ThreadHandle::detach)
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
/// `on_gone` on a thread of its own when it ends, or at once if it already has.
/// Returns `false`, watching nothing, when the host is unknown.
///
/// # Errors
///
/// The thread could not be started.
pub fn watch_spawner(on_gone: impl FnOnce() + Send + 'static) -> std::io::Result<bool> {
    match host_pid() {
        Host::Running(host) => watch_host(host, on_gone).map(|()| true),
        Host::Gone => {
            tutti_platform::spawn_named("host-watch", None, move |_| on_gone()).map(|handle| {
                handle.detach();
                true
            })
        }
        Host::Unknown => Ok(false),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc;

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
        assert!(watch.wait_ended(), "its end wakes the watch");
        child.wait().expect("reaped");
    }

    /// **The end of the host runs the function.** The child is reaped, so the
    /// function runs whether the thread registered its watch before the end or
    /// found the process gone.
    #[test]
    fn the_end_of_the_host_runs_the_function() {
        let mut child = Command::new("sleep").arg("600").spawn().expect("sleep");
        let (gone, ended) = mpsc::channel();
        watch_host(child.id(), move || gone.send(()).expect("the test listens"))
            .expect("the watch starts");

        child.kill().expect("kill");
        child.wait().expect("reaped");
        ended.recv_timeout(HANG_GUARD).expect("its end is learned");
    }

    /// **A host that ended before the watch began runs the function too.**
    #[test]
    fn a_host_that_ended_before_the_watch_runs_the_function() {
        let mut child = Command::new("true").spawn().expect("true");
        let pid = child.id();
        child.wait().expect("reaped");
        let (gone, ended) = mpsc::channel();
        watch_host(pid, move || gone.send(()).expect("the test listens"))
            .expect("the watch starts");
        ended.recv_timeout(HANG_GUARD).expect("its end is learned");
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
