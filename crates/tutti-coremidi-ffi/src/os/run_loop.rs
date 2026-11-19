//! The run loop that delivers the client's notifications.
//!
//! CoreMIDI sends a client's notifications to the run loop of the thread that
//! created the process's first client, and only while that thread runs the loop
//! ([`SharedClient::start`]). [`run_notifications`] is that run, with a timer on the
//! same loop: a loop with no source returns at once, and a timer is a source, so the
//! thread waits in the loop whether or not CoreMIDI delivered it a notification.

use super::client::SharedClient;
use core_foundation::date::CFDate;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerRef, kCFRunLoopDefaultMode,
};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::time::Duration;

/// What the timer calls.
type Tick = Box<dyn FnMut()>;

/// Runs the calling thread's run loop, in which `client`'s notifications arrive,
/// and calls `on_tick` on this thread every `period` while it runs. It returns
/// only when the loop is stopped, which nothing here does, so a thread that calls
/// it serves notifications for its life.
///
/// `client` is the one [`SharedClient::start`] returned, which must have been
/// started on this thread: notifications reach the loop of the thread that created
/// the process's first client. A panic in `on_tick` is caught and the tick is lost.
pub fn run_notifications<F>(_client: SharedClient, period: Duration, on_tick: F)
where
    F: FnMut() + 'static,
{
    // Held until the loop returns, which is as long as the timer exists.
    let mut tick: Tick = Box::new(on_tick);
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: ptr::from_mut(&mut tick).cast::<c_void>(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let seconds = period.as_secs_f64();
    let timer = CFRunLoopTimer::new(
        CFDate::now().abs_time() + seconds,
        seconds,
        0,
        0,
        fire,
        &mut context,
    );
    // SAFETY: `kCFRunLoopDefaultMode` is a constant CFString CoreMIDI's run loop
    // reads notifications in.
    CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopDefaultMode });
    CFRunLoop::run_current();
}

/// The timer's callback: calls the closure `run_notifications` made the timer's
/// info.
extern "C" fn fire(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    // SAFETY: `info` is the address of `run_notifications`'s `tick`, which lives
    // until the loop returns, and the loop calls this on that thread one call at a
    // time, so nothing else reads it.
    let tick = unsafe { &mut *info.cast::<Tick>() };
    // A panic must not unwind into CoreFoundation's frames.
    let _ = catch_unwind(AssertUnwindSafe(tick));
}
