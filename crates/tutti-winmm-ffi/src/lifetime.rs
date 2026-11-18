//! The lifetime of an open input against its callback: who may touch the
//! state, and when it may be torn down.
//!
//! The OS calls a callback on a thread of its own, possibly after the owner has
//! begun to close, and names its input by an id, never a pointer. The state
//! lives in a [`Slot`] that a [`Registry`] holds one `Arc` of per open input:
//!
//! - [`Registry::enter`] looks the id up and counts the callback in, both
//!   under the registry lock; the returned [`Inside`] keeps the slot allocated
//!   and ends the count when dropped, however the callback leaves;
//! - [`Owner::begin_close`] removes the id under the same lock, so after it no
//!   `enter` finds the slot and the count only falls. It then sets
//!   [`Slot::closing`], an early-out for callbacks already inside;
//! - [`Closing::poll`] reports [`Poll::Quiesced`] once the count reads 0. The
//!   [`Closed`] it carries is the proof that no callback is inside or can
//!   enter, and the teardown that frees what the OS holds (reset, unprepare,
//!   close) takes its state only through it;
//! - [`Poll::TimedOut`] and [`Closing::leak`] give a [`Leaked`] instead, which
//!   owns the slot: dropping it frees the state, and [`Leaked::forget`] keeps it
//!   allocated for good, for an OS edge that cannot promise the OS has let go.
//!
//! No callback that entered holds a reference the teardown could free, because
//! the count is final (above) before the teardown starts, and the slot itself
//! is an `Arc`, so a callback that outlives the owner leaves last and frees it.
//!
//! The owner decides what a wait is: [`Closing::poll`] takes the time as a
//! `Duration` from any origin the caller keeps, and reads no clock.
//!
//! The models in `loom_model` and the tests below run this code.

use std::ops::Deref;
use std::time::Duration;

use sync::{Arc, AtomicBool, AtomicUsize, Mutex, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[cfg(loom)]
    pub(super) use loom::sync::{Arc, Mutex, MutexGuard};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::{Arc, Mutex, MutexGuard};
}

/// One open input's state and the counts that guard it.
#[derive(Debug)]
pub(crate) struct Slot<S> {
    state: S,
    /// Callbacks inside; incremented under the registry lock.
    in_flight: AtomicUsize,
    closing: AtomicBool,
}

impl<S> Slot<S> {
    /// Whether the owner has begun to close: what a callback already inside
    /// does not start now, and what the worker no longer feeds.
    pub(crate) fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
}

impl<S> Deref for Slot<S> {
    type Target = S;
    fn deref(&self) -> &S {
        &self.state
    }
}

/// The open inputs, by the id their callback is given.
#[derive(Debug)]
pub(crate) struct Registry<S> {
    slots: Mutex<Vec<(usize, Arc<Slot<S>>)>>,
    next: AtomicUsize,
}

impl<S> Registry<S> {
    #[cfg(not(loom))]
    pub(crate) const fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            next: AtomicUsize::new(1),
        }
    }

    #[cfg(loom)]
    pub(crate) fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            next: AtomicUsize::new(1),
        }
    }

    fn slots(&self) -> sync::MutexGuard<'_, Vec<(usize, Arc<Slot<S>>)>> {
        // A poisoned lock still holds a consistent list: every critical
        // section is a scan, a push or a retain.
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers `state` under a fresh id.
    pub(crate) fn open(&self, state: S) -> Owner<'_, S> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(Slot {
            state,
            in_flight: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
        });
        self.slots().push((id, Arc::clone(&slot)));
        Owner {
            registry: self,
            id,
            slot,
            registered: true,
        }
    }

    /// The callback's entry: the input named `id`, counted in, or `None` when
    /// it is not (or no longer) open.
    pub(crate) fn enter(&self, id: usize) -> Option<Inside<S>> {
        let slots = self.slots();
        let (_, slot) = slots.iter().find(|(registered, _)| *registered == id)?;
        slot.in_flight.fetch_add(1, Ordering::AcqRel);
        Some(Inside(Arc::clone(slot)))
    }

    fn remove(&self, id: usize) {
        self.slots().retain(|(registered, _)| *registered != id);
    }
}

/// A callback inside its body: the count drops when this does.
#[derive(Debug)]
pub(crate) struct Inside<S>(Arc<Slot<S>>);

impl<S> Deref for Inside<S> {
    type Target = Slot<S>;
    fn deref(&self) -> &Slot<S> {
        &self.0
    }
}

