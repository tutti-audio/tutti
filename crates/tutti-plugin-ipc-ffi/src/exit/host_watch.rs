//! [`watch_host`]: a thread that runs a function when a named process ends.

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
    std::thread::Builder::new()
        .name("host-watch".into())
        .spawn(move || {
            if !Imp::of_pid(host).is_some_and(|imp| imp.wait_ended()) {
                while crate::process_exists(host) {
                    std::thread::sleep(NAP);
                }
            }
            on_gone();
        })
        .map(drop)
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
}
