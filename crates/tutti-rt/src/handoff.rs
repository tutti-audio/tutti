//! [`handoff`]: one value that moves between a control thread and the audio
//! thread, at a block boundary, without a lock: a plugin's Processor lent to
//! the graph and recalled for a rate change.
//!
//! The control end, [`Handoff`], **lends** a value, **recalls** it, and **takes**
//! it back once returned. The audio end, [`Rt`], holds the value it was lent
//! and, between blocks, [`poll`](Rt::poll)s: it takes a value on offer and
//! hands its own back when a recall is raised, after running the caller's
//! last step on it (a Processor's `stop`). Neither end waits on the other. A
//! value on its way is one move into a slot; until the other side polls,
//! the audio end renders without it and the control end keeps asking.
//!
//! # The protocol
//!
//! One slot and one state word. The word holds who owns the slot (empty, being
//! written by either side, offered to the audio end, returned to the control
//! end) and a recall bit.
//!
//! - A side writes the slot only after a compare-exchange took it from empty
//!   to its own busy state (`Acquire`), and publishes with a `Release`
//!   read-modify-write that names the reader. An offer is moved out only
//!   after a compare-exchange took it from offered to the taker's busy state,
//!   because two sides may want it (the audio end, and a recall withdrawing
//!   it); a returned value has one taker, so an `Acquire` load suffices. The
//!   taker then empties the slot with a `Release` read-modify-write, so the
//!   next writer's claim is ordered after the move.
//! - Every change of the slot field is a read-modify-write that keeps every
//!   other bit, so a recall raised meanwhile is never lost, nor the marks the
//!   audio end leaves as it goes (below).
//! - A recall of a value still on offer cancels the offer at once (the control
//!   end takes it back without the audio end); one already taken raises the
//!   bit, which the audio end clears when it returns the value. Only the audio
//!   end clears the bit: when it returns the value, or at a poll where it holds
//!   none (a stale recall). A lend is refused while the bit is raised, so a
//!   recall is never erased while the value it asks for still runs.
//!
//! The ends are `&mut self` and not `Clone`, so each side is one thread at a
//! time. The protocol does not trust the caller to lend only what the audio
//! end lacks: a lend while a value is still in the slot is refused (it comes
//! back in the `Err`), and the audio end returns into an empty slot only. No
//! interleaving hands out a value twice or drops one in the slot; a value in
//! the slot when both ends are gone drops with the last of them.
//!
//! # The audio end going away
//!
//! The audio end never destroys the value it was lent. Dropped while it holds
//! one, it hands the value back as it is (`last` is not run: the control end
//! does what a returned value needs), and the control end's
//! [`take`](Handoff::take) yields it as [`Handback::Dropped`]. The slot may be busy or on offer at that
//! moment and a drop cannot come back later, so the value goes to a second
//! cell, the *orphan*, which only the dropping end writes, once, and only the
//! control end reads, after the `Acquire` load that shows it written. The same
//! drop marks the audio end gone: a later [`lend`](Handoff::lend) is refused,
//! since nothing would take the offer, and an offer already in the slot is
//! withdrawn by [`recall`](Handoff::recall) as any other. An orphan nobody took
//! drops with the last end. An audio end that is leaked returns nothing: the
//! control end must not wait for it.
//!
//! The model in `tests/handoff_loom.rs` runs this code.

use std::mem::MaybeUninit;

use crate::ControlDrop;

use crate::sync::{Arc, AtomicU32, Ordering, UnsafeCell};

/// The slot field of the state word.
const SLOT: u32 = 0b111;
/// Nothing in the slot; either side may claim it.
const EMPTY: u32 = 0;
/// The control end is writing the slot, or moving a withdrawn offer out.
const MAIN_BUSY: u32 = 1;
/// A value on offer to the audio end.
const OFFERED: u32 = 2;
/// The audio end is writing the slot, or moving an offer out.
const RT_BUSY: u32 = 3;
/// A value returned to the control end.
const RETURNED: u32 = 4;
/// The control end asked for the audio end's value back.
const RECALL: u32 = 0b1000;
/// The audio end was dropped. Set once, by its drop; never cleared.
const GONE: u32 = 0b1_0000;
/// The orphan cell holds the value the audio end held when it was dropped. Set
/// by that drop, cleared by the control end when it takes the value.
const ORPHAN: u32 = 0b10_0000;

