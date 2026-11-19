//! An OS timestamp as an `Instant`, and back: the one rule both backends use.
//!
//! An OS stamps an event on a clock of its own (an ALSA queue's real time since
//! it started, CoreMIDI's mach host time). A backend reads that clock and
//! `Instant::now()` together once, and each event's instant is then as long
//! before that reading as its stamp is before the clock's. Nothing here reads a
//! clock.

use std::time::Duration;

use tutti_platform::Instant;

/// The instant of an event stamped `stamped` on a clock that read `clock` when
/// the OS's read `os`: as long before `os` as `stamped` is before `clock`. An
/// event stamped after the reading (it should not be), or one that reaches back
/// past what an `Instant` can hold, is at `os`.
fn arrival(os: Instant, clock: Duration, stamped: Duration) -> Instant {
    os.checked_sub(clock.saturating_sub(stamped)).unwrap_or(os)
}

/// The reading of a clock at `at`, the inverse of [`arrival`]: a clock that
/// read `clock` when the OS's read `os` reads `clock` plus as long as `at` is
/// after `os` (less as long before, never below zero). `None` where the
/// reading is past what a `Duration` holds. The one rule both backends
/// schedule by: it is their conversion of a send time to the wire's clock.
fn departure(os: Instant, clock: Duration, at: Instant) -> Option<Duration> {
    match at.checked_duration_since(os) {
        Some(after) => clock.checked_add(after),
        None => Some(clock.saturating_sub(os.duration_since(at))),
    }
}

/// An OS clock read together with `Instant`: how a batch's send times become
/// the OS's.
///
/// The two clocks are read afresh for each batch and not once for the life of an
/// output, because they do not keep step: one that stops across a sleep, or is
/// slewed, would carry a pairing from the output's open further from the
/// `Instant`s it is given with every second. A pairing lasts only the batch it
/// was read for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Paired {
    os: Instant,
    clock: Duration,
}

impl Paired {
    /// The pairing of `os` and `clock`, read at the same moment.
    pub(crate) const fn new(os: Instant, clock: Duration) -> Self {
        Self { os, clock }
    }

    /// The pairing of `reading` with the midpoint of `before` and `after`, the
    /// two readings of `Instant` the clock was read between: the reading's own
    /// delay is split between the two sides.
    pub(crate) fn midpoint(before: Instant, after: Instant, reading: Duration) -> Self {
        Self::new(
            before + after.saturating_duration_since(before) / 2,
            reading,
        )
    }

    /// The clock's reading at `at` ([`departure`]).
    pub(crate) fn departure(&self, at: Instant) -> Option<Duration> {
        departure(self.os, self.clock, at)
    }

    /// The instant of an event the clock stamped `stamped` ([`arrival`]).
    pub(crate) fn arrival(&self, stamped: Duration) -> Instant {
        arrival(self.os, self.clock, stamped)
    }

    /// The clock's reading in this pairing.
    pub(crate) const fn clock(&self) -> Duration {
        self.clock
    }
}

#[cfg(test)]
mod tests;
