//! [`parker()`]: wake one parked thread from another, without the waker
//! ever waiting.
//!
//! A helper thread that has nothing to do parks; the audio thread, which
//! must not lock, allocate or wait, unparks it when work arrives. This is
//! the pair of fences and the park that a worker pool's helper carries, as
//! one primitive with a `loom` model (`tests/parker_loom.rs`) over the
//! shipped code.
//!
//! [`parker()`] makes the pair: an [`Unparker`], the waking side (`Clone`,
//! `Send + Sync`, held by every waker), and a [`Parker`], the waiting side.
//! There is one `Parker` per pair, by type: it is not `Clone`, and `park`
//! takes `&mut self`. It binds the thread of its first park and stays on
//! it: a park from another thread panics, since an unpark wakes one thread
//! only.
//!
//! # The protocol
//!
//! The waker **publishes** what the parker is waiting for (opens a gate,
//! pushes to a ring, sets a flag: any write), then calls
//! [`unpark`](Unparker::unpark). The parker calls [`park`](Parker::park)
//! with a closure that reads what the waker publishes:
//!
//! 1. `park` stores `parked = true`, issues a `SeqCst` fence, then runs the
//!    closure; only if it says nothing is pending does it park the thread;
//! 2. `unpark` issues a `SeqCst` fence, then reads `parked`; only if it is
//!    set does it clear it and unpark the thread.
//!
//! The two `SeqCst` fences are totally ordered (this is the store-load
//! Dekker pattern), so either the closure sees the waker's write and the
//! parker does not park, or `unpark` sees `parked` and unparks. An unpark
//! that lands before the park is not lost either: `unpark` before `park`
//! makes the park return at once. Fences rather than `SeqCst` accesses
//! because `loom` models a fence exactly and treats a `SeqCst` access as
//! `AcqRel`.
//!
//! # What the unparker costs
//!
//! `unpark` allocates nothing and takes no lock. With no parker parked it is
//! a fence and a load. With one parked it is also a swap and
//! `Thread::unpark`, which is a futex wake on Linux (a system call; the
//! wake-up itself, not a wait: it does not block on the parked thread).
//! [`unpark_first`](Unparker::unpark_first) unparks several parkers after
//! one fence.
//!
//! # Spurious returns
//!
//! `park` may return with nothing pending: the thread's park can return
//! spuriously, an `unpark` that found the closure already true leaves its
//! token for the next park, and a timeout ends it. The caller loops and
//! rechecks what it waits for, as with any park.

use std::time::Duration;

use std::sync::Arc;

use sync::{AtomicBool, Ordering, current, fence, park};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, Ordering, fence};
    #[cfg(loom)]
    pub(super) use loom::thread::{Thread, ThreadId, current, park};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, Ordering, fence};
    #[cfg(not(loom))]
    pub(super) use std::thread::{Thread, ThreadId, current, park};
}

/// The waking side of a parker pair: a wake-up for the one [`Parker`] made
/// with it. See the module docs (`src/parker.rs`).
///
/// Clones wake the same parker; any number of threads unpark. Each pair's
/// shared state is aligned to two cache lines, so two parkers' states never
/// share a line.
#[derive(Clone)]
pub struct Unparker {
    inner: Arc<Inner>,
}

/// The waiting side of a parker pair, made by [`parker()`].
///
/// One parker exists per pair: it is not `Clone` and parks through
/// `&mut self`. It is `Send`, so a control thread makes the pair and moves
/// the parker to the thread that parks. It binds the thread of its first
/// park; a park from any other thread panics (an unpark wakes the bound
/// thread, so a park elsewhere would never be woken).
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
/// use tutti_rt::parker;
///
/// let (unparker, mut parker) = parker();
/// let work = Arc::new(AtomicBool::new(false));
/// let thread = {
///     let work = Arc::clone(&work);
///     std::thread::spawn(move || {
///         while !work.load(Ordering::Acquire) {
///             parker.park(|| work.load(Ordering::Acquire));
///         }
///     })
/// };
/// work.store(true, Ordering::Release); // publish, then unpark
/// unparker.unpark();
/// thread.join().unwrap();
/// ```
pub struct Parker {
    inner: Arc<Inner>,
    /// The thread of the first park.
    bound: Option<sync::ThreadId>,
}

/// The state both sides share. Aligned to two cache lines, so the `parked`
/// flags of a slice of unparkers do not share lines.
#[repr(align(128))]
struct Inner {
    parked: AtomicBool,
    thread: std::sync::OnceLock<sync::Thread>,
}

