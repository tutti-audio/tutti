//! macOS: a kqueue watching the process for the process-exit event.

use crate::Process;
use rustix::event::Timespec;
use rustix::event::kqueue::{
    Event, EventFilter, EventFlags, ProcessEvents, kevent_timespec, kqueue,
};
use rustix::fd::OwnedFd;
use rustix::process::Pid;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::Waited;

#[derive(Debug)]
pub(super) struct Imp {
    /// The queue the exit event arrives on.
    queue: OwnedFd,
    /// The event was taken. It is delivered once, so later waits answer from
    /// here: the status may become readable a moment after the event.
    seen: AtomicBool,
}

impl Imp {
    /// `None` when the queue could not be made or the registration was refused
    /// (the process had ended already, say).
    pub(super) fn of(process: &Process) -> Option<Self> {
        Self::watching(Pid::from_child(process.as_child()))
    }

    /// A watch on the process `pid` of any parent. `None` when it has ended
    /// already or the queue was refused.
    #[cfg(not(loom))]
    pub(super) fn of_pid(pid: u32) -> Option<Self> {
        Self::watching(Pid::from_raw(i32::try_from(pid).ok()?)?)
    }

    fn watching(pid: Pid) -> Option<Self> {
        let queue = kqueue().ok().filter(|queue| {
            let change = Event::new(
                EventFilter::Proc {
                    pid,
                    flags: ProcessEvents::EXIT,
                },
                EventFlags::ADD | EventFlags::ONESHOT,
                std::ptr::null_mut(),
            );
            let mut none: [MaybeUninit<Event>; 0] = [];
            let now = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: the one event names a process id, not a file descriptor,
            // so nothing it refers to can be invalid or outlive the queue; the
            // event list is empty and the zero timeout never blocks.
            unsafe { kevent_timespec(queue, &[change], &mut none[..], Some(&now)) }.is_ok()
        });
        queue.map(|queue| Self {
            queue,
            seen: AtomicBool::new(false),
        })
    }

    /// Waits up to `timeout` for the process to end.
    pub(super) fn wait(&self, timeout: Duration) -> Waited {
        if self.seen.load(Ordering::Acquire) {
            return Waited::Ended;
        }
        let timeout = Timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let mut taken = [MaybeUninit::<Event>::uninit(); 1];
        // SAFETY: no change is submitted, so no event refers to a descriptor;
        // the event list is a live buffer of one slot.
        let ready = unsafe { kevent_timespec(&self.queue, &[], &mut taken[..], Some(&timeout)) };
        match ready {
            Ok(([], _)) => Waited::Running,
            Ok(_) => {
                self.seen.store(true, Ordering::Release);
                Waited::Ended
            }
            // A signal ends the wait early: the process still runs.
            Err(rustix::io::Errno::INTR) => Waited::Running,
            Err(_) => Waited::Failed,
        }
    }
}
