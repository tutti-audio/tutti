//! [`Bells`]: carry an audio thread's ring to a future's [`Waker`], with the
//! audio thread never touching the `Waker`.
//!
//! The audio thread must not allocate, lock or wait, and an executor's
//! `Waker::wake` may do any of them. A [`Bell`] is the audio thread's half:
//! [`ring`](Bell::ring) sets a flag and, when a future is waiting on the bell,
//! unparks one control-side [`Parker`]. A
//! [`Listener`] is the control side's half: a future resolves once its bell has
//! rung. Between them, [`Bells`], one per session, runs the **tick** that
//! notices a ring and wakes the waiting future's `Waker`.
//!
//! # The tick
//!
//! Where a thread can start, [`Bells::new`] spawns one **tick task** on a
//! dedicated thread of the executor it is given
//! (`Executor::spawn_dedicated`); it parks on the shared parker and, on each
//! wake, runs one pass over every bell. Where none can (wasm), there is no
//! thread and [`Bells::tick`] runs the same pass when its caller does, once per
//! `Session::update`. On a native target `tick` does nothing, so a caller calls
//! it unconditionally. One `cfg` chooses between the two (`bells/tick_thread.rs`, `bells/tick_inline.rs`).
//!
//! # The protocol
//!
//! Each bell has a flag (`rung`) and a slot for one `Waker`.
//!
//! - **ring** (audio thread): set `rung` (a swap), fence, then, only if
//!   `waiting` is set, [`Unparker::unpark_after_fence`] (the parker's
//!   publish-then-unpark protocol, `parker.rs`). A bell nobody waits on costs a
//!   swap, a fence and a load: it never wakes the tick.
//! - **tick pass**: for each bell, if `rung` is set **and** a waker is stored,
//!   take the waker and wake it. A pass never clears `rung`: only the wait side
//!   does. The tick task parks with a pending check, "some bell is rung and
//!   has a stored waker", so a rung bell with nobody waiting does not make it
//!   spin, and a pass that took a waker leaves nothing pending.
//! - **wait poll** (control side):
//!   1. if `rung.swap(false)`, the poll is `Ready`;
//!   2. otherwise store the context's waker;
//!   3. fence, then check `rung` again: if it is set, take the waker back,
//!      clear `rung` and return `Ready`; otherwise `Pending`.
//!
//! The wait poll's fence and the ring's are the store-load pair (the poll
//! stores `waiting`, fences, loads `rung`; the ring stores `rung`, fences, loads
//! `waiting`): either the poll sees the ring in step 3, or the ring sees
//! `waiting` and unparks the tick, whose pass after it wakes the waker. A ring and a
//! pass that both land before step 2 are the case step 3 closes. A wake that
//! finds the flag already cleared is a spurious wake, which the future absorbs
//! by polling again. Two rings before one resolve resolve once: the flag is one
//! bit, and the second ring finds the tick parked or the waker gone.
//!
//! Every write to `rung` is a swap, as every write to the parker's flag is
//! (`parker.rs`): a plain store racing the wait side's swap is a case loom
//! orders inside the swap's read-modify-write, and its model then loses a
//! ring no execution of the swap has.
//!
//! # Shutdown
//!
//! Dropping the [`Bells`] sets `shut` and runs one **last pass** over every
//! live bell (`Shared::last_pass`): on the tick task's way out, or, with the
//! inline tick, in the drop itself. The last pass takes every stored waker.
//! A rung bell's waker is woken, as in a pass; an unrung bell's is dropped
//! unwoken, which breaks the cycle a parked future makes (executor task,
//! future, [`Listener`], its state, stored waker, task).
//!
//! After `shut`, a wait poll never leaves a waker stored: step 2 is skipped
//! when `shut` is already set, and a poll that stored one checks `shut` after
//! step 3's fence and takes the waker back if it is set. `shut`'s store, then
//! a fence, then the last pass's takes, pair with the poll's store of the
//! waker, then a fence, then its load of `shut`: either the last pass finds
//! the waker, or the poll sees `shut` and removes it itself.
//!
//! Only a ring that happens-before the drop is delivered. A ring concurrent
//! with it can land after the last pass read the bell unrung: its waker is
//! released unwoken, and the future stays parked under an executor that
//! outlives the set. An owner that rings at shutdown drops what rings before
//! the [`Bells`].
//!
//! The model in `tests/bells_loom.rs` runs this code.
//!
//! # Drops
//!
//! A [`Bell`] holds an `Arc` of its bell's state, so the last clone to drop
//! frees it: it is dropped on the control side, never on the audio thread (a
//! debug build panics otherwise, as [`ControlDrop`] does for any such handle).