/// A parker pair: its waking [`Unparker`] and its one [`Parker`].
#[must_use]
pub fn parker() -> (Unparker, Parker) {
    let inner = Arc::new(Inner {
        parked: AtomicBool::new(false),
        thread: std::sync::OnceLock::new(),
    });
    (
        Unparker {
            inner: Arc::clone(&inner),
        },
        Parker { inner, bound: None },
    )
}

impl Unparker {
    /// Wake the parker if it is parked (or about to park), returning
    /// whether it was. Call it **after** the write the parker's closure
    /// reads. Real-time safe (module docs, "What the unparker costs").
    pub fn unpark(&self) -> bool {
        fence(Ordering::SeqCst);
        self.unpark_after_fence()
    }

    /// Unpark `unparkers` in order until `n` parkers have been woken,
    /// returning how many were: one fence for the whole call, where `n`
    /// calls to [`unpark`](Self::unpark) issue one each. With `n == 0` it
    /// issues none. The same rules as `unpark`: call it after the write the
    /// parkers' closures read.
    pub fn unpark_first(unparkers: &[Unparker], n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        fence(Ordering::SeqCst);
        let mut woken = 0;
        for unparker in unparkers {
            if woken == n {
                break;
            }
            if unparker.unpark_after_fence() {
                woken += 1;
            }
        }
        woken
    }

    /// `unpark` after the caller's `SeqCst` fence, which is the waker's half
    /// of the store-load pair with the parker's fence in `park`: the two
    /// fences are totally ordered, so either this sees the parker's
    /// `parked`, or its closure sees the write before the fence.
    pub(crate) fn unpark_after_fence(&self) -> bool {
        let inner = &*self.inner;
        // The load first: the common case, a parker that is awake, is not a
        // read-modify-write on a line the parker owns.
        if inner.parked.load(Ordering::Relaxed) && inner.parked.swap(false, Ordering::Acquire) {
            if let Some(t) = inner.thread.get() {
                crate::scope::sanctioned(|| t.unpark());
            }
            return true;
        }
        false
    }

    /// Whether the parker has announced it is about to park, or is parked.
    /// A reading for diagnostics and tests; it can be stale the moment it
    /// returns.
    pub fn is_parked(&self) -> bool {
        self.inner.parked.load(Ordering::Acquire)
    }
}

impl Parker {
    /// Park the calling thread unless `pending` says there is already
    /// something to do, until [`unpark`](Unparker::unpark)ed. `pending`
    /// reads what the waker publishes. May return spuriously (module docs).
    ///
    /// # Panics
    ///
    /// If the calling thread is not the one of this parker's first park.
    pub fn park(&mut self, pending: impl FnOnce() -> bool) {
        self.park_until(pending, None);
    }

    /// [`park`](Self::park), returning after `timeout` at the latest.
    ///
    /// Under `--cfg loom` the timeout is ignored (loom has no timed park),
    /// which is the stricter model: no wake-up may be lost.
    ///
    /// # Panics
    ///
    /// If the calling thread is not the one of this parker's first park.
    pub fn park_timeout(&mut self, pending: impl FnOnce() -> bool, timeout: Duration) {
        self.park_until(pending, Some(timeout));
    }

    fn park_until(&mut self, pending: impl FnOnce() -> bool, timeout: Option<Duration>) {
        self.bind();
        self.arm(true, Ordering::Release);
        // Pairs with the fence in `unpark`: a write published before it is
        // seen by `pending`; one published after finds `parked` set and
        // unparks (an unpark before the park makes it return).
        fence(Ordering::SeqCst);
        if !pending() {
            match timeout {
                #[cfg(not(loom))]
                Some(t) => std::thread::park_timeout(t),
                _ => park(),
            }
        }
        self.arm(false, Ordering::Relaxed);
    }

    /// Binds the first parking thread, and refuses any other. Runs before
    /// `parked` is set, so an unparker that sees the flag sees the thread
    /// (the `Release` store, the unparker's `Acquire` swap).
    fn bind(&mut self) {
        let me = current();
        match self.bound {
            None => {
                self.bound = Some(me.id());
                // Only this parker sets it, once: the cell is empty.
                let _ = self.inner.thread.set(me);
            }
            Some(bound) => assert!(
                bound == me.id(),
                "a Parker parks on the thread of its first park: an unpark wakes that thread only"
            ),
        }
    }

