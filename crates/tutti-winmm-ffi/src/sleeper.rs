//! One wait that ends at a time or when another thread says so.

use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects,
};

use crate::error::{Error, OsOperation, last_os};

/// Why [`TimerWaiter::wait_for`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Woken {
    /// The time passed.
    Elapsed,
    /// A [`TimerWaker`] was woken.
    Signalled,
}

/// The kernel objects both halves share.
#[derive(Debug)]
struct Handles {
    timer: HANDLE,
    event: HANDLE,
}

// SAFETY: both handles are kernel objects, valid from any thread for their
// whole life, and nothing here is tied to the thread that made them.
unsafe impl Send for Handles {}
// SAFETY: `event` is only passed to `SetEvent`, which any thread may call at any
// time, and to a wait. `timer` is armed and waited on only through
// `TimerWaiter`'s `&mut self` methods, and there is one `TimerWaiter`, so no two
// threads touch it at once.
unsafe impl Sync for Handles {}

impl Drop for Handles {
    fn drop(&mut self) {
        // SAFETY: both are open handles, each closed once, when the last half
        // is gone.
        unsafe {
            CloseHandle(self.timer);
            CloseHandle(self.event);
        }
    }
}

/// The waking half of a wake signal and a waitable timer: any thread, through
/// any clone, ends the [`TimerWaiter`]'s current or next wait.
#[derive(Clone, Debug)]
pub struct TimerWaker {
    handles: Arc<Handles>,
}

/// The waiting half: a waitable timer, high resolution where Windows has it,
/// and the wake signal. There is one, and its waits take `&mut self`, so two
/// threads cannot wait on the timer at once.
#[derive(Debug)]
pub struct TimerWaiter {
    handles: Arc<Handles>,
}

/// A [`TimerWaker`] and the one [`TimerWaiter`] it ends, with nothing signalled.
///
/// # Errors
///
/// The failing call, with `GetLastError`'s error.
pub fn waitable_timer() -> Result<(TimerWaker, TimerWaiter), Error> {
    // SAFETY: null security attributes and name; an auto-reset event that
    // starts unsignalled.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(last_os(OsOperation::CreateEvent));
    }
    // High resolution needs Windows 10 1803; the call refuses the flag
    // before that, and the plain timer is the fallback.
    // SAFETY: null attributes and name; the flag and access are constants.
    let mut timer = unsafe {
        CreateWaitableTimerExW(
            std::ptr::null(),
            std::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer.is_null() {
        // SAFETY: as above, without the flag.
        timer = unsafe {
            CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, TIMER_ALL_ACCESS)
        };
    }
    if timer.is_null() {
        let error = last_os(OsOperation::CreateWaitableTimer);
        // SAFETY: `event` is the open event made above, closed once.
        unsafe { CloseHandle(event) };
        return Err(error);
    }
    let handles = Arc::new(Handles { timer, event });
    Ok((
        TimerWaker {
            handles: Arc::clone(&handles),
        },
        TimerWaiter { handles },
    ))
}

impl TimerWaker {
    /// Ends the [`TimerWaiter`]'s current or next wait. A wake made while
    /// nobody waits ends the next wait at once.
    ///
    /// # Errors
    ///
    /// `SetEvent`'s, with `GetLastError`'s error.
    pub fn wake(&self) -> Result<(), Error> {
        // SAFETY: `event` is the open event.
        if unsafe { SetEvent(self.handles.event) } == 0 {
            return Err(last_os(OsOperation::SetEvent));
        }
        Ok(())
    }
}

impl TimerWaiter {
    /// Returns when a [`TimerWaker`] has woken it. A wake made while nobody
    /// waits ends this wait at once.
    ///
    /// # Errors
    ///
    /// The failing call, with `GetLastError`'s error.
    pub fn wait(&mut self) -> Result<(), Error> {
        self.block(1).map(|_| ())
    }

    /// Returns when `timeout` has passed or a [`TimerWaker`] has woken it.
    ///
    /// # Errors
    ///
    /// The failing call, with `GetLastError`'s error.
    pub fn wait_for(&mut self, timeout: Duration) -> Result<Woken, Error> {
        // A negative due time is relative, in 100 ns units; at least one unit,
        // as zero would be absolute time.
        let units = i64::try_from(timeout.as_nanos() / 100).unwrap_or(i64::MAX);
        let due = -units.max(1);
        // SAFETY: `timer` is the open timer and `due` a live i64; no period
        // and no completion routine.
        let armed = unsafe {
            SetWaitableTimer(
                self.handles.timer,
                &raw const due,
                0,
                None,
                std::ptr::null(),
                0,
            )
        };
        if armed == 0 {
            return Err(last_os(OsOperation::SetWaitableTimer));
        }
        Ok(match self.block(2)? {
            0 => Woken::Signalled,
            _ => Woken::Elapsed,
        })
    }

    /// Waits on the first `count` of the event and the timer; the index of the
    /// one that ended the wait. `block(1)` waits on the event alone, so an
    /// armed or fired timer never ends it.
    fn block(&mut self, count: u32) -> Result<u32, Error> {
        let handles = [self.handles.event, self.handles.timer];
        // SAFETY: `handles` holds open handles, `count` of them are read, and
        // `self.handles` keeps them open for the call.
        let result = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, INFINITE) };
        match result.wrapping_sub(WAIT_OBJECT_0) {
            index if index < count => Ok(index),
            _ => Err(last_os(OsOperation::WaitForMultipleObjects)),
        }
    }
}
