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

/// The packed short message of `ump`: its MIDI 1.0 bytes ([`Ump::midi1_bytes`]),
/// low byte first, the rest of the word zero. `None` for a UMP that has none.
/// The Group is not sent: a byte stream has none.
pub(crate) fn pack(ump: &Ump) -> Option<u32> {
    let mut word = [0; 4];
    let bytes = ump.midi1_bytes()?;
    word[..bytes.as_slice().len()].copy_from_slice(bytes.as_slice());
    Some(u32::from_le_bytes(word))
}

/// Widens winmm's stamp, milliseconds since the input started in a `u32` that
/// wraps after 49.7 days, to the time since it started.
///
/// The first stamp is the time since the start. Each later one is read as the
/// signed difference from the one before it, which is what survives a wrap. A
/// negative difference is a stamp out of order (a SysEx buffer that completes
/// after a short message that came later): it leaves the time where it is, so
/// the time never runs backward and never jumps by a wrap.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stamps {
    /// The latest in-order stamp; none before the first.
    last: Option<u32>,
    elapsed: u64,
}

impl Stamps {
    /// The time since the input started at the stamp `millis`.
    pub(crate) fn since_start(&mut self, millis: u32) -> Duration {
        let Some(last) = self.last else {
            // The first stamp is the time since the start, as it is.
            self.last = Some(millis);
            self.elapsed = u64::from(millis);
            return Duration::from_millis(self.elapsed);
        };
        let step = millis.wrapping_sub(last) as i32;
        if let Ok(step) = u64::try_from(step) {
            self.elapsed += step;
            self.last = Some(millis);
        }
        Duration::from_millis(self.elapsed)
    }
}

#[cfg(test)]
mod tests;
