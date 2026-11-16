//! macOS: a kqueue watching the process for `NOTE_EXIT`.

use rustix::event::Timespec;
use rustix::event::kqueue::{
    Event, EventFilter, EventFlags, ProcessEvents, kevent_timespec, kqueue,
};
use rustix::fd::OwnedFd;
use rustix::process::Pid;
use std::mem::MaybeUninit;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug)]
pub(super) struct Imp {
    /// The queue the exit event arrives on; `None` when it could not be made or
    /// the registration was refused (the process had ended already, say): the
    /// wait then only sleeps, and the owner's `try_wait` reads the status.
    queue: Option<OwnedFd>,
    /// The event was taken. It is delivered once, so later waits answer from
    /// here: the status may become readable a moment after the event.
    seen: AtomicBool,
}

impl Imp {
    pub(super) fn of(child: &Child) -> Self {
        let queue = kqueue().ok().filter(|queue| {
            let change = Event::new(
                EventFilter::Proc {
                    pid: Pid::from_child(child),
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
        Self {
            queue,
            seen: AtomicBool::new(false),
        }
    }

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let Some(queue) = &self.queue else {
            std::thread::sleep(timeout.min(super::NAP));
            return true;
        };
        if self.seen.load(Ordering::Acquire) {
            return true;
        }
        let timeout = Timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let mut taken = [MaybeUninit::<Event>::uninit(); 1];
        // SAFETY: no change is submitted, so no event refers to a descriptor;
        // the event list is a live buffer of one slot.
        let ready = unsafe { kevent_timespec(queue, &[], &mut taken[..], Some(&timeout)) };
        // A failure to ask is "may have ended": the owner's `try_wait` decides.
        let ended = ready.map_or(true, |(events, _)| !events.is_empty());
        if ended {
            self.seen.store(true, Ordering::Release);
        }
        ended
    }
}
