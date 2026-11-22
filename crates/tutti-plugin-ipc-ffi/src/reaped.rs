//! A child process that cannot outlive its owner.

use crate::{ExitWait, Process, has_ended, kill_group};
use std::process::{Child, ExitStatus};

/// A child process that is killed and reaped when it drops.
///
/// std's [`Child`] has no `Drop`: a child dropped on an error path, or by a
/// panic between spawn and wait, keeps running, and once it exits it is a
/// zombie until this process ends. Every child the plugin crates spawn (the
/// plugin-server, the probe) is held as one of these from the moment it is
/// spawned, so no path out of the code that holds it leaves one behind.
///
/// It does not deref to the [`Child`]: a `kill` or a `try_wait` through it would
/// skip the child's [process group](Self::leading_group). Its calls are
/// [`id`](Self::id), [`try_reap`](Self::try_reap), [`kill_all`](Self::kill_all),
/// [`wait`](Self::wait) and [`exit_wait`](Self::exit_wait); a child that already
/// exited is dropped harmlessly.
#[derive(Debug)]
pub struct Reaped {
    child: Process,
    /// The child leads a process group of its own, which its end takes with it.
    group: bool,
}

impl Reaped {
    /// Own `child` from now on.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self {
            child: child.into(),
            group: false,
        }
    }

    /// Own `child`, which leads a process group of its own
    /// ([`Spawn::Leader`](crate::Spawn::Leader)): its end, by the kill
    /// of a drop or by its reaping, ends the group's other processes too (the
    /// helpers a plugin spawned).
    #[must_use]
    pub fn leading_group(child: Child) -> Self {
        Self {
            child: child.into(),
            group: true,
        }
    }

    /// The child's process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// A wait on the OS's wake for the child's end, made while it is unreaped.
    #[must_use]
    pub fn exit_wait(&self) -> ExitWait {
        ExitWait::of(&self.child)
    }

    /// The child's status if it has ended, reaping it. Its group is ended
    /// first, while the unreaped leader still holds the group's id: a reaped
    /// leader's id may name another process later.
    ///
    /// # Errors
    ///
    /// The OS error of `try_wait`.
    pub fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.group && has_ended(self.child.id()) {
            self.end_group();
        }
        let status = self.child.try_wait()?;
        if status.is_some() {
            // Ended between the look and the reaping (or a platform that cannot
            // look): the id is as good as it was an instant ago.
            self.end_group();
        }
        Ok(status)
    }

    /// Kills the child and its group, without waiting for it.
    ///
    /// # Errors
    ///
    /// The OS error of `kill`.
    pub fn kill_all(&mut self) -> std::io::Result<()> {
        self.end_group();
        self.child.kill()
    }

    /// Ends the child's group, then waits for the child: unbounded, so for a
    /// child already killed.
    ///
    /// # Errors
    ///
    /// The OS error of `wait`.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.end_group();
        self.child.wait()
    }

    /// Gives the child up: it is killed with its group, and neither waited for
    /// nor reaped, so this never blocks. It stays a zombie until this process
    /// ends. For a child whose reaping could not be handed to anything that
    /// may wait for it; [`Drop`] would wait.
    pub fn leave(mut self) {
        let _ = self.kill_all();
        // Dropping would wait; a `Child` holds no resource that a forget leaks
        // beyond the zombie.
        let _left = std::mem::ManuallyDrop::new(self);
    }

    /// Ends the group while its leader's id is still the leader's: before the
    /// reaping, or at it. A `false` `group` has nothing to end.
    fn end_group(&mut self) {
        if std::mem::take(&mut self.group) {
            kill_group(self.child.id());
        }
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        // No `wait` when the kill failed: a process this one cannot signal
        // could block the reap forever, and a leaked process is the lesser harm.
        if self.kill_all().is_ok() {
            let _ = self.child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// **A child is killed and reaped when its guard unwinds**: a panic
    /// between spawn and wait leaves no running process and no zombie (a
    /// zombie still answers `kill -0`).
    #[test]
    fn a_child_is_killed_and_reaped_when_its_guard_unwinds() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = Reaped::new(child);
            panic!("between spawn and wait");
        }));
        assert!(unwound.is_err());
        assert!(!exists(pid), "child {pid} survived its guard");
    }

    /// **A child that is left is killed and not reaped**: the call returns
    /// without waiting, and the ended child is still a zombie (a zombie answers
    /// `kill -0`; a reaped child does not).
    #[test]
    fn a_left_child_is_killed_and_not_reaped() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let reaped = Reaped::new(child);
        let wait = reaped.exit_wait();
        reaped.leave();
        // The first wait only says it may have ended; the second returns when
        // the OS signals the kill's end.
        for _ in 0..2 {
            assert!(wait.wait(std::time::Duration::from_secs(600), |_| ()));
        }
        assert!(exists(pid), "a left child is not reaped");
    }
}