impl<S> Drop for Inside<S> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// An open input's registration. Dropping it removes the registration.
#[derive(Debug)]
pub(crate) struct Owner<'r, S> {
    registry: &'r Registry<S>,
    id: usize,
    slot: Arc<Slot<S>>,
    registered: bool,
}

impl<S> Owner<'_, S> {
    /// The id the callback is given.
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    /// The slot, for a thread of the input's own.
    pub(crate) fn slot(&self) -> Arc<Slot<S>> {
        Arc::clone(&self.slot)
    }

    /// Removes the registration: no `enter` from here finds the slot.
    pub(crate) fn unregister(&mut self) {
        if std::mem::take(&mut self.registered) {
            self.registry.remove(self.id);
        }
    }

    /// Unregisters, then sets `closing`. `bound` is how long
    /// [`Closing::poll`] waits for the callbacks inside.
    pub(crate) fn begin_close(mut self, bound: Duration) -> Closing<S> {
        self.unregister();
        self.slot.closing.store(true, Ordering::Release);
        Closing {
            slot: Arc::clone(&self.slot),
            bound,
            since: None,
        }
    }
}

impl<S> Deref for Owner<'_, S> {
    type Target = Slot<S>;
    fn deref(&self) -> &Slot<S> {
        &self.slot
    }
}

impl<S> Drop for Owner<'_, S> {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// A close that waits for the callbacks inside to leave.
#[derive(Debug)]
pub(crate) struct Closing<S> {
    slot: Arc<Slot<S>>,
    bound: Duration,
    since: Option<Duration>,
}

/// What one look at a [`Closing`] found.
#[must_use]
#[derive(Debug)]
pub(crate) enum Poll<S> {
    /// A callback is inside and the bound has not passed: look again.
    Waiting(Closing<S>),
    /// No callback is inside or can enter.
    Quiesced(Closed<S>),
    /// A callback was still inside `bound` after the first look.
    TimedOut(Leaked<S>),
}

impl<S> Closing<S> {
    /// Looks at the count. `now` is a time from any origin the caller keeps;
    /// the first look starts the wait, so a count that is already 0 never
    /// times out.
    pub(crate) fn poll(mut self, now: Duration) -> Poll<S> {
        if self.slot.in_flight.load(Ordering::Acquire) == 0 {
            return Poll::Quiesced(Closed(self.slot));
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_sub(since) >= self.bound {
            return Poll::TimedOut(Leaked(self.slot));
        }
        Poll::Waiting(self)
    }

    /// Gives up without waiting: the OS edge cannot promise it is done with the
    /// state.
    pub(crate) fn leak(self) -> Leaked<S> {
        Leaked(self.slot)
    }
}

/// A closed input with no callback inside: the teardown may free what the OS
/// holds of the state.
#[derive(Debug)]
pub(crate) struct Closed<S>(Arc<Slot<S>>);

impl<S> Closed<S> {
    /// The teardown is over. `released` is whether the OS has let go of the
    /// state (the port closed, or gone); when it has not, the state is
    /// [`Leaked`].
    pub(crate) fn finish(self, released: bool) -> Outcome<S> {
        if released {
            Outcome::Closed
        } else {
            Outcome::Leaked(Leaked(self.0))
        }
    }
}

impl<S> Deref for Closed<S> {
    type Target = Slot<S>;
    fn deref(&self) -> &Slot<S> {
        &self.0
    }
}

/// How a close ended.
#[must_use]
#[derive(Debug)]
pub(crate) enum Outcome<S> {
    /// The OS holds nothing of the state; it is freed with its last reference.
    Closed,
    /// The OS may still hold the state.
    Leaked(Leaked<S>),
}

/// A state the OS may still reach. Dropping it frees the state with its last
/// reference; [`forget`](Self::forget) keeps it allocated for good.
#[must_use]
#[derive(Debug)]
pub(crate) struct Leaked<S>(Arc<Slot<S>>);

impl<S> Leaked<S> {
    /// Keeps the state allocated for the life of the process.
    // `loom` reports a forgotten `Arc` as a leak, so its models never call this.
    #[cfg_attr(loom, allow(dead_code))]
    pub(crate) fn forget(self) {
        std::mem::forget(self.0);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const BOUND: Duration = Duration::from_millis(10);

    /// Counts its own drops.
    struct Counted<'a>(&'a AtomicUsize);
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn enter_finds_an_open_id_and_no_other() {
        let registry = Registry::new();
        let owner = registry.open(());
        assert!(registry.enter(owner.id()).is_some());
        assert!(registry.enter(owner.id() + 1).is_none());
    }

