//! The parts of a [`Meta`](crate::Meta) that are not a plain field: the wire's
//! 24-bit tempo, the SMPTE Offset, the type byte of an untyped meta event, and a
//! meta event the reader could not type.

use tutti_midi_protocol::{
    MtcRate, SmpteTimecode, TimecodeHours, TimecodeMinutes, TimecodeSeconds,
};

tutti_units::ranged! {
    /// Microseconds per quarter note as a Set Tempo holds it: 24 bits,
    /// `0..=0xFF_FFFF` (SMF 1.0 section 3.3, `FF 51 03 tttttt`). Zero is no
    /// valid tempo and is kept so a file writes back as it was read.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    MicrosPerQuarter(u32), MicrosPerQuarterOutOfRange, "within 0..=16777215", |raw| raw <= 0xFF_FFFF
}

impl MicrosPerQuarter {
    /// The value of a Set Tempo's three data bytes, most significant first.
    pub(crate) const fn from_wire(bytes: [u8; 3]) -> Self {
        Self(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    /// The three data bytes of a Set Tempo, most significant first.
    pub(crate) const fn to_wire(self) -> [u8; 3] {
        let [_, a, b, c] = self.0.to_be_bytes();
        [a, b, c]
    }
}

tutti_units::ranged! {
    /// The fractional frames of an SMPTE Offset, `0..=99`: the `ff` field is in
    /// 100ths of a frame (SMF 1.0 section 3.3, `FF 54 05 hr mn se fr ff`).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    SmpteFractionalFrames(u8), SmpteFractionalFramesOutOfRange, "within 0..=99", |raw| raw <= 99
}

/// The SMPTE time a track starts at: the five data bytes of `FF 54 05 hr mn se
/// fr ff` (SMF 1.0 section 3.3).
///
/// The hour byte is encoded as MIDI Time Code encodes it, `0rrhhhhh`: the frame
/// rate in bits 5 and 6 over the hour in bits 0 to 4. So the time's rate is one
/// of MTC's four, and [`new`](Self::new) refuses a timecode at any other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SmpteOffset {
    time: SmpteTimecode,
    fractional: SmpteFractionalFrames,
}

impl SmpteOffset {
    /// The offset of `time` and `fractional` 100ths of a frame, or `None` when
    /// `time` is counted at 29.97 non-drop, which MTC has no code for.
    #[must_use]
    pub const fn new(time: SmpteTimecode, fractional: SmpteFractionalFrames) -> Option<Self> {
        match time.frame_rate().mtc_rate() {
            Some(_) => Some(Self { time, fractional }),
            None => None,
        }
    }

    /// The hours, minutes, seconds and frames, at the rate the hour byte names.
    #[must_use]
    pub const fn time(self) -> SmpteTimecode {
        self.time
    }

    /// The fractional frames, in 100ths of a frame.
    #[must_use]
    pub const fn fractional(self) -> SmpteFractionalFrames {
        self.fractional
    }

    /// The offset of the five data bytes, `None` for a byte that is out of its
    /// range: the hour byte's top bit, an hour past 23, a minute or second past
    /// 59, a frame past the rate's count, a fraction past 99.
    pub(crate) fn from_wire([hours, minutes, seconds, frames, fraction]: [u8; 5]) -> Option<Self> {
        if hours & 0x80 != 0 {
            return None;
        }
        let (rate, hours) = MtcRate::from_hours_byte(hours);
        let time = SmpteTimecode::new(
            TimecodeHours::new(hours)?,
            TimecodeMinutes::new(minutes)?,
            TimecodeSeconds::new(seconds)?,
            frames,
            rate.frame_rate(),
        )?;
        Self::new(time, SmpteFractionalFrames::new(fraction)?)
    }

    /// The five data bytes: the hour byte carries the rate's MTC code.
    pub(crate) fn to_wire(self) -> [u8; 5] {
        // A rate MTC has no code for (29.97 non-drop) writes as code 0, the
        // 24 fps code; the offset's rate is only what its hours byte names.
        let hours = self.time.hours().get();
        let hours_byte = self
            .time
            .frame_rate()
            .mtc_rate()
            .map_or(hours, |rate| rate.hours_byte(hours));
        [
            hours_byte,
            self.time.minutes().get(),
            self.time.seconds().get(),
            self.time.frames(),
            self.fractional.get(),
        ]
    }
}

/// Whether `kind` is a meta event type the model types: Sequence Number `00`,
/// the text events `01` to `09`, MIDI Channel Prefix `20`, End of Track `2F`
/// (a track's `end`), Set Tempo `51`, SMPTE Offset `54`, Time Signature `58`,
/// Key Signature `59` and Sequencer-Specific `7F`.
const fn is_typed(kind: u8) -> bool {
    matches!(
        kind,
        0x00..=0x09 | 0x20 | 0x2F | 0x51 | 0x54 | 0x58 | 0x59 | 0x7F
    )
}

tutti_units::ranged! {
    /// The type byte of a [`Meta::Other`](crate::Meta::Other): any of `0..=127`
    /// that [`Meta`](crate::Meta) has no variant for. A typed kind (`00` to `09`,
    /// `20`, `2F`, `51`, `54`, `58`, `59`, `7F`) is refused, so an `Other` is
    /// never read back as the variant of its kind, and a type byte has no top
    /// bit (SMF 1.0 section 3.3).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    OtherMetaKind(u8), OtherMetaKindOutOfRange, "a meta type byte below 128 that has no Meta variant", |raw| raw < 0x80 && !is_typed(raw)
}

/// A meta event whose type the model types but whose data is not what the type
/// defines: a Set Tempo, Time Signature or Key Signature shorter than its
/// fixed part, a Key Signature whose mode is neither 0 nor 1, a MIDI Channel
/// Prefix of a channel past 15, an SMPTE Offset with a field out of its range, a
/// Sequencer-Specific with no manufacturer id, an End of Track with data (SMF
/// 1.0 section 3.3), or a type byte with its top bit set.
///
/// A lenient reader keeps it: the file's other events are read, and a write
/// gives the event's bytes back exactly. Only a read makes one, so a
/// `MalformedMeta` is never one that reads back as a typed event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MalformedMeta {
    kind: u8,
    data: Box<[u8]>,
}

impl MalformedMeta {
    /// The event the reader found: its type byte and data.
    pub(crate) fn new(kind: u8, data: &[u8]) -> Self {
        Self {
            kind,
            data: data.into(),
        }
    }

    /// The type byte as the file has it.
    #[must_use]
    pub const fn kind(&self) -> u8 {
        self.kind
    }

    /// The data as the file has it, without its length.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// The first `N` bytes of a meta event's data and the rest, which a program
/// ignores (SMF 1.0 section 3.3): `None` when the data is shorter than `N`.
pub(crate) fn split_extra<const N: usize>(data: &[u8]) -> Option<([u8; N], Box<[u8]>)> {
    let (fixed, extra) = data.split_first_chunk::<N>()?;
    Some((*fixed, extra.into()))
}

/// The data of a meta event: its `fixed` bytes, then the `extra` it was read with.
pub(crate) fn with_extra(fixed: &[u8], extra: &[u8]) -> Vec<u8> {
    [fixed, extra].concat()
}