#![allow(
    clippy::disallowed_types,
    reason = "the set's locks are loom's under loom, so the models see them; the tick and the control side take them, a ring never does"
)]

#[cfg(not(loom))]
use std::io;
use std::sync::PoisonError;
use std::task::{Context, Poll, Waker};

use crate::sync::{Arc, AtomicBool, Mutex, MutexGuard, Ordering, fence};

use crate::{ControlDrop, Parker, Unparker, parker};

#[cfg_attr(loom, path = "bells/tick_stepped.rs")]
#[cfg_attr(all(not(loom), target_arch = "wasm32"), path = "bells/tick_inline.rs")]
#[cfg_attr(
    all(not(loom), not(target_arch = "wasm32")),
    path = "bells/tick_thread.rs"
)]
mod tick;
#[cfg(any(loom, not(target_arch = "wasm32")))]
mod tick_body;

#[allow(
    clippy::disallowed_types,
    reason = "the waker lock is `crate::sync`'s Mutex: loom's under the bell's loom model, std's otherwise"
)]
/// One bell's state, shared by its [`Bell`] clones and its [`Listener`].
struct State {
    /// Rung since the listener last resolved.
    rung: AtomicBool,
    /// The waker of the future waiting on the bell. Control-side threads only.
    waker: Mutex<Option<Waker>>,
    /// `waker` is `Some`: set and cleared inside `waker`'s lock, and read
    /// without it, so the tick task's park check is two atomic loads that
    /// the fences order (a mutex is not ordered by a fence).
    waiting: AtomicBool,
    /// The set's `shut` flag, which the wait poll reads.
    shut: Arc<AtomicBool>,
}

impl State {
    fn waker(&self) -> MutexGuard<'_, Option<Waker>> {
        self.waker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores the waiting future's waker.
    fn store_waker(&self, waker: Waker) {
        let mut slot = self.waker();
        *slot = Some(waker);
        self.waiting.store(true, Ordering::Release);
    }

    /// Takes the waker back, if there is one.
    fn take_waker(&self) -> Option<Waker> {
        let mut slot = self.waker();
        self.waiting.store(false, Ordering::Release);
        slot.take()
    }
}

/// What the ticks and the bells share.
struct Shared {
    bells: Mutex<Vec<Arc<State>>>,
    shut: Arc<AtomicBool>,
    unparker: Unparker,
}

impl Shared {
    fn bells(&self) -> MutexGuard<'_, Vec<Arc<State>>> {
        self.bells.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One pass: wake the waker of every rung bell that has one, and forget
    /// the bells whose both sides are gone. Wakes outside every lock, since a
    /// `Waker` may do anything, including poll the listener.
    fn pass(&self) {
        let live: Vec<Arc<State>> = {
            let mut bells = self.bells();
            bells.retain(|state| Arc::strong_count(state) > 1);
            bells.clone()
        };
        for state in live {
            if state.rung.load(Ordering::Acquire) {
                let waker = state.take_waker();
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
        }
    }

    /// The pass at shutdown: take the waker of every live bell, waking a rung
    /// bell's and dropping an unrung one's unwoken, so no stored waker
    /// outlives the set. Call it after `shut` is set; the fence pairs with a
    /// wait poll's (see "Shutdown" in the module docs). Wakes and drops
    /// outside every lock.
    fn last_pass(&self) {
        fence(Ordering::SeqCst);
        let live: Vec<Arc<State>> = self.bells().clone();
        for state in live {
            let rung = state.rung.load(Ordering::Acquire);
            // An unrung bell's waker drops unwoken at the end of the `if`.
            if let Some(waker) = state.take_waker()
                && rung
            {
                waker.wake();
            }
        }
    }
}

/// The control-side set of bells: one per session, and the tick that wakes
/// their futures. See the module docs (`src/bells.rs`).
///
/// Dropping it ends the tick task, if there is one, and waits for it. A bell
/// rung before the drop wakes its waiting future; a future still waiting on an
/// unrung bell is never woken again, and its waker is released. After the drop
/// a wait poll stores no waker. Only a ring that happens-before the drop is
/// delivered, so drop what rings at shutdown first.
#[must_use]
pub struct Bells {
    shared: Arc<Shared>,
    tick: tick::Tick,
}

impl Bells {
    /// An empty set, with its tick task on a dedicated thread of `executor`
    /// where a thread can start.
    ///
    /// Drop the set before the executor's last clone: the tick then ends through
    /// its own shutdown and runs its last pass. An executor that drops first stops
    /// the tick, which ends without waiting out the executor's stop bound.
    ///
    /// # Errors
    ///
    /// The OS's refusal to start the tick's thread.
    #[cfg(not(loom))]
    pub fn new(executor: &tutti_platform::Executor) -> io::Result<Self> {
        let (shared, parker) = Self::shared();
        let tick = tick::Tick::start(&shared, parker, executor)?;
        Ok(Self { shared, tick })
    }