    #[test]
    fn enter_after_begin_close_finds_nothing_and_a_closing_flag_is_set_for_those_inside() {
        let registry = Registry::new();
        let owner = registry.open(());
        let id = owner.id();
        let inside = registry.enter(id).expect("open");
        assert!(!inside.closing());
        assert!(!owner.slot().closing());
        let _closing = owner.begin_close(BOUND);
        assert!(inside.closing());
        assert!(registry.enter(id).is_none());
    }

    #[test]
    fn a_dropped_owner_unregisters() {
        let registry = Registry::new();
        let owner = registry.open(());
        let id = owner.id();
        drop(owner);
        assert!(registry.enter(id).is_none());
    }

    #[test]
    fn no_callback_inside_is_quiesced_at_the_first_look_even_with_a_zero_bound() {
        let registry = Registry::new();
        let closing = registry.open(()).begin_close(Duration::ZERO);
        assert!(matches!(closing.poll(Duration::ZERO), Poll::Quiesced(_)));
    }

    #[test]
    fn a_callback_inside_holds_the_wait_until_the_bound_then_times_out() {
        let registry = Registry::new();
        let owner = registry.open(());
        let _inside = registry.enter(owner.id()).expect("open");
        let t0 = Duration::from_secs(100);
        let Poll::Waiting(closing) = owner.begin_close(BOUND).poll(t0) else {
            panic!("a callback is inside");
        };
        let Poll::Waiting(closing) = closing.poll(t0 + BOUND - Duration::from_nanos(1)) else {
            panic!("the bound has not passed");
        };
        let Poll::TimedOut(leaked) = closing.poll(t0 + BOUND) else {
            panic!("the bound has passed");
        };
        leaked.forget();
    }

    #[test]
    fn the_last_callback_out_quiesces_the_close() {
        let registry = Registry::new();
        let owner = registry.open(());
        let id = owner.id();
        let first = registry.enter(id).expect("open");
        let second = registry.enter(id).expect("a second callback may be inside");
        let Poll::Waiting(closing) = owner.begin_close(BOUND).poll(Duration::ZERO) else {
            panic!("two callbacks are inside");
        };
        drop(first);
        let Poll::Waiting(closing) = closing.poll(Duration::ZERO) else {
            panic!("one callback is still inside");
        };
        drop(second);
        assert!(matches!(closing.poll(Duration::ZERO), Poll::Quiesced(_)));
    }

