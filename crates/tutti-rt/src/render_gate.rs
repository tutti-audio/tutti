//! [`RenderGate`]: whether a plugin's renders are admitted, and how many are
//! in flight, in one word, so the control side that opens or closes the plugin
//! has the render side's answer without a lock.
//!
//! [`RenderCount`](crate::RenderCount) counts renders for a thread that clears
//! what they read. A gate adds the lifecycle around them: an OS-level
//! initialise and uninitialise that renders must not overlap.
//!
//! # The states
//!
//! [`GateState`]: `Closed` (initial), `Opening`, `Open`, `Closing`, `Held`.
//! Renders are admitted only in `Open`.
//!
//! ```text
//! Closed --begin_open--> Opening --finish--> Open --close--> Closing --finish--> Closed
//!    |  ^                    |                 ^                |
//!    |  +------- drop -------+                 +----- drop -----+
//!    +--hold_closed--> Held --drop--> Closed
//! ```
//!
//! # The word
//!
//! A `u64`: the state in the low three bits, the number of admitted renders
//! (plus any refused `enter` not yet undone) above them. Every change is one
//! read-modify-write on the word.
//!
//! - **Render side:** [`enter`](RenderGate::enter) is a `fetch_add` of one
//!   count (`Acquire`); the state is read from the value it returned. In
//!   `Open` the render is admitted and its guard's drop is a `Release`
//!   `fetch_sub`. Otherwise `enter` undoes its add with a `Relaxed`
//!   `fetch_sub` and returns `None`, as a refused shared claim
//!   of an [`AudioThreadCell`](crate::AudioThreadCell) does. Wait-free; never locks or
//!   allocates.
//! - **Control side:** [`begin_open`](RenderGate::begin_open),
//!   [`close`](RenderGate::close) and [`hold_closed`](RenderGate::hold_closed)
//!   are compare-exchanges (`AcqRel`) of the state bits that carry the count
//!   along, retried when a render's add or undo moved it. The holder of
//!   `Opening`, `Closing` or `Held` owns the state bits, so its finish and its
//!   revert are one `fetch_add` of the difference of the two states
//!   (`Release`): they need no zero count, and keep a refused `enter`'s add that
//!   is not yet undone.
//!
//! # Why no fence
//!
//! `RenderCount` needs a `SeqCst` fence because the render's count and its read
//! of the cleared slot are two words. Here the admission and the count are one
//! word, so `close`'s compare-exchange and an `enter`'s add are ordered in that
//! word's modification order: an `enter` before the close is counted in what
//! the close waits for, one after it reads `Closing` and is refused. No
//! render is admitted after `close` has changed the state, and none that was
//! admitted before is missed.
//!
//! # Waiting
//!
//! `close` waits on the control thread until the count reads zero (`Acquire`,
//! so every admitted render's work is ordered before what the caller does
//! next). A render lasts at most one block, so it backs off:
//! a spin, then yields, then sleeps of at most a millisecond. The render's
//! drop stays one `fetch_sub`: waking a parked waiter would need an unparker
//! per gate, and a wait is rare and short, so polling costs the audio side
//! nothing and the control side a bounded latency.
//!
//! A leaked guard keeps the count above zero for good, so `close` never
//! returns: the safe direction.
//!
//! The model in `tests/render_gate_loom.rs` runs this code.

use sync::{AtomicU64, Ordering};

use crate::backoff::Backoff;

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
}

const CLOSED: u64 = 0;
const OPENING: u64 = 1;
const OPEN: u64 = 2;
const CLOSING: u64 = 3;
const HELD: u64 = 4;
/// The state bits.
const STATE: u64 = 0b111;
/// One render in the count.
const ONE: u64 = 1 << 3;

/// Where a [`RenderGate`] is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GateState {
    /// Shut; renders are refused. The initial state.
    Closed,
    /// Being opened ([`RenderGate::begin_open`]); renders are refused.
    Opening,
    /// Renders are admitted.
    Open,
    /// Being closed ([`RenderGate::close`]); renders are refused.
    Closing,
    /// Shut and kept so ([`RenderGate::hold_closed`]); renders and opens are refused.
    Held,
}

/// A plugin's render admission and lifecycle. See the module docs
/// (`src/render_gate.rs`).
#[derive(Debug)]
pub struct RenderGate {
    word: AtomicU64,
}

