//! [`Calls`]: a count of calls.

unit! {
    /// A count of calls. How many calls something happened to: calls refused because they came from the wrong thread, round trips that panicked, snapshots that failed.
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
}
