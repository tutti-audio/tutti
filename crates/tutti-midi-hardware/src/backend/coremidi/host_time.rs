//! CoreMIDI's host time as an `Instant`.
//!
//! An event packet carries `timeStamp`: mach host time, the ticks of
//! `mach_absolute_time`, where 0 means "now". The tick is not a nanosecond
//! (`mach_timebase_info` gives the ratio), and `Instant` has no constructor from
//! a mach reading, so a callback reads the host clock and `Instant::now()`
//! together once ([`Pairing::now`]) and each packet is placed before that
//! reading by its stamp ([`Paired::arrival`]).
//!
//! The Mach clock itself is `tutti-coremidi-ffi`'s (`host_ticks`, `Timebase`).

use std::time::Instant;

use crate::stamp::Paired;
use tutti_coremidi_ffi::{Timebase, host_ticks};
use tutti_midi_runtime::SendAt;

/// The host clock and the OS's read together ([`Paired`]): to place a
/// callback's packets, and to stamp a batch's.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pairing {
    base: Timebase,
    paired: Paired,
}

impl Pairing {
    /// Reads both clocks now. Allocation-free, so it runs in CoreMIDI's callback.
    pub(super) fn now() -> Self {
        let base = Timebase::get();
        let before = Instant::now();
        let reading = base.duration(host_ticks());
        let paired = Paired::midpoint(before, Instant::now(), reading);
        Self { base, paired }
    }

    /// The instant of a packet stamped `host_time` ticks; a 0 stamp ("now") is
    /// the pairing's own.
    pub(super) fn instant_of(&self, host_time: u64) -> Instant {
        if host_time == 0 {
            return self.paired.arrival(self.paired.clock());
        }
        self.paired.arrival(self.base.duration(host_time))
    }

    /// The host time to stamp a packet that is to leave at `at`: the one place
    /// an `Instant` becomes CoreMIDI's clock, by this pairing's reading of both.
    /// An instant the host clock cannot hold is 0, "now".
    pub(super) fn host_time_of(&self, at: Instant) -> u64 {
        self.paired
            .departure(at)
            .map_or(0, |host| self.base.ticks(host))
    }
}

/// The host-time stamp of a packet sent `send`: 0 ("now") for [`SendAt::Now`],
/// else the host time of its instant by `pairing`, which is read from the clocks
/// the first time a scheduled packet needs it.
pub(super) fn stamp_of(send: SendAt, pairing: &mut Option<Pairing>) -> u64 {
    match send {
        SendAt::Now => 0,
        SendAt::At(instant) => pairing
            .get_or_insert_with(Pairing::now)
            .host_time_of(instant),
    }
}

#[cfg(test)]
mod tests;