struct Shared<T> {
    state: AtomicU32,
    slot: UnsafeCell<MaybeUninit<T>>,
    /// What the audio end held when it was dropped, while [`ORPHAN`] is set.
    orphan: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the slot is reached only by the side whose compare-exchange or
// `Acquire` load the state word names as its owner (module docs), so no two
// threads touch it at once, and a value crosses threads only by moving: `T:
// Send` is all that needs. The orphan cell is written by the audio end's drop
// alone, before it sets `ORPHAN`, and read by the control end alone, after it
// saw the bit.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        // `&mut self`: both ends are gone, so nothing else reads the word.
        let held = matches!(
            self.state.load(Ordering::Acquire) & SLOT,
            OFFERED | RETURNED
        );
        if held {
            self.slot.with_mut(|slot| {
                // SAFETY: an offered or returned slot holds a value its writer
                // initialized and no reader took (a reader empties the field
                // after it moves the value out).
                unsafe { (*slot).assume_init_drop() }
            });
        }
        if self.state.load(Ordering::Acquire) & ORPHAN != 0 {
            self.orphan.with_mut(|orphan| {
                // SAFETY: the bit is set only after the audio end's drop wrote
                // the cell, and cleared by the control end when it moved the
                // value out; set here, the value is initialized and untaken.
                unsafe { (*orphan).assume_init_drop() }
            });
        }
    }
}

