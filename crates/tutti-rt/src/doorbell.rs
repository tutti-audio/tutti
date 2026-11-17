//! [`doorbell`]: wake one sleeping thread from another, without the waker
//! ever waiting.
//!
//! A helper thread that has nothing to do parks; the audio thread, which
//! must not lock, allocate or wait, rings it when work arrives. This is the
//! pair of fences and the park that a worker pool's helper carries, as one
//! primitive with a `loom` model (`tests/doorbell_loom.rs`) over the shipped
//! code.
//!
//! [`doorbell`] makes the pair: a [`Doorbell`], the ringing side (`Clone`,
//! `Send + Sync`, held by every ringer), and a [`Sleeper`], the waiting side.
//! There is one `Sleeper` per doorbell, by type: it is not `Clone`, and
//! `wait` takes `&mut self`. It binds the thread of its first wait and stays
//! on it: a wait from another thread panics, since a ring unparks one
//! thread only.
//!
//! # The protocol
//!
//! The ringer **publishes** what the sleeper is waiting for (opens a gate,
//! pushes to a ring, sets a flag: any write), then calls [`ring`](Doorbell::ring).
//! The sleeper calls [`wait`](Sleeper::wait) with a closure that reads what
//! the ringer publishes:
//!
//! 1. `wait` stores `asleep = true`, issues a `SeqCst` fence, then runs the
//!    closure; only if it says nothing is pending does it park;
//! 2. `ring` issues a `SeqCst` fence, then reads `asleep`; only if it is set
//!    does it clear it and unpark the thread.
//!
//! The two `SeqCst` fences are totally ordered (this is the store-load
//! Dekker pattern), so either
//! the closure sees the ringer's write and the sleeper does not park, or
//! `ring` sees `asleep` and unparks. A ring that lands before the park is
//! not lost either: `unpark` before `park` makes the park return at once.
//! Fences rather than `SeqCst` accesses because `loom` models a fence
//! exactly and treats a `SeqCst` access as `AcqRel`.
//!
//! # What the ringer costs
//!
//! `ring` allocates nothing and takes no lock. With no sleeper parked it is
//! a fence and a load. With one parked it is also a swap and
//! `Thread::unpark`, which is a futex wake on Linux (a system call; the
//! wake-up itself, not a wait: it does not block on the sleeper).
//! [`ring_first`](Doorbell::ring_first) rings several bells after one fence.
//!
//! # Spurious returns
//!
//! `wait` may return with nothing pending: `park` can return spuriously, a
//! `ring` that found the closure already true leaves its unpark token for
//! the next park, and a timeout ends it. The caller loops and rechecks what
//! it waits for, as with any park.

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

/// The ringing side of a doorbell: a wake-up for the one [`Sleeper`] made
/// with it. See the module docs (`src/doorbell.rs`).
///
/// Clones ring the same sleeper; any number of threads ring. A slice of
/// doorbells for [`ring_first`](Self::ring_first) does not share cache lines
/// between sleepers (each one's state is aligned to two lines).
#[derive(Clone)]
pub struct Doorbell {
    inner: Arc<Inner>,
}

/// The waiting side of a doorbell, made by [`doorbell`].
///
/// One sleeper exists per doorbell: it is not `Clone` and waits through
/// `&mut self`. It is `Send`, so a control thread makes the pair and moves
/// the sleeper to the thread that waits. It binds the thread of its first
/// wait; a wait from any other thread panics (a ring unparks the bound
/// thread, so a wait elsewhere would never be woken).
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
/// use tutti_rt::doorbell;
///
/// let (bell, mut sleeper) = doorbell();
/// let work = Arc::new(AtomicBool::new(false));
/// let thread = {
///     let work = Arc::clone(&work);
///     std::thread::spawn(move || {
///         while !work.load(Ordering::Acquire) {
///             sleeper.wait(|| work.load(Ordering::Acquire));
///         }
///     })
/// };
/// work.store(true, Ordering::Release); // publish, then ring
/// bell.ring();
/// thread.join().unwrap();
/// ```
pub struct Sleeper {
    inner: Arc<Inner>,
    /// The thread of the first wait.
    bound: Option<sync::ThreadId>,
}

/// The state both sides share. Aligned to two cache lines, so the `asleep`
/// flags of a slice of doorbells do not share lines.
#[repr(align(128))]
struct Inner {
    asleep: AtomicBool,
    thread: std::sync::OnceLock<sync::Thread>,
}

