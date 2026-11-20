//! [`ExitWait`]: a wait for a child process to end that the OS wakes, so the
//! host learns of a server's death when it happens and not at the next poll.
//!
//! One platform module is chosen here and nowhere else: a pidfd on Linux, a
//! kqueue process-exit event on macOS, the process handle on Windows, and a short nap
//! everywhere else.

#[cfg(target_os = "linux")]
#[path = "exit/pidfd.rs"]
mod platform;
#[cfg(windows)]
#[path = "exit/handle.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "exit/kqueue.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    /// A platform with no wake for a child's end: no value exists, and
    /// [`of`](Self::of) is always `None`.
    #[derive(Debug)]
    pub(super) enum Imp {}

    impl Imp {
        pub(super) fn of(_child: &std::process::Child) -> Option<Self> {
            None
        }

        pub(super) fn of_pid(_pid: u32) -> Option<Self> {
            None
        }

        pub(super) fn wait(&self, _timeout: std::time::Duration) -> super::Waited {
            match *self {}
        }
    }
}

mod host_watch;

/// What one OS wait on a process found. A failure to ask is its own case: it
/// says nothing about the process, so it is never read as an end.
#[derive(Debug)]
pub(super) enum Waited {
    /// The OS signalled the process's end.
    Ended,
    /// The wait ran out, or a signal cut it short, with the process running.
    Running,
    /// The OS could not be asked; nothing the caller can act on says why.
    Failed,
}

pub use host_watch::{HOST_PID_ENV, Host, host_pid, watch_host, watch_spawner};

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The slice a platform with no wake pauses between asks.
const NAP: Duration = Duration::from_millis(10);

/// A wait for one child process to end.
///
/// It answers "may have ended" rather than "has ended with this status": the
/// owner of the [`Child`] asks `try_wait` for the status, and this only says
/// when to ask. Where the OS has no such wake, [`wait`](Self::wait) hands a
/// short slice to the caller's pause and says "may have ended" at its end, so
/// the owner's `try_wait` is a poll there.
#[derive(Debug)]
pub struct ExitWait {
    /// `None` when the OS gave no wake for this child: the wait then pauses.
    inner: Option<platform::Imp>,
    /// Until the first [`wait`](Self::wait) has answered. A child may have
    /// ended before the OS was asked to watch it (a kqueue registration on a
    /// zombie succeeds and never fires), so the first wait says "may have
    /// ended" without asking, and the owner's `try_wait` reads the status.
    unasked: AtomicBool,
}

impl ExitWait {
    /// A wait on `child`, made while it is still unreaped (a reaped child's
    /// process id may name another process).
    #[must_use]
    pub fn of(child: &Child) -> Self {
        Self {
            inner: platform::Imp::of(child),
            unasked: AtomicBool::new(true),
        }
    }

    /// Waits up to `timeout` for the child to end. `true` when it may have
    /// ended (the OS signalled it, or this platform wakes only by pausing),
    /// `false` when the whole `timeout` passed with the OS saying it still
    /// runs.
    ///
    /// With no OS wake, the wait is `pause` of `timeout` or a short slice,
    /// whichever is less, and then `true`: the caller says how it pauses (a stop
    /// token's wait, so that a stop ends it) and never one that ignores the end
    /// of the wait. `pause` is not called when the OS wakes.
    #[must_use]
    pub fn wait(&self, timeout: Duration, pause: impl FnOnce(Duration)) -> bool {
        if self.unasked.swap(false, Ordering::AcqRel) {
            return true;
        }
        let Some(inner) = &self.inner else {
            pause(timeout.min(NAP));
            return true;
        };
        // A failure to ask is "may have ended": the owner's `try_wait` is the
        // authority and reads the status.
        !matches!(inner.wait(timeout), Waited::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tutti_platform::Instant;

    /// A command that ends at once, on every platform.
    fn quick() -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit"]);
            command
        } else {
            Command::new("true")
        }
    }

    /// **A child that ended before it was watched is still learned of**: the
    /// wait is made after the exit, so an OS wake for it may never come, and the
    /// owner reads the status within a bound all the same.
    #[test]
    fn a_child_that_ended_before_the_wait_was_made_is_learned_of() {
        let mut child = quick()
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the command spawns");
        // The child closes its stdout as it ends: the read returns once the
        // child is gone, and the wait below is made after.
        std::io::copy(
            &mut child.stdout.take().expect("piped"),
            &mut std::io::sink(),
        )
        .expect("the pipe reads to its end");
        let wait = ExitWait::of(&child);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if wait.wait(Duration::from_millis(100), std::thread::sleep)
                && let Some(status) = child.try_wait().expect("the status reads")
            {
                assert!(status.success(), "{status:?}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the end of the child was never learned of"
            );
        }
    }

    /// **A running child's wait ends "running" at its timeout, and its end
    /// wakes the wait** (the first wait only says "may have ended", so it is
    /// spent first). The woken wait's timeout outlasts the test harness's limit,
    /// so a wait that ran it out fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_end_of_a_child_wakes_the_wait() {
        let mut child = Command::new("sleep").arg("30").spawn().expect("sleep");
        let wait = ExitWait::of(&child);
        assert!(
            wait.wait(Duration::ZERO, std::thread::sleep),
            "the first wait asks nothing"
        );
        assert!(
            !wait.wait(Duration::from_millis(50), std::thread::sleep),
            "a running child"
        );

        child.kill().expect("kill");
        assert!(
            wait.wait(Duration::from_secs(600), std::thread::sleep),
            "its end wakes the wait"
        );
        child.wait().expect("reaped");
    }

    /// **Without an OS wake the wait pauses and says "may have ended", until the
    /// child ends.** The wait has no OS side, and the injected pause is where the
    /// child ends, so a wait that did not hand its slice to the pause, or that
    /// did not end with the child, fails.
    #[cfg(unix)]
    #[test]
    fn the_wait_without_a_wake_pauses_until_the_child_ends() {
        let mut child = Command::new("sleep").arg("600").spawn().expect("sleep");
        let wait = ExitWait {
            inner: None,
            unasked: AtomicBool::new(false),
        };
        let mut pauses = Vec::new();
        let mut status = None;
        while status.is_none() {
            assert!(wait.wait(Duration::from_secs(600), |slice| {
                pauses.push(slice);
                child.kill().expect("kill");
                // The wake is the pause's end: the child is gone by then.
                child.wait().expect("reaped");
            }));
            status = child.try_wait().expect("status");
        }
        child.wait().expect("reaped");
        assert_eq!(pauses, [NAP], "one slice, capped at the nap");
    }
}