    fn shared() -> (Arc<Shared>, Parker) {
        let (unparker, parker) = parker();
        let shared = Arc::new(Shared {
            bells: Mutex::new(Vec::new()),
            shut: Arc::new(AtomicBool::new(false)),
            unparker,
        });
        (shared, parker)
    }

    /// A new bell in the set: the ring side for the audio thread and the wait
    /// side for the control-side future.
    pub fn bell(&self) -> (Bell, Listener) {
        self.handle().bell()
    }

    /// A handle that makes bells in this set without owning it: the part of
    /// the session that opens taps holds one, the set stays with its owner.
    pub fn handle(&self) -> BellsHandle {
        BellsHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Runs one pass where the set has no tick task (wasm), and does nothing
    /// where it has one: call it unconditionally, once per `Session::update`.
    /// It wakes the waker of every rung bell that has one.
    pub fn tick(&self) {
        self.tick.run(&self.shared);
    }
}

/// A handle that makes bells in a [`Bells`] it does not own. Made by
/// [`Bells::handle`]; `Clone`.
///
/// It does not keep the set running: once the [`Bells`] is dropped, a bell it
/// makes is never woken, and a future waiting on its listener stays pending.
#[derive(Clone)]
pub struct BellsHandle {
    shared: Arc<Shared>,
}

impl BellsHandle {
    /// A new bell in the set ([`Bells::bell`]). It forgets the bells whose both
    /// sides are gone first, as a pass does: a ring nobody waits on runs no
    /// pass, so the set stays as large as its live bells only if the make does
    /// this too.
    pub fn bell(&self) -> (Bell, Listener) {
        let state = Arc::new(State {
            rung: AtomicBool::new(false),
            waker: Mutex::new(None),
            waiting: AtomicBool::new(false),
            shut: Arc::clone(&self.shared.shut),
        });
        {
            let mut bells = self.shared.bells();
            bells.retain(|state| Arc::strong_count(state) > 1);
            bells.push(Arc::clone(&state));
        }
        (
            Bell {
                _control: ControlDrop::new(),
                state: Arc::clone(&state),
                unparker: self.shared.unparker.clone(),
            },
            Listener { state },
        )
    }
}

impl std::fmt::Debug for BellsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BellsHandle").finish_non_exhaustive()
    }
}

impl Drop for Bells {
    fn drop(&mut self) {
        self.shared.shut.store(true, Ordering::Release);
        self.shared.unparker.unpark();
        // With a tick task, it runs the last pass on its way out and the
        // `Tick`, a field, waits for it when it drops after this.
        self.tick.shut(&self.shared);
    }
}

impl std::fmt::Debug for Bells {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bells")
            .field("bells", &self.shared.bells().len())
            .finish_non_exhaustive()
    }
}

/// The ring side of a bell, held by the audio thread. Made by [`Bells::bell`].
///
/// `Clone`, `Send` and `Sync`; every clone rings the same bell. It holds an
/// `Arc` of the bell's state, so the last clone to drop frees it: that drop
/// is on the control side, never on the audio thread (a debug build panics
/// otherwise).
#[must_use]
pub struct Bell {
    _control: ControlDrop<Bell>,
    state: Arc<State>,
    unparker: Unparker,
}

