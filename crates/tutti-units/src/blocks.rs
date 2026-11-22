//! [`Blocks`]: a count of audio blocks.
//!
//! Absent on purpose:
//!
//! - `Blocks - Blocks`: an unsigned count has no safe `-`. A plain one wraps
//!   in release, and a saturating one hides the ordering bug that produced
//!   it.
//! - `Blocks * Blocks`: a count squared is not a count.
//! - `Blocks + u64`: the bare operand could be any count; the caller says
//!   `Blocks::new(n)`.

unit! {
    /// A count of audio blocks: how many of a graph node's process calls
    /// something happened to.
    ///
    /// One count unit per thing counted, so a count of blocks cannot be added
    /// to a count of frames ([`Samples`](crate::Samples)) or of any other
    /// thing. Like `Samples` it is an integer that is compared, not a
    /// [`Unit`](crate::Unit): no parameter cell holds it. A bare number becomes
    /// one with [`new`](Self::new) and leaves with [`get`](Self::get); there is
    /// no `From` either way.
    pub struct Blocks(u64): Display;
}

impl Blocks {
    /// No blocks.
    pub const ZERO: Blocks = Blocks(0);

    /// Wraps a raw number of blocks.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw number of blocks.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

// ── Operators ───────────────────────────────────────────────────────────────
//
// Written out rather than named as `unit!` abilities: a count adds by the
// saturating integer rule, which the float abilities do not state. What is
// absent is listed in the module docs.

/// Blocks counts sum. Saturating: a `u64` count overflows only on a corrupt
/// input, and a clamped total beats a wrapped one near zero.
impl core::ops::Add for Blocks {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::AddAssign for Blocks {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

/// The total of several counts, saturating as `+` does.
impl core::iter::Sum for Blocks {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, |a, b| a + b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_round_trips_through_new_and_get_and_orders() {
        assert_eq!(Blocks::new(7).get(), 7);
        assert_eq!(Blocks::ZERO.get(), 0);
        assert!(Blocks::new(1) < Blocks::new(2));
        assert_eq!(Blocks::new(3).to_string(), "3");
    }

    #[test]
    fn add_saturates_rather_than_wrapping() {
        assert_eq!(Blocks::new(3) + Blocks::new(4), Blocks::new(7));
        assert_eq!(
            Blocks::new(u64::MAX) + Blocks::new(1),
            Blocks::new(u64::MAX)
        );
        let mut acc = Blocks::new(1);
        acc += Blocks::new(2);
        assert_eq!(acc, Blocks::new(3));
        acc += Blocks::new(u64::MAX);
        assert_eq!(acc, Blocks::new(u64::MAX));
    }

    #[test]
    fn a_sum_totals_and_saturates() {
        let none: [Blocks; 0] = [];
        assert_eq!(none.into_iter().sum::<Blocks>(), Blocks::ZERO);
        assert_eq!(
            [1, 2, 3].map(Blocks::new).into_iter().sum::<Blocks>(),
            Blocks::new(6)
        );
        assert_eq!(
            [u64::MAX, 5].map(Blocks::new).into_iter().sum::<Blocks>(),
            Blocks::new(u64::MAX)
        );
    }
}
