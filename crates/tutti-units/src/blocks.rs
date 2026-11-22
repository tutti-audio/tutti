//! [`Blocks`]: a count of audio blocks.

unit! {
    /// A count of audio blocks. How many audio blocks something happened to: a plugin bridge's blocks that found no lent buffer, failed, or wrote a non-finite sample.
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
}