impl Bell {
    /// Ring: set the bell's flag, and unpark the tick if a future is waiting on
    /// the bell. Real-time safe: a swap, a fence and a load, plus a futex wake
    /// when a future waits and the tick is parked (the wake-up, never a wait on
    /// it). It never allocates, locks, waits or touches a `Waker`.
    #[inline]
    pub fn ring(&self) {
        self.state.rung.swap(true, Ordering::Release);
        // Pairs with the wait poll's fence: the poll stored `waiting` before it,
        // so either this load sees it or the poll sees `rung`.
        fence(Ordering::SeqCst);
        if self.state.waiting.load(Ordering::Acquire) {
            self.unparker.unpark_after_fence();
        }
    }
}

impl Clone for Bell {
    fn clone(&self) -> Self {
        Self {
            _control: ControlDrop::new(),
            state: Arc::clone(&self.state),
            unparker: self.unparker.clone(),
        }
    }
}

impl std::fmt::Debug for Bell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bell").finish_non_exhaustive()
    }
}

/// The wait side of a bell, held by the control-side future. Made by
/// [`Bells::bell`]; not `Clone`, so one future waits on a bell.
#[must_use]
pub struct Listener {
    state: Arc<State>,
}

impl Listener {
    /// A future that resolves once the bell has rung since the last time one
    /// resolved. Resolving clears the ring, so two rings before it resolve it
    /// once.
    ///
    /// ```
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    /// use tutti_rt::Bells;
    ///
    /// let executor = tutti_platform::Executor::new();
    /// let bells = Bells::new(&executor).unwrap();
    /// let (bell, mut listener) = bells.bell();
    /// bell.ring();
    /// let mut cx = Context::from_waker(Waker::noop());
    /// assert_eq!(pin!(listener.rung()).poll(&mut cx), Poll::Ready(()));
    /// assert_eq!(pin!(listener.rung()).poll(&mut cx), Poll::Pending);
    /// ```
    pub fn rung(&mut self) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(|cx| self.poll_rung(cx))
    }

    /// The wait poll of the module docs.
    fn poll_rung(&self, cx: &mut Context<'_>) -> Poll<()> {
        let state = &*self.state;
        if state.rung.swap(false, Ordering::Acquire) {
            return Poll::Ready(());
        }
        if state.shut.load(Ordering::Acquire) {
            // A ring before the shut may land after the swap above.
            return Self::after_shut(state);
        }
        state.store_waker(cx.waker().clone());
        // Pairs with the fence in the ring: a ring after this fence finds
        // `waiting` set; one before it is seen here.
        fence(Ordering::SeqCst);
        if state.rung.load(Ordering::Acquire) {
            state.take_waker();
            state.rung.swap(false, Ordering::Release);
            return Poll::Ready(());
        }
        // The last pass's fence is the other half: it finds the waker stored
        // above, or `shut` is seen here and the waker is not left behind.
        if state.shut.load(Ordering::Acquire) {
            state.take_waker();
            return Self::after_shut(state);
        }
        Poll::Pending
    }

    /// A poll that sees `shut` stores no waker: `Ready` for a ring that
    /// landed before the shut (the load of `shut` acquires it), else
    /// `Pending`.
    fn after_shut(state: &State) -> Poll<()> {
        if state.rung.swap(false, Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}

/// The loom model's way in: a set with no tick task, and the tick's body one
/// step at a time.
#[cfg(loom)]
impl Bells {
    /// A set with no tick task, and the [`Ticker`] that steps it.
    pub fn stepped() -> (Self, Ticker) {
        let (shared, parker) = Self::shared();
        let ticker = Ticker {
            shared: Arc::clone(&shared),
            parker,
        };
        (
            Self {
                shared,
                tick: tick::Tick::none(),
            },
            ticker,
        )
    }

    /// Ask the stepped tick to end: its [`Ticker::tick_once`] returns `false`.
    pub fn shut(&self) {
        self.shared.shut.store(true, Ordering::Release);
        self.shared.unparker.unpark();
    }
}

/// The tick task's body, stepped by a loom model.
#[cfg(loom)]
pub struct Ticker {
    shared: Arc<Shared>,
    parker: Parker,
}

#[cfg(loom)]
impl Ticker {
    /// The shipped tick step: park until a bell has a waiter or the set is
    /// shut, then one pass. `false` once shut.
    pub fn tick_once(&mut self) -> bool {
        self.shared.tick_once(&mut self.parker)
    }
}

#[cfg(loom)]
impl std::fmt::Debug for Ticker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticker").finish_non_exhaustive()
    }
}

// Its threads and `Arc`s are std's: the loom model is `tests/bells_loom.rs`.
#[cfg(all(test, not(loom)))]
mod tests;
