//! Linux: a pidfd, readable once the process has ended (reaped or not).

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags};
use std::os::fd::OwnedFd;
use std::process::Child;
use std::time::Duration;

use super::Waited;

#[derive(Debug)]
pub(super) struct Imp {
    pidfd: OwnedFd,
}

impl Imp {
    /// `None` when the kernel refused a pidfd (before 5.3, or a sandbox that
    /// filters the call).
    pub(super) fn of(child: &Child) -> Option<Self> {
        rustix::process::pidfd_open(Pid::from_child(child), PidfdFlags::empty())
            .ok()
            .map(|pidfd| Self { pidfd })
    }

    /// A watch on the process `pid` of any parent. `None` when it has ended
    /// already or the kernel refused a pidfd.
    #[cfg(not(loom))]
    pub(super) fn of_pid(pid: u32) -> Option<Self> {
        let pid = Pid::from_raw(i32::try_from(pid).ok()?)?;
        rustix::process::pidfd_open(pid, PidfdFlags::empty())
            .ok()
            .map(|pidfd| Self { pidfd })
    }

    /// Waits up to `timeout` for the process to end.
    pub(super) fn wait(&self, timeout: Duration) -> Waited {
        let timeout = Timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let mut polls = [PollFd::new(&self.pidfd, PollFlags::IN)];
        match rustix::event::poll(&mut polls, Some(&timeout)) {
            Ok(ready) if ready > 0 => Waited::Ended,
            // A signal ends the poll early: the process still runs.
            Ok(_) | Err(rustix::io::Errno::INTR) => Waited::Running,
            Err(_) => Waited::Failed,
        }
    }
}
