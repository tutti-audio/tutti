//! [`Resyncs`]: a count of times a clock map re-anchored.

unit! {
    /// A count of re-anchors: how many times a MIDI input's clock map was set
    /// again on a reading instead of following it (a reading too far off the
    /// prediction, or one the map refused and was rebuilt on).
    ///
    /// One count unit per thing counted, so a count of resyncs cannot be added
    /// to a count of frames ([`Samples`](crate::Samples)) or of any other
    /// thing. Like `Samples` it is an integer that is compared, not a
    /// [`Unit`](crate::Unit): no `Param` holds it. A bare number becomes one
    /// with [`new`](Self::new) and leaves with [`get`](Self::get); there is no
    /// `From` either way.
    pub struct Resyncs(u64): Display;
}

impl Resyncs {
    /// No re-anchors.
    pub const ZERO: Resyncs = Resyncs(0);

    /// Wraps a raw number of re-anchors.
    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw number of re-anchors.
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
        assert_eq!(Resyncs::new(7).get(), 7);
        assert_eq!(Resyncs::ZERO.get(), 0);
        assert!(Resyncs::new(1) < Resyncs::new(2));
        assert_eq!(Resyncs::new(3).to_string(), "3");
    }
}
