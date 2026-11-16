//! [`ExitWait`]: a wait for a child process to end that the OS wakes, so the
//! host learns of a server's death when it happens and not at the next poll.
//!
//! One platform module is chosen here and nowhere else: a pidfd on Linux, a
//! kqueue `NOTE_EXIT` on macOS, the process handle on Windows, and a short nap
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
#[path = "exit/polled.rs"]
mod platform;

use std::process::Child;
use std::time::Duration;

/// The slice a platform with no wake sleeps between asks.
const NAP: Duration = Duration::from_millis(10);

/// A wait for one child process to end.
///
/// It answers "may have ended" rather than "has ended with this status": the
/// owner of the [`Child`] asks `try_wait` for the status, and this only says
/// when to ask. Where the OS has no such wake, [`wait`](Self::wait) sleeps a
/// short slice and says "may have ended" at its end, so the owner's `try_wait`
/// is a poll there.
#[derive(Debug)]
pub struct ExitWait {
    inner: platform::Imp,
}

impl ExitWait {
    /// A wait on `child`, made while it is still unreaped (a reaped child's
    /// process id may name another process).
    #[must_use]
    pub fn of(child: &Child) -> Self {
        Self {
            inner: platform::Imp::of(child),
        }
    }

    /// Waits up to `timeout` for the child to end. `true` when it may have
    /// ended (the OS signalled it, or this platform wakes only by sleeping),
    /// `false` when the whole `timeout` passed with the OS saying it still
    /// runs.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> bool {
        self.inner.wait(timeout)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    /// **A running child is waited out in full, and its end wakes the wait
    /// long before the timeout.** On a platform with no wake the wait only
    /// sleeps, so the test names the platforms that have one.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_end_of_a_child_wakes_the_wait() {
        let mut child = Command::new("sleep").arg("30").spawn().expect("sleep");
        let wait = ExitWait::of(&child);
        let start = Instant::now();
        assert!(!wait.wait(Duration::from_millis(50)), "a running child");
        assert!(start.elapsed() >= Duration::from_millis(50));

        child.kill().expect("kill");
        let start = Instant::now();
        assert!(wait.wait(Duration::from_secs(10)), "its end wakes the wait");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "woken by the end, not the timeout: {:?}",
            start.elapsed()
        );
        child.wait().expect("reaped");
    }
}
