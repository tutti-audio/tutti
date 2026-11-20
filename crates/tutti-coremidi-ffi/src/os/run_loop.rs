//! The run loop that delivers the client's notifications.
//!
//! CoreMIDI sends a client's notifications to the run loop of the thread that
//! created the process's first client, and only while that thread runs the loop
//! ([`SharedClient::start`]). [`run_notifications`] is that run. A loop with no
//! source returns at once, which is what a process whose first client was not this
//! thread's has; there a timer on the loop keeps the thread in it, and its ticks are
//! the thread's chance to look for what no notification told it.

use super::client::SharedClient;
use super::receive::drop_guarded;
use core_foundation::base::TCFType;
use core_foundation::date::CFDate;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopRunResult, CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerInvalidate,
    CFRunLoopTimerRef, kCFRunLoopDefaultMode,
};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::time::Duration;

/// What the timer calls.
type Tick = Box<dyn FnMut()>;

/// Invalidates its timer on drop, so the run loop releases it and never calls its
/// callback again, on a normal return and on an unwind alike.
struct Armed(CFRunLoopTimer);

impl Drop for Armed {
    fn drop(&mut self) {
        // SAFETY: the timer is a live `CFRunLoopTimer` this guard owns.
        unsafe { CFRunLoopTimerInvalidate(self.0.as_concrete_TypeRef()) };
    }
}

/// Runs the calling thread's run loop, in which `client`'s notifications arrive.
/// It returns when the loop is stopped, which nothing here does, so a thread that
/// calls it serves notifications for its life.
///
/// `client` is the one [`SharedClient::start`] returned, which must have been
/// started on this thread: notifications reach the loop of the thread that created
/// the process's first client. When the loop has a source (the client's), the
/// thread simply runs it and `on_tick` is never called. When it has none, a timer
/// calls `on_tick` on this thread every `period` while the loop runs. A panic in
/// `on_tick` is caught and the tick is lost.
pub fn run_notifications<F>(_client: SharedClient, period: Duration, on_tick: F)
where
    F: FnMut() + 'static,
{
    // A pass of the loop with a zero timeout finishes at once when the mode has no
    // source or timer, and times out when it has one.
    // SAFETY: `kCFRunLoopDefaultMode` is a constant CFString.
    let mode = unsafe { kCFRunLoopDefaultMode };
    if CFRunLoop::run_in_mode(mode, Duration::ZERO, false) != CFRunLoopRunResult::Finished {
        CFRunLoop::run_current();
        return;
    }
    // Declared before the timer's guard, so it outlives the timer.
    let mut tick: Tick = Box::new(on_tick);
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: ptr::from_mut(&mut tick).cast::<c_void>(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let seconds = period.as_secs_f64();
    let timer = Armed(CFRunLoopTimer::new(
        CFDate::now().abs_time() + seconds,
        seconds,
        0,
        0,
        fire,
        &mut context,
    ));
    CFRunLoop::get_current().add_timer(&timer.0, mode);
    CFRunLoop::run_current();
}

/// The timer's callback: calls the closure `run_notifications` made the timer's
/// info.
extern "C" fn fire(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    // SAFETY: `info` is the address of `run_notifications`'s `tick`. The timer's
    // guard is dropped before `tick`, and invalidating the timer ends its calls,
    // so this runs only while `tick` is alive; it runs on that thread, one call at
    // a time, so nothing else reads it.
    let tick = unsafe { &mut *info.cast::<Tick>() };
    // A panic must not unwind into CoreFoundation's frames, and neither may the
    // drop of its payload.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(tick)) {
        drop_guarded(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::client::test_client;
    use std::sync::mpsc;

    /// On a thread whose loop has no source the timer ticks (here until the third
    /// tick stops the loop), `run_notifications` returns when the loop stops, and
    /// the timer is gone: another pass of the loop finds nothing to run and no tick
    /// follows.
    #[test]
    fn a_loop_with_no_source_ticks_and_leaves_no_timer() {
        // The client is made on this thread, so the thread below has no source.
        let client = test_client();
        let (ticks, heard) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut count = 0;
            let tx = ticks.clone();
            run_notifications(client, Duration::from_millis(5), move || {
                count += 1;
                let _ = tx.send(count);
                if count == 3 {
                    CFRunLoop::get_current().stop();
                }
            });
            // SAFETY: `kCFRunLoopDefaultMode` is a constant CFString.
            let mode = unsafe { kCFRunLoopDefaultMode };
            let after = CFRunLoop::run_in_mode(mode, Duration::from_millis(50), false);
            let _ = ticks.send(0);
            after
        });
        let after = thread.join().expect("the thread returns");
        assert_eq!(heard.try_iter().collect::<Vec<_>>(), [1, 2, 3, 0]);
        assert_eq!(after, CFRunLoopRunResult::Finished);
    }
}