impl<T> Shared<T> {
    /// Claims an empty slot for writing as `busy`, keeping every other bit.
    /// `false` if the slot is not empty, or, for a `lend`, if a recall is raised
    /// or the audio end is gone. The refusal and the claim are one
    /// compare-exchange of the whole word, so an audio end that goes between
    /// the two is seen.
    fn claim(&self, busy: u32, lend: bool) -> bool {
        let mut seen = self.state.load(Ordering::Relaxed);
        loop {
            if seen & SLOT != EMPTY || (lend && seen & (RECALL | GONE) != 0) {
                return false;
            }
            match self.state.compare_exchange_weak(
                seen,
                (seen & !SLOT) | busy,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    /// Claims an offered slot as `busy`, to move its value out, keeping every
    /// other bit. `false` if nothing is on offer (or the other side claimed
    /// it first).
    fn take_offer(&self, busy: u32) -> bool {
        let mut seen = self.state.load(Ordering::Relaxed);
        while seen & SLOT == OFFERED {
            match self.state.compare_exchange_weak(
                seen,
                (seen & !SLOT) | busy,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
        false
    }

    /// Sets the slot field to `to`, keeping every other bit, or clearing the
    /// recall with `clear_recall`: one read-modify-write, so the other side
    /// never sees the field between this side's states. `Release` (and
    /// `Acquire`, for the bits it keeps).
    fn set_slot(&self, to: u32, clear_recall: bool) {
        let keep = if clear_recall {
            !(SLOT | RECALL)
        } else {
            !SLOT
        };
        let mut seen = self.state.load(Ordering::Relaxed);
        while let Err(now) = self.state.compare_exchange_weak(
            seen,
            (seen & keep) | to,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            seen = now;
        }
    }

    /// Clears a raised recall unless an offer is in the slot (the audio end
    /// takes that first). One read-modify-write; it publishes nothing, so
    /// `Relaxed`.
    fn clear_stale_recall(&self) {
        let mut seen = self.state.load(Ordering::Relaxed);
        while seen & RECALL != 0 && seen & SLOT != OFFERED {
            match self.state.compare_exchange_weak(
                seen,
                seen & !RECALL,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => seen = now,
            }
        }
    }

    /// Moves `value` into a slot this side claimed.
    fn write(&self, value: T) {
        self.slot.with_mut(|slot| {
            // SAFETY: the caller claimed the slot (it is busy as this side),
            // so no other thread reaches it, and it holds no value (a claim
            // succeeds only from empty).
            unsafe { (*slot).write(value) };
        });
    }

    /// Moves the value out of a slot this side owns.
    fn read(&self) -> T {
        self.slot.with_mut(|slot| {
            // SAFETY: the caller claimed an offered slot with an `Acquire`
            // compare-exchange, or saw it returned to it (it has no other
            // taker) with an `Acquire` load, so the writer's `Release` publish
            // happened before, the value is initialized, and this side is the
            // only one reaching it until it empties the field.
            unsafe { (*slot).assume_init_read() }
        })
    }
}

/// How a value came back to the control end ([`Handoff::take`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Handback<T> {
    /// The audio end answered a recall at a poll: `last` was run on the value.
    Recalled(T),
    /// The audio end was dropped while it held the value: it is as it was,
    /// and `last` was not run on it.
    Dropped(T),
}

impl<T> Handback<T> {
    /// The value, whichever way it came back.
    pub fn into_inner(self) -> T {
        match self {
            Self::Recalled(value) | Self::Dropped(value) => value,
        }
    }
}

/// The control end of a [`handoff`]: lends the value, recalls it and takes it
/// back.
pub struct Handoff<T> {
    shared: Arc<Shared<T>>,
}

/// The audio end of a [`handoff`]: holds the value it was lent and trades it
/// at [`poll`](Self::poll), between blocks.
///
/// It is dropped on the control side (a graph node's retirement path does),
/// and a debug build panics when this end drops inside an audio block. A value
/// it holds then goes back to the control end as it is, which
/// [`Handoff::take`] yields: this end never destroys what it was lent. The
/// holder that wants the value destroyed takes it out first
/// ([`take_held`](Self::take_held)).
pub struct Rt<T> {
    _control: ControlDrop<Self>,
    shared: Arc<Shared<T>>,
    held: Option<T>,
}

/// A new handoff with nothing lent. The one allocation is here, on the control
/// side; neither end allocates after it.
#[must_use]
pub fn handoff<T: Send>() -> (Handoff<T>, Rt<T>) {
    let shared = Arc::new(Shared {
        state: AtomicU32::new(EMPTY),
        slot: UnsafeCell::new(MaybeUninit::uninit()),
        orphan: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (
        Handoff {
            shared: Arc::clone(&shared),
        },
        Rt {
            _control: ControlDrop::new(),
            shared,
            held: None,
        },
    )
}

impl<T> Handoff<T> {
    /// Offers `value` to the audio end, which takes it at its next poll.
    ///
    /// # Errors
    ///
    /// `value`, back, when the slot is not empty (an earlier offer the audio
    /// end has not taken, or a returned value not yet [`take`](Self::take)n),
    /// while a recall is raised, or once the audio end is gone
    /// ([`borrower_gone`](Self::borrower_gone)): nothing would take the offer.
    /// A lend never clears a recall: the audio end
    /// may still hold the value it was for, and a lend that cleared it would
    /// leave that value running with nothing asking for it. The audio end
    /// clears a recall it has nothing for at its next poll, so a refused lend
    /// is retried on a later tick.
    pub fn lend(&mut self, value: T) -> Result<(), T> {
        if !self.shared.claim(MAIN_BUSY, true) {
            return Err(value);
        }
        self.shared.write(value);
        self.shared.set_slot(OFFERED, false);
        Ok(())
    }

    /// Asks for the value back. A value still on offer is withdrawn and
    /// returned here at once; one the audio end holds comes back through
    /// [`take`](Self::take) after its next poll. `None` in that case. Once the
    /// audio end is gone a recall with nothing on offer stays raised, which
    /// changes nothing: a lend is refused for the end being gone.
    pub fn recall(&mut self) -> Option<T> {
        if self.shared.take_offer(MAIN_BUSY) {
            let value = self.shared.read();
            self.shared.set_slot(EMPTY, false);
            return Some(value);
        }
        self.shared.state.fetch_or(RECALL, Ordering::Release);
        None
    }

    /// The value the audio end returned, if it has, and how: at a poll that
    /// answered a recall, with `last` run on it, or by being dropped while it
    /// held one, with the value as it was. The two cannot both be waiting: an
    /// audio end that returned its value holds none to leave behind. One that
    /// answered a recall and was then dropped yields
    /// [`Recalled`](Handback::Recalled), with
    /// [`borrower_gone`](Self::borrower_gone) true.
    pub fn take(&mut self) -> Option<Handback<T>> {
        let state = self.shared.state.load(Ordering::Acquire);
        if state & SLOT == RETURNED {
            let value = self.shared.read();
            self.shared.set_slot(EMPTY, false);
            return Some(Handback::Recalled(value));
        }
        if state & ORPHAN != 0 {
            let value = self.shared.orphan.with_mut(|orphan| {
                // SAFETY: the audio end's drop wrote the cell and then set the
                // bit with `Release`, which the `Acquire` load above saw; that
                // end is gone and wrote it once, and this end is the only
                // reader, so the value is initialized and no one else reaches
                // it. The bit is cleared below, so it is moved out once.
                unsafe { (*orphan).assume_init_read() }
            });
            self.shared.state.fetch_and(!ORPHAN, Ordering::Release);
            return Some(Handback::Dropped(value));
        }
        None
    }

    /// Whether the audio end was dropped. A lend is refused from then on, and
    /// what that end held, if anything, is [`take`](Self::take)n.
    #[must_use]
    pub fn borrower_gone(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) & GONE != 0
    }
}

impl<T> Rt<T> {
    /// Between blocks: takes a value on offer if this end holds none, and
    /// returns the one it holds if a recall is raised, calling `last` on it
    /// first (a Processor's `stop`). Then the value this end holds, if any.
    ///
    /// Wait-free: a few atomic operations and at most one move each way. It
    /// does not allocate, and drops nothing (a value leaves only into the
    /// slot). `last` must not panic: it runs with the slot claimed, which an
    /// unwind would leave claimed, and the value would drop in it.
    pub fn poll(&mut self, last: impl FnOnce(&mut T)) -> Option<&mut T> {
        let state = self.shared.state.load(Ordering::Acquire);
        if self.held.is_none() && state & SLOT == OFFERED {
            // Claimed, not just seen: a recall may be withdrawing the same
            // offer, and only one of the two may move it out.
            if self.shared.take_offer(RT_BUSY) {
                self.held = Some(self.shared.read());
                self.shared.set_slot(EMPTY, false);
            }
        } else if state & RECALL != 0 {
            if self.held.is_some() {
                if self.shared.claim(RT_BUSY, false)
                    && let Some(mut value) = self.held.take()
                {
                    last(&mut value);
                    self.shared.write(value);
                    self.shared.set_slot(RETURNED, true);
                }
            } else {
                // A recall with nothing here to answer it (raised when nothing
                // was lent, or again after a return): cleared, so the next lend
                // is not refused for it. Only here, where it is known that no
                // value is held.
                self.shared.clear_stale_recall();
            }
        }
        self.held.as_mut()
    }

    /// The value this end holds, without trading.
    pub fn held(&mut self) -> Option<&mut T> {
        self.held.as_mut()
    }

    /// Takes the value this end holds out of the handoff: it does not go back
    /// to the control end, and dropping it destroys it. Control side.
    pub fn take_held(&mut self) -> Option<T> {
        self.held.take()
    }
}

impl<T> Drop for Rt<T> {
    fn drop(&mut self) {
        let mut marks = GONE;
        if let Some(value) = self.held.take() {
            self.shared.orphan.with_mut(|orphan| {
                // SAFETY: this drop is the cell's only writer and runs once,
                // and the control end reads it only after it saw `ORPHAN`,
                // which is set below: no other thread reaches the cell here,
                // and it holds no value.
                unsafe { (*orphan).write(value) };
            });
            marks |= ORPHAN;
        }
        // `Release`: publishes the orphan to the control end's `Acquire` load.
        self.shared.state.fetch_or(marks, Ordering::Release);
    }
}

impl<T> std::fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handoff").finish_non_exhaustive()
    }
}

impl<T> std::fmt::Debug for Rt<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rt")
            .field("holds", &self.held.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
