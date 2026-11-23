//! The count units: [`Blocks`], [`Calls`], [`Chunks`], [`Resyncs`] and [`UndoSteps`].
//!
//! Absent on purpose:
//!
//! - `-` between two counts: an unsigned count has no safe `-`. A plain one
//!   wraps in release, and a saturating operator hides the ordering bug that
//!   produced it. The one real use, a count since a baseline reading, is
//!   named: [`Chunks::saturating_sub`].
//! - `*` between two counts: a count squared is not a count.
//! - `+` with a bare `u64`: the operand could be any count; the caller says
//!   `new(n)`.
//! - `+` on [`Chunks`] and [`Resyncs`]: no caller totals them; add it with its
//!   use.

/// Defines a count unit: the type, `ZERO`, `new` and `get`, and with `add`
/// the saturating `+`, `+=` and `Sum`.
macro_rules! count {
    ($(#[$doc:meta])* $name:ident, $what:literal $(, $add:ident)?) => {
        unit! {
            $(#[$doc])*
            ///
            /// One count unit per thing counted, so it cannot be added to a
            /// count of frames ([`Samples`](crate::Samples)) or of any other
            /// thing. Like `Samples` it is an integer that is compared, not a
            /// [`Unit`](crate::Unit): no parameter cell holds it. A bare number
            /// becomes one with [`new`](Self::new) and leaves with
            /// [`get`](Self::get); there is no `From` either way.
            pub struct $name(u64): Display;
        }

        impl $name {
            #[doc = concat!("No ", $what, ".")]
            pub const ZERO: $name = $name(0);

            #[doc = concat!("Wraps a raw number of ", $what, ".")]
            #[inline]
            #[must_use]
            pub const fn new(v: u64) -> Self {
                Self(v)
            }

            #[doc = concat!("Returns the raw number of ", $what, ".")]
            #[inline]
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        $(count!(@$add $name);)?
    };
    (@add $name:ident) => {
        /// Counts sum. Saturating: a `u64` count overflows only on a corrupt
        /// input, and a clamped total beats a wrapped one near zero.
        impl core::ops::Add for $name {
            type Output = Self;
            #[inline]
            fn add(self, rhs: Self) -> Self {
                Self(self.0.saturating_add(rhs.0))
            }
        }

        impl core::ops::AddAssign for $name {
            #[inline]
            fn add_assign(&mut self, rhs: Self) {
                self.0 = self.0.saturating_add(rhs.0);
            }
        }

        /// The total of several counts, saturating as `+` does.
        impl core::iter::Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |a, b| a + b)
            }
        }
    };
}

count! {
    /// A count of audio blocks: how many of a graph node's process calls
    /// something happened to.
    Blocks, "blocks", add
}

count! {
    /// A count of calls: how many calls something happened to.
    Calls, "calls", add
}

count! {
    /// A count of transfer chunks: how many chunks of a pipelined transfer
    /// something happened to.
    Chunks, "chunks"
}

count! {
    /// A count of re-anchors: how many times a MIDI input's clock map was set
    /// again on a reading instead of following it (a reading too far off the
    /// prediction, or one the map refused and was rebuilt on).
    Resyncs, "re-anchors"
}

count! {
    /// A count of undo steps: how many steps of an edit log something
    /// happened to (dropped when the log is over its cap).
    UndoSteps, "undo steps", add
}

impl Chunks {
    /// Returns the chunks counted since `earlier`, a reading of the same
    /// running count: `self - earlier`, or zero when `earlier` is the later
    /// one (the count was reset between the readings).
    #[inline]
    #[must_use]
    pub const fn saturating_sub(self, earlier: Chunks) -> Chunks {
        Chunks(self.0.saturating_sub(earlier.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_round_trips_through_new_and_get_and_orders() {
        assert_eq!(Blocks::new(7).get(), 7);
        assert_eq!(Calls::ZERO.get(), 0);
        assert!(Chunks::new(1) < Chunks::new(2));
        assert_eq!(Resyncs::new(3).to_string(), "3");
        assert_eq!(UndoSteps::new(2) + UndoSteps::new(1), UndoSteps::new(3));
        assert_eq!(
            UndoSteps::new(u64::MAX) + UndoSteps::new(1),
            UndoSteps::new(u64::MAX)
        );
    }

    #[test]
    fn add_saturates_rather_than_wrapping() {
        assert_eq!(Blocks::new(3) + Blocks::new(4), Blocks::new(7));
        assert_eq!(Calls::new(u64::MAX) + Calls::new(1), Calls::new(u64::MAX));
        let mut acc = Blocks::new(1);
        acc += Blocks::new(2);
        assert_eq!(acc, Blocks::new(3));
        acc += Blocks::new(u64::MAX);
        assert_eq!(acc, Blocks::new(u64::MAX));
    }

    #[test]
    fn a_sum_totals_and_saturates() {
        let none: [Calls; 0] = [];
        assert_eq!(none.into_iter().sum::<Calls>(), Calls::ZERO);
        assert_eq!(
            [1, 2, 3].map(Blocks::new).into_iter().sum::<Blocks>(),
            Blocks::new(6)
        );
        assert_eq!(
            [u64::MAX, 5].map(Blocks::new).into_iter().sum::<Blocks>(),
            Blocks::new(u64::MAX)
        );
    }

    #[test]
    fn saturating_sub_is_the_count_since_and_zero_past_a_reset() {
        assert_eq!(
            Chunks::new(10).saturating_sub(Chunks::new(4)),
            Chunks::new(6)
        );
        assert_eq!(Chunks::new(4).saturating_sub(Chunks::new(10)), Chunks::ZERO);
        assert_eq!(Chunks::new(7).saturating_sub(Chunks::ZERO), Chunks::new(7));
    }
}
