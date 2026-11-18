//! A child process that cannot outlive its owner.

use std::ops::{Deref, DerefMut};
use std::process::{Child, ExitStatus};

/// A child process that is killed and reaped when it drops.
///
/// std's [`Child`] has no `Drop`: a child dropped on an error path, or by a
/// panic between spawn and wait, keeps running, and once it exits it is a
/// zombie until this process ends. Every child the plugin crates spawn (the
/// plugin-server, the probe) is held as one of these from the moment it is
/// spawned, so no path out of the code that holds it leaves one behind.
///
/// It derefs to the [`Child`], so `id`, `kill` and `wait` are std's own. A
/// child that already exited (or was reaped through the deref) is dropped
/// harmlessly: `kill` answers `Ok` and `wait` its status at once. Reap through
/// [`try_reap`](Self::try_reap), not the deref's `try_wait`, when the child
/// leads a [group](Self::leading_group).
#[derive(Debug)]
pub struct Reaped {
    child: Child,
    /// The child leads a process group of its own, which its end takes with it.
    group: bool,
}

impl Reaped {
    /// Own `child` from now on.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self {
            child,
            group: false,
        }
    }

    /// Own `child`, which leads a process group of its own
    /// ([`own_group`](tutti_plugin_ipc_ffi::own_group)): its end, by the kill
    /// of a drop or by its reaping, ends the group's other processes too (the
    /// helpers a plugin spawned).
    #[must_use]
    pub fn leading_group(child: Child) -> Self {
        Self { child, group: true }
    }

    /// The child's status if it has ended, reaping it, and ending its group
    /// with it: the id of a leader reaped and then left would name another
    /// process later, so the group goes at the reaping.
    ///
    /// # Errors
    ///
    /// The OS error of `try_wait`.
    pub fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
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

    /// Ends the group while its leader's id is still the leader's: before the
    /// reaping, or at it. A `false` `group` has nothing to end.
    fn end_group(&mut self) {
        if std::mem::take(&mut self.group) {
            tutti_plugin_ipc_ffi::kill_group(self.child.id());
        }
    }
}

impl Deref for Reaped {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
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
    /// zombie still answers `kill -0`). Moved here with the guard from
    /// `tutti-plugin`'s probe, which had its own copy.
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
}
