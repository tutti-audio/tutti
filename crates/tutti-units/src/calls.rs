//! [`Calls`]: a count of calls.
//!
//! Absent on purpose:
//!
//! - `Calls - Calls`: an unsigned count has no safe `-`. A plain one wraps
//!   in release, and a saturating one hides the ordering bug that produced
//!   it.
//! - `Calls * Calls`: a count squared is not a count.
//! - `Calls + u64`: the bare operand could be any count; the caller says
//!   `Calls::new(n)`.

unit! {
    /// A count of calls: how many calls something happened to.
    ///
    /// One count unit per thing counted, so a count of calls cannot be added
    /// to a count of frames ([`Samples`](crate::Samples)) or of any other
    /// thing. Like `Samples` it is an integer that is compared, not a
    /// [`Unit`](crate::Unit): no parameter cell holds it. A bare number becomes
    /// one with [`new`](Self::new) and leaves with [`get`](Self::get); there is
    /// no `From` either way.
    pub struct Calls(u64): Display;
}

impl Calls {
    /// No calls.
    pub const ZERO: Calls = Calls(0);

    /// Wraps a raw number of calls.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw number of calls.
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

/// Calls counts sum. Saturating: a `u64` count overflows only on a corrupt
/// input, and a clamped total beats a wrapped one near zero.
impl core::ops::Add for Calls {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::AddAssign for Calls {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_round_trips_through_new_and_get_and_orders() {
        assert_eq!(Calls::new(7).get(), 7);
        assert_eq!(Calls::ZERO.get(), 0);
        assert!(Calls::new(1) < Calls::new(2));
        assert_eq!(Calls::new(3).to_string(), "3");
    }

    #[test]
    fn add_saturates_rather_than_wrapping() {
        assert_eq!(Calls::new(3) + Calls::new(4), Calls::new(7));
        assert_eq!(Calls::new(u64::MAX) + Calls::new(1), Calls::new(u64::MAX));
        let mut acc = Calls::new(1);
        acc += Calls::new(2);
        assert_eq!(acc, Calls::new(3));
        acc += Calls::new(u64::MAX);
        assert_eq!(acc, Calls::new(u64::MAX));
    }
}