    #[test]
    fn a_released_close_frees_the_state_once() {
        let drops = AtomicUsize::new(0);
        let registry = Registry::new();
        let Poll::Quiesced(closed) = registry
            .open(Counted(&drops))
            .begin_close(BOUND)
            .poll(Duration::ZERO)
        else {
            panic!("nothing is inside");
        };
        assert!(matches!(closed.finish(true), Outcome::Closed));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unreleased_close_keeps_the_state_until_the_leak_is_dropped_or_forgotten() {
        let drops = AtomicUsize::new(0);
        let registry = Registry::new();
        let Poll::Quiesced(closed) = registry
            .open(Counted(&drops))
            .begin_close(BOUND)
            .poll(Duration::ZERO)
        else {
            panic!("nothing is inside");
        };
        let Outcome::Leaked(leaked) = closed.finish(false) else {
            panic!("the OS has not let go");
        };
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(leaked);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let registry = Registry::new();
        registry
            .open(Counted(&drops))
            .begin_close(BOUND)
            .leak()
            .forget();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

/// The shipped code under `loom`: `cargo test -p tutti-winmm-ffi --release
/// --lib loom_model` with the `loom` cfg flag set.
///
/// The state is a `loom` cell standing for what the OS writes and the teardown
/// frees (the headers and buffers). A callback reads it inside an [`Inside`];
/// the teardown writes it, and its drop writes it again, so loom reports any
/// interleaving in which a read and a write are not ordered by the protocol.
/// Over every interleaving:
///
/// - a callback either finds nothing or finishes its read before the teardown
///   runs;
/// - the state is dropped exactly once, by whichever reference goes last;
/// - a timed-out close never runs the teardown, and its [`Leaked`] frees the
///   state once the stuck callback has gone.
#[cfg(all(test, loom))]
mod loom_model {
    use loom::cell::UnsafeCell;
    use loom::thread;

    use super::*;

    struct Probe {
        cell: UnsafeCell<u32>,
        drops: Arc<AtomicUsize>,
    }

    // SAFETY: the cell is read and written only as the protocol
    // orders them, which is what the model checks.
    unsafe impl Sync for Probe {}

    impl Probe {
        fn new(drops: &Arc<AtomicUsize>) -> Self {
            Self {
                cell: UnsafeCell::new(0),
                drops: Arc::clone(drops),
            }
        }

        fn read(&self) {
            // SAFETY: a shared access loom checks.
            self.cell.with(|p| unsafe { *p });
        }

        fn teardown(&self) {
            // SAFETY: an exclusive access loom checks.
            self.cell.with_mut(|p| unsafe { *p = 1 });
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.teardown();
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    type Shared = Arc<Registry<Probe>>;

    /// A callback: reads the state if it finds it, once more for each of
    /// `nested`, each a fresh entry made while the earlier one is held.
    fn callback(registry: &Shared, id: usize, nested: usize) -> thread::JoinHandle<()> {
        let registry = Arc::clone(registry);
        thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..=nested {
                if let Some(inside) = registry.enter(id) {
                    inside.read();
                    held.push(inside);
                }
            }
        })
    }

    /// Polls with the time frozen, so only the count decides.
    fn wait(mut closing: Closing<Probe>) -> Closed<Probe> {
        loop {
            match closing.poll(Duration::ZERO) {
                Poll::Quiesced(closed) => return closed,
                Poll::Waiting(again) => {
                    closing = again;
                    thread::yield_now();
                }
                Poll::TimedOut(_) => unreachable!("the bound is endless"),
            }
        }
    }

    fn run(callbacks: &'static [usize]) {
        loom::model(move || {
            let drops = Arc::new(AtomicUsize::new(0));
            let registry: Shared = Arc::new(Registry::new());
            let owner = registry.open(Probe::new(&drops));
            let id = owner.id();
            let threads: Vec<_> = callbacks
                .iter()
                .map(|&n| callback(&registry, id, n))
                .collect();
            let slot = owner.slot();
            let closed = wait(owner.begin_close(Duration::MAX));
            assert!(slot.closing());
            drop(slot);
            closed.teardown();
            assert!(matches!(closed.finish(true), Outcome::Closed));
            for t in threads {
                t.join().expect("callback");
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_callback_racing_the_close_reads_before_the_teardown_or_finds_nothing() {
        run(&[0]);
    }

    #[test]
    fn two_callbacks_and_a_re_entry_all_leave_before_the_teardown() {
        run(&[1, 0]);
    }

    #[test]
    fn a_timed_out_close_runs_no_teardown_and_its_leak_frees_the_state_after_the_callback() {
        loom::model(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            let registry: Shared = Arc::new(Registry::new());
            let owner = registry.open(Probe::new(&drops));
            let id = owner.id();
            let go = Arc::new(AtomicBool::new(false));
            let stuck = {
                let (registry, go) = (Arc::clone(&registry), Arc::clone(&go));
                thread::spawn(move || {
                    let Some(inside) = registry.enter(id) else {
                        return;
                    };
                    while !go.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    inside.read();
                })
            };
            let leaked = match owner.begin_close(Duration::ZERO).poll(Duration::ZERO) {
                // The callback had not entered, and now cannot.
                Poll::Quiesced(closed) => {
                    closed.teardown();
                    assert!(matches!(closed.finish(true), Outcome::Closed));
                    None
                }
                Poll::TimedOut(leaked) => Some(leaked),
                Poll::Waiting(_) => unreachable!("a zero bound does not wait"),
            };
            go.store(true, Ordering::Release);
            stuck.join().expect("callback");
            if let Some(leaked) = leaked {
                // The callback is gone; the leak is the last reference.
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                drop(leaked);
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_leak_taken_without_waiting_frees_the_state_only_after_the_callback_inside_leaves() {
        loom::model(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            let registry: Shared = Arc::new(Registry::new());
            let owner = registry.open(Probe::new(&drops));
            let reader = callback(&registry, owner.id(), 0);
            let leaked = owner.begin_close(Duration::MAX).leak();
            reader.join().expect("callback");
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(leaked);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_close_the_os_did_not_release_keeps_the_state_until_the_leak_drops() {
        loom::model(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            let registry: Shared = Arc::new(Registry::new());
            let owner = registry.open(Probe::new(&drops));
            let closed = wait(owner.begin_close(Duration::MAX));
            closed.teardown();
            let Outcome::Leaked(leaked) = closed.finish(false) else {
                panic!("the OS did not let go");
            };
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(leaked);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }
}
