//! [`LastOut`]: a value two parties share, which the last of them to leave
//! takes.
//!
//! [`leave`](LastOut::leave) is one `fetch_or` of the party's bit, `AcqRel`.
//! The call that sees the other party's bit already set is the later of the
//! two in the word's modification order, so exactly one `leave` takes the
//! value. Its `Acquire` reads from the other's `Release`, so everything the
//! other party did before leaving is visible to the taker. A party's second
//! `leave` sees its own bit set: it panics in a debug build, and returns
//! `None` in release, never taking twice. A `LastOut` dropped before both left
//! drops the value with it.
//!
//! Wait-free; neither party locks or allocates (the value's drop, run by the
//! taker, is the taker's).
//!
//! The model in `tests/last_out_loom.rs` runs this code.

use crate::sync::{AtomicU8, Ordering, UnsafeCell};

/// One of the two parties of a [`LastOut`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Party {
    /// The first party.
    A,
    /// The second party.
    B,
}

impl Party {
    fn bit(self) -> u8 {
        match self {
            Party::A => 0b01,
            Party::B => 0b10,
        }
    }

    fn other(self) -> Party {
        match self {
            Party::A => Party::B,
            Party::B => Party::A,
        }
    }
}

/// A value the last of two parties to [`leave`](Self::leave) takes. See the
/// module docs (`src/last_out.rs`).
pub struct LastOut<T> {
    left: AtomicU8,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: `value` is touched by `leave` only after the `fetch_or` that proves
// the caller is the single later party (see `leave`), and by the field's own
// drop, which has exclusive access; so no two threads access it at once, and
// the value moves to one thread, which needs `T: Send`.
unsafe impl<T: Send> Sync for LastOut<T> {}

impl<T> LastOut<T> {
    /// Holds `value` for the two parties.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            left: AtomicU8::new(0),
            value: UnsafeCell::new(Some(value)),
        }
    }

    /// `party` leaves: the value if the other party has already left, else
    /// `None`. A second `leave` by the same party panics in a debug build and
    /// returns `None` in release.
    ///
    /// # Panics
    ///
    /// In a debug build, if `party` has already left.
    pub fn leave(&self, party: Party) -> Option<T> {
        let prev = self.left.fetch_or(party.bit(), Ordering::AcqRel);
        debug_assert!(prev & party.bit() == 0, "{party:?} left twice");
        if prev & party.bit() != 0 || prev & party.other().bit() == 0 {
            return None;
        }
        // SAFETY: this call set `party`'s bit for the first time and saw the
        // other's set, so it is the later of the two `fetch_or`s: the only
        // call that reaches here (a repeat sees its own bit and returned
        // above), and the other party has finished with the cell. The
        // `Acquire` of that `fetch_or` orders the other's `Release` before this.
        self.value.with_mut(|value| unsafe { (*value).take() })
    }
}

impl<T> std::fmt::Debug for LastOut<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LastOut")
            .field("left", &self.left.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// **The second to leave takes the value, whichever party it is.**
    #[test]
    fn the_last_party_to_leave_takes_the_value() {
        for (first, second) in [(Party::A, Party::B), (Party::B, Party::A)] {
            let out = LastOut::new(7u32);
            assert_eq!(out.leave(first), None);
            assert_eq!(out.leave(second), Some(7));
        }
    }

    /// **A value is dropped with the `LastOut` unless it was taken.**
    #[test]
    fn an_untaken_value_drops_with_the_last_out() {
        let value = Arc::new(());
        let out = LastOut::new(Arc::clone(&value));
        out.leave(Party::A);
        assert_eq!(Arc::strong_count(&value), 2);
        drop(out);
        assert_eq!(Arc::strong_count(&value), 1);
    }

    /// **A party that leaves twice never takes the value twice** (release builds).
    #[test]
    #[cfg(not(debug_assertions))]
    fn a_repeated_leave_takes_nothing() {
        let out = LastOut::new(1u8);
        out.leave(Party::A);
        assert_eq!(out.leave(Party::A), None);
        assert_eq!(out.leave(Party::B), Some(1));
        assert_eq!(out.leave(Party::B), None);
    }

    /// **A repeated leave panics in a debug build.**
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "left twice")]
    fn a_repeated_leave_panics_in_debug() {
        let out = LastOut::new(1u8);
        out.leave(Party::A);
        out.leave(Party::A);
    }
}
