//! Linux: a pidfd, readable once the process has ended (reaped or not).

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags};
use std::os::fd::OwnedFd;
use std::process::Child;
use std::time::Duration;

#[derive(Debug)]
pub(super) struct Imp {
    /// `None` when the kernel refused a pidfd (before 5.3, or a sandbox that
    /// filters the call): the wait then only sleeps.
    pidfd: Option<OwnedFd>,
}

impl Imp {
    pub(super) fn of(child: &Child) -> Self {
        let pidfd = rustix::process::pidfd_open(Pid::from_child(child), PidfdFlags::empty()).ok();
        Self { pidfd }
    }

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let Some(pidfd) = &self.pidfd else {
            std::thread::sleep(timeout.min(super::NAP));
            return true;
        };
        let timeout = Timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let mut polls = [PollFd::new(pidfd, PollFlags::IN)];
        // A signal that interrupts the poll, or any failure to ask, reads as
        // "may have ended": the owner's `try_wait` is the authority.
        rustix::event::poll(&mut polls, Some(&timeout)).map_or(true, |ready| ready > 0)
    }
}
