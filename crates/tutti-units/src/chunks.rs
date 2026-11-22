//! [`Chunks`]: a count of transfer chunks.

unit! {
    /// A count of transfer chunks. How many chunks of a pipelined transfer something happened to: a plugin-server pipeline's abandoned, late or answered chunks.
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
}
