//! A short message's `dwParam` and a stamp's width, with no OS call.
//!
//! winmm packs a short message into a `DWORD`: the status byte, then its data
//! bytes, low byte first, the unused bytes zero (Microsoft's `midiOutShortMsg`
//! page). Here that is a MIDI 1.0 UMP on Group 0 and back (M2-104 section 7.3,
//! 7.6).

use std::time::Duration;

use tutti_ump::{MidiGroup, Ump, parse_exact};

/// The UMP of a packed short message, on Group 0. `None` for a status that is
/// not a message (a stray data byte, or a SysEx status, which winmm sends as a
/// buffer).
pub(crate) fn unpack(packed: u32) -> Option<Ump> {
    let [status, d1, d2, _] = packed.to_le_bytes();
    parse_exact(MidiGroup::FIRST, &[status, d1, d2]).map(Ump::from)
}

/// The packed short message of `ump`, a MIDI 1.0 channel voice (type 2) or
/// system common or real time (type 1) message. `None` for any other message
/// type, which has no MIDI 1.0 byte-stream form, and for a first word whose
/// bytes are not a message. The Group is not sent: a byte stream has none.
pub(crate) fn pack(ump: &Ump) -> Option<u32> {
    if !matches!(ump.message_type(), 1 | 2) {
        return None;
    }
    let [_, status, d1, d2] = ump.0[0].to_be_bytes();
    parse_exact(MidiGroup::FIRST, &[status, d1, d2])?;
    Some(u32::from_le_bytes([status, d1, d2, 0]))
}

/// Widens winmm's stamp, milliseconds since the input started in a `u32` that
/// wraps after 49.7 days, to the time since it started.
///
/// An input's stamps never run backward, so each is the previous plus the
/// wrapping difference.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stamps {
    last: u32,
    elapsed: u64,
}

impl Stamps {
    /// The time since the input started at the stamp `millis`.
    pub(crate) fn since_start(&mut self, millis: u32) -> Duration {
        self.elapsed += u64::from(millis.wrapping_sub(self.last));
        self.last = millis;
        Duration::from_millis(self.elapsed)
    }
}

#[cfg(test)]
mod tests;
