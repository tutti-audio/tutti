//! The service's clock, as `tutti_platform`'s `Instant`.

use tutti_platform::{Instant, instant_of_monotonic_ns, monotonic_ns_of};

use super::Error;
use super::call::{call, statics};
use crate::bindings::Windows::Devices::Midi2::{IMidiClockStatics, MidiClock};
use crate::checks::timestamp::Frequency;

/// The service's clock, whose ticks are `QueryPerformanceCounter`'s: the timestamps of the
/// messages it carries are points on the clock `Instant` reads, so a timestamp and an
/// `Instant` convert exactly (to a tick).
#[derive(Clone, Copy, Debug)]
pub(super) struct Clock {
    frequency: Frequency,
    /// `MidiClock.TimestampConstantSendImmediately`: the timestamp that asks for a send now.
    immediate: u64,
}

impl Clock {
    /// Reads the clock's frequency from the service's API: `Error::ApiMissing` when the API is
    /// not present, `Error::Call` when a read fails and `Error::Timestamp` for a clock that
    /// reports no frequency.
    pub(super) fn new() -> Result<Self, Error> {
        let clock = statics::<MidiClock, IMidiClockStatics>()?;
        let frequency = call!("TimestampFrequency", clock, TimestampFrequency() -> u64)?;
        let immediate = call!(
            "TimestampConstantSendImmediately",
            clock,
            TimestampConstantSendImmediately() -> u64
        )?;
        Ok(Self {
            frequency: Frequency::new(frequency).ok_or(Error::Timestamp)?,
            immediate,
        })
    }

    /// The `Instant` of a timestamp; `None` where it is not one (before the process's
    /// clock pairing by more than an `Instant` holds, or past an `i64` of nanoseconds).
    pub(super) fn instant(&self, ticks: u64) -> Option<Instant> {
        instant_of_monotonic_ns(self.frequency.nanos(ticks)?)
    }

    /// The timestamp that sends at `at`, or now for `None`. A time the service's clock
    /// reaches only later is rounded up to its next tick.
    pub(super) fn timestamp(&self, at: Option<Instant>) -> Result<u64, Error> {
        let Some(at) = at else {
            return Ok(self.immediate);
        };
        monotonic_ns_of(at)
            .and_then(|nanos| self.frequency.ticks(nanos))
            .ok_or(Error::Timestamp)
    }
}

#[cfg(all(test, feature = "services-tests"))]
mod tests {
    use tutti_platform::{Instant, instant_of_monotonic_ns};

    use super::*;

    /// **The service's clock is the clock `Instant` reads**: a reading, mapped through the
    /// clock's frequency, lies between two reads of `Instant::now`.
    #[test]
    fn the_services_clock_is_the_instant_clock() {
        let clock = Clock::new().expect("the API's clock");
        let service = statics::<MidiClock, IMidiClockStatics>().expect("the clock's statics");
        // The pairing of the two clocks is made at its first use: make it before the reads.
        let _ = instant_of_monotonic_ns(0);
        let before = Instant::now();
        let ticks = call!("Now", service, Now() -> u64).expect("a reading");
        let after = Instant::now();
        let now = clock.instant(ticks).expect("a reading is an Instant");
        assert!(
            before <= now && now <= after,
            "{before:?} {now:?} {after:?}"
        );
    }
}