/// A doorbell: its ringing [`Doorbell`] and its one [`Sleeper`].
#[must_use]
pub fn doorbell() -> (Doorbell, Sleeper) {
    let inner = Arc::new(Inner {
        asleep: AtomicBool::new(false),
        thread: std::sync::OnceLock::new(),
    });
    (
        Doorbell {
            inner: Arc::clone(&inner),
        },
        Sleeper { inner, bound: None },
    )
}

impl Doorbell {
    /// Wake the sleeper if it is parked (or about to park), returning
    /// whether it was. Call it **after** the write the sleeper's closure
    /// reads. Real-time safe (module docs, "What the ringer costs").
    pub fn ring(&self) -> bool {
        fence(Ordering::SeqCst);
        self.ring_after_fence()
    }

    /// Ring `bells` in order until `n` sleepers have been woken, returning
    /// how many were: one fence for the whole call, where `n` calls to
    /// [`ring`](Self::ring) issue one each. With `n == 0` it issues none.
    /// The same rules as `ring`: call it after the write the sleepers'
    /// closures read.
    pub fn ring_first(bells: &[Doorbell], n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        fence(Ordering::SeqCst);
        let mut woken = 0;
        for bell in bells {
            if woken == n {
                break;
            }
            if bell.ring_after_fence() {
                woken += 1;
            }
        }
        woken
    }

    /// `ring` after the caller's `SeqCst` fence, which is the ringer's half
    /// of the store-load pair with the sleeper's fence in `park`: the two
    /// fences are totally ordered, so either this sees the sleeper's
    /// `asleep`, or its closure sees the write before the fence.
    fn ring_after_fence(&self) -> bool {
        let inner = &*self.inner;
        // The load first: the common case, a sleeper that is awake, is not a
        // read-modify-write on a line the sleeper owns.
        if inner.asleep.load(Ordering::Relaxed) && inner.asleep.swap(false, Ordering::Acquire) {
            if let Some(t) = inner.thread.get() {
                t.unpark();
            }
            return true;
        }
        false
    }

    /// Whether the sleeper has announced it is about to park, or is parked.
    /// A reading for diagnostics and tests; it can be stale the moment it
    /// returns.
    pub fn is_asleep(&self) -> bool {
        self.inner.asleep.load(Ordering::Acquire)
    }
}

impl Sleeper {
    /// Park the calling thread unless `pending` says there is already
    /// something to do, until [`ring`](Doorbell::ring)ed. `pending` reads what
    /// the ringer publishes. May return spuriously (module docs).
    ///
    /// # Panics
    ///
    /// If the calling thread is not the one of this sleeper's first wait.
    pub fn wait(&mut self, pending: impl FnOnce() -> bool) {
        self.park(pending, None);
    }

    /// [`wait`](Self::wait), returning after `timeout` at the latest.
    ///
    /// Under `--cfg loom` the timeout is ignored (loom has no timed park),
    /// which is the stricter model: no wake-up may be lost.
    ///
    /// # Panics
    ///
    /// If the calling thread is not the one of this sleeper's first wait.
    pub fn wait_timeout(&mut self, pending: impl FnOnce() -> bool, timeout: Duration) {
        self.park(pending, Some(timeout));
    }

    fn park(&mut self, pending: impl FnOnce() -> bool, timeout: Option<Duration>) {
        self.bind();
        self.arm(true, Ordering::Release);
        // Pairs with the fence in `ring`: a write published before it is
        // seen by `pending`; one published after finds `asleep` set and
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

    /// Binds the first waiting thread, and refuses any other. Runs before
    /// `asleep` is set, so a ringer that sees the flag sees the thread (the
    /// `Release` store, the ringer's `Acquire` swap).
    fn bind(&mut self) {
        let me = current();
        match self.bound {
            None => {
                self.bound = Some(me.id());
                // Only this sleeper sets it, once: the cell is empty.
                let _ = self.inner.thread.set(me);
            }
            Some(bound) => assert!(
                bound == me.id(),
                "a Sleeper waits on the thread of its first wait: a ring unparks that thread only"
            ),
        }
    }

    /// Every write to `asleep` is a swap, so they are totally ordered with
    /// `ring`'s: a plain store racing a swap is a case `loom` orders inside
    /// the swap's read-modify-write (its model then loses a wake-up no
    /// execution of the swap has). Only the sleeper's cold path pays for it.
    fn arm(&self, asleep: bool, order: Ordering) {
        self.inner.asleep.swap(asleep, order);
    }
}

impl std::fmt::Debug for Doorbell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Doorbell")
            .field("asleep", &self.is_asleep())
            .finish()
    }
}

