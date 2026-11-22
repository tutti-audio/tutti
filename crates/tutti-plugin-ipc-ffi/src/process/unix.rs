//! Unix: the process is std's `Child`.

use std::process::{Child, ExitStatus};

/// A process this one started, held by [`Reaped`](crate::Reaped) and nowhere
/// else: its id, a non-blocking status, a kill, a blocking wait, and its handle
/// released when it drops.
#[derive(Debug)]
pub struct Process(Child);

impl From<Child> for Process {
    fn from(child: Child) -> Self {
        Self(child)
    }
}

impl Process {
    /// The process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// The status if the process has ended, reaping it.
    ///
    /// # Errors
    ///
    /// The OS error of the look.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }

    /// Kills the process; one that has ended already is `Ok`.
    ///
    /// # Errors
    ///
    /// The OS refused the kill.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    /// Waits for the process to end, unbounded.
    ///
    /// # Errors
    ///
    /// The OS error of the wait.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.0.wait()
    }

    /// The process as the OS wakes for its end ([`ExitWait`](crate::ExitWait)).
    pub(crate) fn as_child(&self) -> &Child {
        &self.0
    }
}