impl Default for RenderGate {
    fn default() -> Self {
        Self::new()
    }
}

/// A render admitted until it drops ([`RenderGate::enter`]).
#[derive(Debug)]
#[must_use = "the render is counted in flight only while the guard is held"]
pub struct GateGuard<'a>(&'a RenderGate);

/// The state bits a control-side guard owns: on drop, they move from `from`
/// to `to`.
#[derive(Debug)]
struct Owned<'a> {
    gate: &'a RenderGate,
    from: u64,
    to: u64,
}

/// The gate while it is being opened ([`RenderGate::begin_open`]); dropped
/// unfinished, it goes back to `Closed`.
#[derive(Debug)]
#[must_use = "dropping an unfinished Opening closes the gate again"]
pub struct Opening<'a>(Owned<'a>);

/// The gate once its renders have drained ([`RenderGate::close`]); dropped
/// unfinished, it goes back to `Open`.
#[derive(Debug)]
#[must_use = "dropping an unfinished Closing opens the gate again"]
pub struct Closing<'a>(Owned<'a>);

/// The gate kept closed ([`RenderGate::hold_closed`]) until it drops.
#[derive(Debug)]
#[must_use = "the gate is held closed only while the guard is held"]
pub struct HeldClosed<'a> {
    _owned: Owned<'a>,
}

impl RenderGate {
    /// A closed gate with no render in flight.
    #[must_use]
    pub fn new() -> Self {
        Self {
            word: AtomicU64::new(CLOSED),
        }
    }

    /// Admits a render until the guard drops, or `None` unless the gate is
    /// `Open`. Wait-free; never locks or allocates.
    pub fn enter(&self) -> Option<GateGuard<'_>> {
        let prev = self.word.fetch_add(ONE, Ordering::Acquire);
        if prev & STATE == OPEN {
            return Some(GateGuard(self));
        }
        self.word.fetch_sub(ONE, Ordering::Relaxed);
        None
    }

    /// `Closed` to `Opening`; `None` in any other state (including `Held`).
    /// Finish it once the plugin is initialised.
    pub fn begin_open(&self) -> Option<Opening<'_>> {
        self.acquire(CLOSED, OPENING)
            .then(|| Opening(Owned::new(self, OPENING, CLOSED)))
    }

    /// `Open` to `Closing` at once, so every later [`enter`](Self::enter) is
    /// refused, then waits until the renders already admitted have dropped.
    /// Returns `None` only when the gate is not `Open`; renders never refuse
    /// it. Control side: it waits (module docs, "Waiting"). Uninitialise the
    /// plugin after it returns, then finish. Dropped unfinished, the guard
    /// reopens the gate, so leave the plugin renderable before dropping it.
    /// Called from a render, or while its own thread holds a [`GateGuard`], it
    /// never returns: it waits for that render.
    pub fn close(&self) -> Option<Closing<'_>> {
        if !self.acquire(OPEN, CLOSING) {
            return None;
        }
        let closing = Closing(Owned::new(self, CLOSING, OPEN));
        let mut backoff = Backoff::new();
        while self.word.load(Ordering::Acquire) >> 3 != 0 {
            backoff.snooze();
        }
        Some(closing)
    }

    /// `Closed` to `Held`: [`begin_open`](Self::begin_open) refuses until the
    /// guard drops. `None` in any other state.
    pub fn hold_closed(&self) -> Option<HeldClosed<'_>> {
        self.acquire(CLOSED, HELD).then(|| HeldClosed {
            _owned: Owned::new(self, HELD, CLOSED),
        })
    }

    /// The state now; it can have changed by the time it is read.
    #[must_use]
    pub fn state(&self) -> GateState {
        match self.word.load(Ordering::Acquire) & STATE {
            CLOSED => GateState::Closed,
            OPENING => GateState::Opening,
            OPEN => GateState::Open,
            CLOSING => GateState::Closing,
            _ => GateState::Held,
        }
    }

    /// Moves the state bits from `from` to `to`, keeping the count; `false`
    /// when the state is not `from`.
    fn acquire(&self, from: u64, to: u64) -> bool {
        let mut cur = self.word.load(Ordering::Relaxed);
        loop {
            if cur & STATE != from {
                return false;
            }
            match self.word.compare_exchange(
                cur,
                cur - from + to,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.0.word.fetch_sub(ONE, Ordering::Release);
    }
}

impl<'a> Owned<'a> {
    fn new(gate: &'a RenderGate, from: u64, to: u64) -> Self {
        Self { gate, from, to }
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        // The holder owns the state bits, so the difference lands them on
        // `to` whatever the count is doing.
        let prev = self
            .gate
            .word
            .fetch_add(self.to.wrapping_sub(self.from), Ordering::Release);
        debug_assert_eq!(prev & STATE, self.from, "only the holder moves the state");
    }
}

impl Opening<'_> {
    /// The plugin is initialised: the gate goes `Open`.
    pub fn finish(mut self) {
        self.0.to = OPEN;
    }
}