impl std::fmt::Debug for Sleeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sleeper")
            .field("bound", &self.bound.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// The sleeper's thread: waits on `work` until it is set.
    fn spawn_sleeper(mut sleeper: Sleeper, work: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        let work = Arc::clone(work);
        std::thread::spawn(move || {
            while !work.load(Ordering::Acquire) {
                sleeper.wait(|| work.load(Ordering::Acquire));
            }
        })
    }

    /// A parked sleeper is woken by a ring (`ring` reports it), however late.
    #[test]
    fn a_ring_wakes_a_parked_sleeper() {
        let (bell, sleeper) = doorbell();
        let work = Arc::new(AtomicBool::new(false));
        let t = spawn_sleeper(sleeper, &work);
        // Wait until it is parked (asleep is set just before the park).
        while !bell.is_asleep() {
            std::thread::yield_now();
        }
        assert!(!t.is_finished());
        // Ring with `work` still false: the sleeper's check is false, so it
        // is committed to park, and this ring must find `asleep`. (Storing
        // `work` first races: a sleeper that sees it before parking clears
        // `asleep` and leaves, and `ring` truthfully returns false.)
        assert!(bell.ring());
        // Now the work, and a ring that must not be lost; its return is
        // not asserted, since the sleeper may already be awake.
        work.store(true, Ordering::Release);
        bell.ring();
        t.join().unwrap();
    }

    /// Nobody asleep: a ring reports `false` and costs no unpark.
    #[test]
    fn a_ring_with_no_sleeper_reports_it() {
        let (bell, _sleeper) = doorbell();
        assert!(!bell.ring());
    }

    /// `ring_first` with `n` wakes `n` of the parked sleepers and no more;
    /// the rest stay asleep.
    #[test]
    fn ring_first_wakes_at_most_n_sleepers() {
        let work = Arc::new(AtomicBool::new(false));
        let (bells, threads): (Vec<_>, Vec<_>) = (0..3)
            .map(|_| {
                let (bell, mut sleeper) = doorbell();
                let work = Arc::clone(&work);
                let (tx, rx) = std::sync::mpsc::channel::<()>();
                let t = std::thread::spawn(move || {
                    // Parks until rung (the flag stays false), then reports.
                    sleeper.wait(|| work.load(Ordering::Acquire));
                    tx.send(()).unwrap();
                });
                (bell, (t, rx))
            })
            .unzip();
        for bell in &bells {
            while !bell.is_asleep() {
                std::thread::yield_now();
            }
        }
        assert_eq!(Doorbell::ring_first(&bells, 2), 2);
        assert_eq!(Doorbell::ring_first(&bells, 0), 0);
        // The first two were woken, in order; the third is still asleep.
        threads[0].1.recv().unwrap();
        threads[1].1.recv().unwrap();
        assert!(bells[2].is_asleep());
        assert!(threads[2].1.try_recv().is_err());
        assert!(bells[2].ring());
        for (t, rx) in threads {
            t.join().unwrap();
            drop(rx);
        }
    }

    /// A timed wait with nothing pending returns on its own: the test ends.
    #[test]
    fn a_timed_wait_returns_without_a_ring() {
        let (bell, mut sleeper) = doorbell();
        sleeper.wait_timeout(|| false, Duration::from_millis(20));
        assert!(!bell.is_asleep(), "the waiter disarmed itself on return");
    }

    /// A closure that is already true keeps the sleeper from parking.
    #[test]
    fn a_pending_wait_does_not_park() {
        let (bell, mut sleeper) = doorbell();
        sleeper.wait(|| true);
        assert!(!bell.is_asleep());
    }

    /// A wait from a thread other than the first wait's panics, naming the
    /// misuse, before it arms: a ring would unpark the first thread only.
    #[test]
    fn a_wait_from_a_second_thread_panics_without_arming() {
        let (bell, mut sleeper) = doorbell();
        sleeper.wait(|| true);
        let (bell, panic) = std::thread::spawn(move || {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sleeper.wait(|| true);
            }))
            .expect_err("a second thread's wait panics");
            (bell, panic)
        })
        .join()
        .unwrap();
        let message = panic.downcast_ref::<&str>().copied().unwrap_or_default();
        assert!(message.contains("thread of its first wait"), "{message}");
        assert!(!bell.is_asleep());
    }
}
