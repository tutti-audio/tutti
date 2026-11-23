//! [`RenderCount`]: how many renders of one thing are in flight, so a thread
//! that has cleared what a render reads can tell when none still reads it.
//!
//! # The protocol
//!
//! - **Render side:** [`enter`](RenderCount::enter) is a `Relaxed` `fetch_add`
//!   and a `SeqCst` fence, and the guard's drop a `Release` `fetch_sub`. The
//!   render reads what it reads after `enter` returns. Wait-free; never
//!   allocates or locks.
//! - **Clearing side:** writes what the renders read (a callback slot), then
//!   calls [`quiescent`](RenderCount::quiescent): a `SeqCst` fence and an
//!   `Acquire` load.
//!
//! The two sides are a store-load pair each (the render's count then its read,
//! the clearer's write then the count), so each takes a `SeqCst` fence, not a
//! `SeqCst` access. **A `quiescent` that returns `true` orders its caller's
//! writes before every render that enters later**: such a render's fence follows
//! the clearer's in the fences' single order, so what the render reads is
//! what the clearer wrote. A render that entered earlier is counted, so
//! `quiescent` returns `false` until its guard drops; the `Release` of that
//! drop and the `Acquire` load order what the render did before what the
//! clearer does after seeing zero.
//!
//! A guard that is leaked (`mem::forget`) keeps the count above zero for good,
//! so `quiescent` never returns `true` again: the clearer then leaves what it
//! would have freed allocated, which is the safe direction. The count is a
//! `usize`, and it cannot wrap in practice: it would take more than `usize::MAX`
//! guards held at once.
//!
//! The model in `tests/render_count_loom.rs` runs this code.

use crate::sync::{AtomicUsize, Ordering, fence};

/// The renders of one thing in flight: any number of threads enter, one clearing
/// side asks whether none is.
#[derive(Debug, Default)]
pub struct RenderCount {
    in_flight: AtomicUsize,
}

/// A render counted in flight until it drops ([`RenderCount::enter`]).
#[derive(Debug)]
#[must_use = "the render is counted in flight only while the guard is held"]
pub struct RenderGuard<'a>(&'a RenderCount);

impl RenderCount {
    /// A count with no render in flight.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Counts a render in flight until the guard drops (a guard that is leaked
    /// counts forever: [`quiescent`](Self::quiescent) then stays `false`). Call
    /// it before the render reads anything a [`quiescent`](Self::quiescent)
    /// caller clears.
    /// Two atomic operations and a fence: never allocates, locks or waits.
    pub fn enter(&self) -> RenderGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        RenderGuard(self)
    }

    /// Whether no render is in flight, as of a fence after the caller's last
    /// write to what renders read: when `true`, every render that enters
    /// later reads those writes, and every render that has dropped its guard
    /// has finished with them.
    #[must_use]
    pub fn quiescent(&self) -> bool {
        fence(Ordering::SeqCst);
        self.in_flight.load(Ordering::Acquire) == 0
    }
}

impl Drop for RenderGuard<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A render in flight is not quiescent, and its end is**: nested guards
    /// each count, and the count reads quiet only after the last drops.
    #[test]
    fn quiescence_is_the_absence_of_every_guard() {
        let count = RenderCount::new();
        assert!(count.quiescent(), "nothing entered");
        let outer = count.enter();
        let inner = count.enter();
        assert!(!count.quiescent());
        drop(inner);
        assert!(!count.quiescent(), "one render still in flight");
        drop(outer);
        assert!(count.quiescent());
    }
}