impl Closing<'_> {
    /// The plugin is uninitialised: the gate goes `Closed`.
    pub fn finish(mut self) {
        self.0.to = CLOSED;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> RenderGate {
        let gate = RenderGate::new();
        gate.begin_open().expect("closed").finish();
        gate
    }

    /// **A render is admitted only while Open**: Closed, Opening, Closing and Held refuse.
    #[test]
    fn renders_are_admitted_only_when_open() {
        let gate = RenderGate::new();
        assert!(gate.enter().is_none(), "closed");
        let opening = gate.begin_open().expect("closed");
        assert_eq!(gate.state(), GateState::Opening);
        assert!(gate.enter().is_none(), "opening");
        opening.finish();
        assert_eq!(gate.state(), GateState::Open);
        drop(gate.enter().expect("open"));
        let closing = gate.close().expect("open");
        assert_eq!(gate.state(), GateState::Closing);
        assert!(gate.enter().is_none(), "closing");
        closing.finish();
        assert_eq!(gate.state(), GateState::Closed);
        let held = gate.hold_closed().expect("closed");
        assert!(gate.enter().is_none(), "held");
        drop(held);
    }

    /// **A failed initialise reverts**: an unfinished `Opening` leaves the gate Closed.
    #[test]
    fn an_unfinished_opening_reverts_to_closed() {
        let gate = RenderGate::new();
        drop(gate.begin_open().expect("closed"));
        assert_eq!(gate.state(), GateState::Closed);
        assert!(gate.begin_open().is_some());
    }

    /// **A failed uninitialise reverts**: an unfinished `Closing` leaves the gate Open.
    #[test]
    fn an_unfinished_closing_reverts_to_open() {
        let gate = open();
        drop(gate.close().expect("open"));
        assert_eq!(gate.state(), GateState::Open);
        assert!(gate.enter().is_some());
    }

    /// **A transition from the wrong state is refused**, and refusals change nothing.
    #[test]
    fn transitions_from_the_wrong_state_are_refused() {
        let gate = RenderGate::new();
        assert!(gate.close().is_none(), "closed");
        let opening = gate.begin_open().expect("closed");
        assert!(gate.begin_open().is_none());
        assert!(gate.hold_closed().is_none());
        assert!(gate.close().is_none());
        opening.finish();
        assert!(gate.begin_open().is_none(), "open");
        assert!(gate.hold_closed().is_none(), "open");
        assert_eq!(gate.state(), GateState::Open);
    }

    /// **A held gate refuses to open** until the hold drops.
    #[test]
    fn a_held_gate_refuses_begin_open() {
        let gate = RenderGate::new();
        let held = gate.hold_closed().expect("closed");
        assert_eq!(gate.state(), GateState::Held);
        assert!(gate.begin_open().is_none());
        assert!(gate.hold_closed().is_none());
        drop(held);
        assert_eq!(gate.state(), GateState::Closed);
        assert!(gate.begin_open().is_some());
    }

    /// **`close` returns only once the admitted renders have dropped**, and
    /// the count never leaks a refused enter.
    #[test]
    fn close_waits_for_the_admitted_render() {
        let gate = open();
        let guard = gate.enter().expect("open");
        let waited = std::thread::scope(|s| {
            let closer = s.spawn(|| gate.close().map(|c| c.finish()));
            while gate.state() != GateState::Closing {
                std::thread::yield_now();
            }
            assert!(gate.enter().is_none());
            assert!(!closer.is_finished(), "a render is still in flight");
            drop(guard);
            closer.join().expect("closer")
        });
        assert!(waited.is_some());
        assert_eq!(gate.state(), GateState::Closed);
        gate.begin_open().expect("count is zero again").finish();
        assert!(
            gate.close().is_some(),
            "no refused enter left a count behind"
        );
    }
}
