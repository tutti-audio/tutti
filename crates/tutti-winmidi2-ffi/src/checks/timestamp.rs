//! The service's timestamps: ticks of its clock, `MidiClock.TimestampFrequency` per
//! second, in nanoseconds and back.
//!
//! The ticks are `QueryPerformanceCounter`'s, which is also what the OS monotonic clock
//! `tutti_platform::monotonic_ns` reads, so a tick count is a point on that clock and
//! the two conversions are the whole mapping.

use std::num::NonZeroU64;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Ticks per second of the service's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frequency(NonZeroU64);

impl Frequency {
    /// The frequency `ticks_per_second`; `None` for zero, which no clock has.
    pub(crate) fn new(ticks_per_second: u64) -> Option<Self> {
        NonZeroU64::new(ticks_per_second).map(Self)
    }

    /// The nanoseconds `ticks` ticks last, rounded down; `None` past an `i64`.
    pub(crate) fn nanos(self, ticks: u64) -> Option<i64> {
        let nanos = u128::from(ticks) * NANOS_PER_SECOND / u128::from(self.0.get());
        i64::try_from(nanos).ok()
    }

    /// The fewest ticks that last at least `nanos` nanoseconds, so that
    /// `nanos(ticks(n)) >= n`: a message stamped with them is never stamped before `n`.
    /// `None` for a negative time and past a `u64`.
    pub(crate) fn ticks(self, nanos: i64) -> Option<u64> {
        let nanos = u128::try_from(nanos).ok()?;
        let ticks = (nanos * u128::from(self.0.get())).div_ceil(NANOS_PER_SECOND);
        u64::try_from(ticks).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MHZ: u64 = 10_000_000;

    fn frequency(ticks_per_second: u64) -> Frequency {
        Frequency::new(ticks_per_second).expect("a nonzero frequency")
    }

    /// **A tick lasts one over the frequency**: a second of ticks is a second, and a tick at
    /// 10 MHz is 100 ns.
    #[test]
    fn a_tick_lasts_one_over_the_frequency() {
        let f = frequency(TEN_MHZ);
        assert_eq!(f.nanos(TEN_MHZ), Some(1_000_000_000));
        assert_eq!(f.nanos(1), Some(100));
        assert_eq!(f.ticks(1_000_000_000), Some(TEN_MHZ));
        assert_eq!(f.ticks(100), Some(1));
    }

    /// **Nanoseconds to ticks rounds up and ticks to nanoseconds rounds down**: at 3 Hz a
    /// tick is 333,333,333.33 ns.
    #[test]
    fn the_roundings_never_stamp_a_message_early() {
        let f = frequency(3);
        assert_eq!(f.nanos(1), Some(333_333_333));
        assert_eq!(f.ticks(1), Some(1));
        assert_eq!(f.ticks(333_333_333), Some(1));
        assert_eq!(f.ticks(333_333_334), Some(2));
    }

    /// **A time survives the round trip no earlier than it was**, and a tick count survives
    /// it exactly while a tick is at least a nanosecond.
    #[test]
    fn a_round_trip_is_never_early() {
        for hz in [3, 1_000, TEN_MHZ, 1_000_000_000] {
            let f = frequency(hz);
            for nanos in [
                0,
                1,
                99,
                100,
                101,
                999_999_999,
                1_000_000_001,
                86_400_000_000_000,
            ] {
                let back = f
                    .nanos(f.ticks(nanos).expect("in range"))
                    .expect("in range");
                assert!(back >= nanos, "{hz} Hz: {nanos} ns came back as {back} ns");
            }
        }
        for hz in [1_000, TEN_MHZ, 1_000_000_000] {
            let f = frequency(hz);
            for ticks in [0, 1, 2, 1_000_001, 123_456_789_012] {
                assert_eq!(f.ticks(f.nanos(ticks).expect("in range")), Some(ticks));
            }
        }
    }

    /// **What no clock has is refused**: a zero frequency, a time before the clock's origin,
    /// and a count past an `i64` of nanoseconds or a `u64` of ticks.
    #[test]
    fn what_is_out_of_range_is_none() {
        assert_eq!(Frequency::new(0), None);
        assert_eq!(frequency(TEN_MHZ).ticks(-1), None);
        assert_eq!(frequency(1).nanos(u64::MAX), None);
        assert_eq!(
            frequency(1_000_000_000).ticks(i64::MAX),
            Some(i64::MAX as u64)
        );
        assert_eq!(frequency(u64::MAX).ticks(i64::MAX), None);
    }
}