    /// Every write to `parked` is a swap, so they are totally ordered with
    /// `unpark`'s: a plain store racing a swap is a case `loom` orders inside
    /// the swap's read-modify-write (its model then loses a wake-up no
    /// execution of the swap has). Only the parker's cold path pays for it.
    fn arm(&self, parked: bool, order: Ordering) {
        self.inner.parked.swap(parked, order);
    }
}

impl std::fmt::Debug for Unparker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unparker")
            .field("parked", &self.is_parked())
            .finish()
    }
}

impl std::fmt::Debug for Parker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parker")
            .field("bound", &self.bound.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// The parker's thread: parks on `work` until it is set.
    fn spawn_parker(mut parker: Parker, work: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        let work = Arc::clone(work);
        std::thread::spawn(move || {
            while !work.load(Ordering::Acquire) {
                parker.park(|| work.load(Ordering::Acquire));
            }
        })
    }

    /// A parked parker is woken by an unpark (`unpark` reports it), however
    /// late.
    #[test]
    fn an_unpark_wakes_a_parked_parker() {
        let (unparker, parker) = parker();
        let work = Arc::new(AtomicBool::new(false));
        let t = spawn_parker(parker, &work);
        // Wait until it is parked (`parked` is set just before the park).
        while !unparker.is_parked() {
            std::thread::yield_now();
        }
        assert!(!t.is_finished());
        // Unpark with `work` still false: the parker's check is false, so it
        // is committed to park, and this unpark must find `parked`. (Storing
        // `work` first races: a parker that sees it before parking clears
        // `parked` and leaves, and `unpark` truthfully returns false.)
        assert!(unparker.unpark());
        // Now the work, and an unpark that must not be lost; its return is
        // not asserted, since the parker may already be awake.
        work.store(true, Ordering::Release);
        unparker.unpark();
        t.join().unwrap();
    }

    /// Nothing parked: an unpark reports `false` and costs no thread wake.
    #[test]
    fn an_unpark_with_no_parked_thread_reports_it() {
        let (unparker, _parker) = parker();
        assert!(!unparker.unpark());
    }

    /// `unpark_first` reports the parkers it woke: none for `n == 0`, and
    /// two for `n == 2` of three parked parkers (which holds unless two of
    /// them are mid-spurious-return at once). That it wakes at most `n`, in
    /// order, is the loom model's property.
    #[test]
    fn unpark_first_reports_the_parkers_it_woke() {
        let done = Arc::new(AtomicBool::new(false));
        let (unparkers, threads): (Vec<_>, Vec<_>) = (0..3)
            .map(|_| {
                let (unparker, mut parker) = parker();
                let done = Arc::clone(&done);
                let t = std::thread::spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        parker.park(|| done.load(Ordering::Acquire));
                    }
                });
                (unparker, t)
            })
            .unzip();
        for unparker in &unparkers {
            while !unparker.is_parked() {
                std::thread::yield_now();
            }
        }
        assert_eq!(Unparker::unpark_first(&unparkers, 0), 0);
        assert_eq!(Unparker::unpark_first(&unparkers, 2), 2);
        done.store(true, Ordering::Release);
        Unparker::unpark_first(&unparkers, 3);
        for t in threads {
            t.join().unwrap();
        }
    }

    /// A timed park with nothing pending returns on its own: the test ends.
    #[test]
    fn a_timed_park_returns_without_an_unpark() {
        let (unparker, mut parker) = parker();
        parker.park_timeout(|| false, Duration::from_millis(20));
        assert!(
            !unparker.is_parked(),
            "the parker disarmed itself on return"
        );
    }

    /// A closure that is already true keeps the parker from parking.
    #[test]
    fn a_pending_park_does_not_park() {
        let (unparker, mut parker) = parker();
        parker.park(|| true);
        assert!(!unparker.is_parked());
    }

    /// A park from a thread other than the first park's panics, naming the
    /// misuse: an unpark would wake the first thread only.
    #[test]
    fn a_park_from_a_second_thread_panics() {
        let (_unparker, mut parker) = parker();
        parker.park(|| true);
        let panic = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parker.park(|| true);
            }))
            .expect_err("a second thread's park panics")
        })
        .join()
        .unwrap();
        let message = panic.downcast_ref::<&str>().copied().unwrap_or_default();
        assert!(message.contains("thread of its first park"), "{message}");
    }
}
