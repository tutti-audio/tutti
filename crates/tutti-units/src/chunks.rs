//! [`Chunks`]: a count of transfer chunks.
//!
//! Absent on purpose:
//!
//! - `Chunks - Chunks`: an unsigned count has no safe `-`. A plain one
//!   wraps in release, and a saturating operator hides the ordering bug
//!   that produced it. The one real use, a count since a baseline reading,
//!   is named: [`saturating_sub`](Chunks::saturating_sub).
//! - `Chunks + Chunks`, `Chunks * Chunks`: no caller totals chunk counts;
//!   add `+` with its use.
//! - `Chunks + u64`: the bare operand could be any count.

unit! {
    /// A count of transfer chunks: how many chunks of a pipelined transfer
    /// something happened to.
    ///
    /// One count unit per thing counted, so a count of chunks cannot be added
    /// to a count of frames ([`Samples`](crate::Samples)) or of any other
    /// thing. Like `Samples` it is an integer that is compared, not a
    /// [`Unit`](crate::Unit): no parameter cell holds it. A bare number becomes
    /// one with [`new`](Self::new) and leaves with [`get`](Self::get); there is
    /// no `From` either way.
    pub struct Chunks(u64): Display;
}

impl Chunks {
    /// No chunks.
    pub const ZERO: Chunks = Chunks(0);

    /// Wraps a raw number of chunks.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw number of chunks.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

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
        assert_eq!(Chunks::new(7).get(), 7);
        assert_eq!(Chunks::ZERO.get(), 0);
        assert!(Chunks::new(1) < Chunks::new(2));
        assert_eq!(Chunks::new(3).to_string(), "3");
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
