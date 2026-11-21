//! Standard MIDI File bytes: the wire half under [`MidiFile`](crate::MidiFile).
//!
//! [`read()`] is a `winnow` parser (private to `read.rs`, in no signature) and
//! [`write()`] a fold of pure functions, the reader's inverse. Both parse onto
//! and write from this crate's own types ([`FileEvent`](crate::FileEvent),
//! [`Meta`](crate::Meta)).
//!
//! # Everything here is 7-bit
//!
//! An SMF cannot express MIDI 2.0 resolution: a channel message is a
//! [`Voice1`](tutti_ump::Voice1) on the first Group, and the MIDI 2.0 side of the
//! crate (the Clip File) is a *different format*, not a wider encoding of
//! this one.
//!
//! # Truncation
//!
//! A chunk that runs past the end of the file, fewer track chunks than the
//! header declares, or a track that ends mid-event is a
//! [`ReadError::Smf`](crate::ReadError::Smf) whose cause is one of the
//! [`SmfParseError`] cut-off variants, with the track
//! and byte offset it was found at. A write the fields cannot hold is
//! [`TracksError::TooLarge`](crate::TracksError::TooLarge), refused when the
//! file is built.

mod read;
mod write;

pub(crate) use read::read;
pub(crate) use write::{measure, write};

use tutti_midi_protocol::MtcRate;
use tutti_ump::TicksPerQuarterNote;

use crate::SmfParseError;
use crate::file::{Opening, ReadRepair, SmfFormat, Track};

tutti_units::ranged! {
    /// A metrical division in ticks per quarter note, `1..=0x7FFF`: the
    /// header's division word has 15 bits for it, and its top bit says SMPTE
    /// timing (SMF 1.0 section 2.1). Zero is no division.
    #[derive(Eq, Hash)]
    SmfTicksPerQuarter(u16), SmfTicksPerQuarterOutOfRange, "within 1..=32767", |raw| raw >= 1 && raw <= 0x7FFF
}

tutti_units::ranged! {
    /// An SMPTE division's resolution within a frame, `1..=255`: the low byte
    /// of the division word (SMF 1.0 section 2.1). Zero is no resolution.
    #[derive(Eq, Hash)]
    SmfTicksPerFrame(u8), SmfTicksPerFrameOutOfRange, "within 1..=255", |raw| raw >= 1
}

impl From<SmfTicksPerQuarter> for TicksPerQuarterNote {
    fn from(ticks: SmfTicksPerQuarter) -> Self {
        // `SmfTicksPerQuarter`'s range lies inside `TicksPerQuarterNote`'s (1..=65535).
        TicksPerQuarterNote::of(ticks.get())
    }
}

/// The division word of an SMF header (SMF 1.0 section 2.1): what a delta
/// time counts.
///
/// With the word's top bit clear the file is metrical, the word being ticks
/// per quarter note. With it set the file is in SMPTE time: the high byte is
/// the frame rate as a negative two's-complement number and the low byte the
/// ticks per frame, so a tick is a fixed fraction of a second and the file has
/// no beat grid.
///
/// ```
/// use tutti_midi_file::{SmfDivision, SmfTicksPerFrame};
/// use tutti_midi_protocol::MtcRate;
///
/// // The specification's millisecond file: 25 frames a second, 40 ticks a frame.
/// let ms = SmfDivision::Smpte { fps: MtcRate::Fps25, ticks_per_frame: SmfTicksPerFrame::of(40) };
/// assert_eq!(ms.word(), 0xE728);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SmfDivision {
    /// Ticks per quarter note.
    Metrical(SmfTicksPerQuarter),
    /// SMPTE time: a frame rate and ticks per frame. The rate is one of the
    /// four the header names: -24, -25, -29 and -30, where the specification
    /// says "-29 corresponds to 30 drop frame", read as the 30000/1001 rate.
    Smpte {
        /// Frames per second.
        fps: MtcRate,
        /// The resolution within a frame.
        ticks_per_frame: SmfTicksPerFrame,
    },
}

impl SmfDivision {
    /// A metrical division of `ticks_per_quarter` for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `ticks_per_quarter` is not within `1..=32767`.
    #[must_use]
    #[track_caller]
    pub const fn per_quarter(ticks_per_quarter: u16) -> Self {
        Self::Metrical(SmfTicksPerQuarter::of(ticks_per_quarter))
    }

    /// The division the header word names.
    ///
    /// # Errors
    ///
    /// [`SmfParseError::ZeroTicksPerBeat`] for the word 0,
    /// [`SmfParseError::InvalidFrameRate`] for a top bit set over a high byte
    /// that is none of -24, -25, -29 and -30, and
    /// [`SmfParseError::ZeroTicksPerFrame`] for an SMPTE division of no
    /// resolution.
    pub(crate) fn from_word(word: u16) -> Result<Self, SmfParseError> {
        let [high, low] = word.to_be_bytes();
        if word & 0x8000 == 0 {
            return SmfTicksPerQuarter::new(word)
                .map(Self::Metrical)
                .ok_or(SmfParseError::ZeroTicksPerBeat);
        }
        let fps = match i8::from_be_bytes([high]) {
            -24 => MtcRate::Fps24,
            -25 => MtcRate::Fps25,
            -29 => MtcRate::Fps2997Df,
            -30 => MtcRate::Fps30,
            _ => return Err(SmfParseError::InvalidFrameRate),
        };
        let ticks_per_frame = SmfTicksPerFrame::new(low).ok_or(SmfParseError::ZeroTicksPerFrame)?;
        Ok(Self::Smpte {
            fps,
            ticks_per_frame,
        })
    }

    /// The header word: ticks per quarter note, or the negated frame rate over
    /// the ticks per frame.
    #[must_use]
    pub const fn word(self) -> u16 {
        match self {
            Self::Metrical(ticks) => ticks.get(),
            Self::Smpte {
                fps,
                ticks_per_frame,
            } => {
                let rate: i8 = match fps {
                    MtcRate::Fps24 => -24,
                    MtcRate::Fps25 => -25,
                    MtcRate::Fps2997Df => -29,
                    MtcRate::Fps30 => -30,
                };
                u16::from_be_bytes([rate.to_be_bytes()[0], ticks_per_frame.get()])
            }
        }
    }
}

/// What an SMF holds, read: the header's format and division and the tracks.
pub(crate) struct SmfParts {
    pub(crate) format: SmfFormat,
    pub(crate) division: SmfDivision,
    pub(crate) tracks: Vec<Track>,
    /// Each track's Sequence Number and SMPTE Offset.
    pub(crate) openings: Vec<Opening>,
    /// The notice of the first track.
    pub(crate) copyright: Option<Box<[u8]>>,
    /// What the read changed to make the tracks writable.
    pub(crate) repairs: Vec<ReadRepair>,
}
