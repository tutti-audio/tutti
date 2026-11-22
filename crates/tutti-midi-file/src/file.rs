//! [`MidiFile`]: one file model for both containers, lossless, with a tempo
//! map.
//!
//! `.mid` is worn by a Standard MIDI File and a MIDI 2.0 Clip File alike, so
//! which decoder runs is decided by the leading bytes ([`MidiFileKind::detect`]),
//! never by the name. Both read into the same shape: [`Track`]s of
//! `(Tick, FileEvent)`, the file's division, and the [`TempoMap`] its tempo
//! events declare. Ticks are integers, so a file read and written holds the
//! same events with no rounding; [`MetricalView::events`] is where they become
//! [`Beat`]s.
//!
//! The file layer does not translate. An SMF channel message is a MIDI 1.0
//! [`Message::Midi1`] on the first Group, and a MIDI 1.0 Note On at velocity 0 stays
//! one (it is a note-off by the MIDI 1.0 rule, which [`Track::notes`] applies);
//! a Clip File's packets are as stored. Translation runs where a clip is
//! loaded for playback. A file allocates and never reaches the audio thread.

use std::collections::HashMap;
use std::io::{ErrorKind, Write as _};
use std::sync::Arc;

use tutti_midi_ci::ProfileOnRequest;
use tutti_midi_protocol::{MtcRate, channel_voice};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_platform::{Asset, Destination};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_ump::{
    Address, Flex, FlexAddress, FlexTextKind, Form, ManufacturerId, Message, MidiChannel,
    MidiGroup, NoteKeyEvent, NoteNumber, NoteVelocity, Sysex7Packets, SysexPart, System,
    TicksPerQuarterNote, U7, Ump, Unknown, Voice2,
};
use tutti_units::{Beat, Bpm, Seconds};

use crate::MidiFileKind;
use crate::smf::{SmfDivision, SmfTicksPerFrame};
use crate::{OutOfOrder, ReadError, Refusal, TrackPart, TracksError, WriteError, clip_file, smf};

mod meta_kind;
mod to_clip;

pub use meta_kind::{
    MalformedMeta, MicrosPerQuarter, MicrosPerQuarterOutOfRange, OtherMetaKind,
    OtherMetaKindOutOfRange, SmpteFractionalFrames, SmpteFractionalFramesOutOfRange, SmpteOffset,
};
pub(crate) use meta_kind::{split_extra, with_extra};
pub use to_clip::{
    Altered, Converted, DropReason, Dropped, LyricChange, Moved, SmfPart, ToClipError,
};
use tutti_ump::Voice1;

/// A position in a file, in ticks of its division: whole numbers, so a file
/// written back lands every event on the tick it was read at.
///
/// A tick is a count, and the whole of `u64` is in range. Arithmetic is
/// checked: [`checked_add`](Tick::checked_add), [`ticks_since`](Tick::ticks_since)
/// and [`quarters`](Tick::quarters) answer `None` where `u64` would wrap.
/// [`MetricalView::beat_at`] is the conversion to a [`Beat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(pub u64);

impl Tick {
    /// `count` quarter notes in a file of division `ticks_per_quarter`, `None`
    /// when the tick is past `u64::MAX`.
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    /// use tutti_ump::TicksPerQuarterNote;
    ///
    /// assert_eq!(Tick::quarters(4, TicksPerQuarterNote::of(480)), Some(Tick(1_920)));
    /// assert_eq!(Tick::quarters(u64::MAX, TicksPerQuarterNote::of(2)), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn quarters(count: u64, ticks_per_quarter: TicksPerQuarterNote) -> Option<Self> {
        match count.checked_mul(ticks_per_quarter.get() as u64) {
            Some(ticks) => Some(Self(ticks)),
            None => None,
        }
    }

    /// `ticks` later, `None` when the tick is past `u64::MAX`.
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    ///
    /// assert_eq!(Tick(10).checked_add(5), Some(Tick(15)));
    /// assert_eq!(Tick(u64::MAX).checked_add(1), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn checked_add(self, ticks: u64) -> Option<Self> {
        match self.0.checked_add(ticks) {
            Some(tick) => Some(Self(tick)),
            None => None,
        }
    }

    /// The ticks from `earlier` to this tick, `None` when `earlier` is after
    /// it.
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    ///
    /// assert_eq!(Tick(15).ticks_since(Tick(10)), Some(5));
    /// assert_eq!(Tick(10).ticks_since(Tick(15)), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn ticks_since(self, earlier: Tick) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }

    /// This tick in quarter notes: the tick over the division
    /// (`ticks_per_quarter`, [`MetricalView::ticks_per_quarter`]).
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    /// use tutti_units::Beat;
    /// use tutti_ump::TicksPerQuarterNote;
    ///
    /// assert_eq!(Tick(720).beats(TicksPerQuarterNote::of(480)), Beat::new(1.5));
    /// ```
    #[inline]
    #[must_use]
    pub fn beats(self, ticks_per_quarter: TicksPerQuarterNote) -> Beat {
        Beat::new(self.0 as f64 / f64::from(ticks_per_quarter.get()))
    }

    /// The tick nearest `beat` in a file of division `ticks_per_quarter`: the
    /// inverse of [`beats`](Self::beats).
    ///
    /// A beat between two ticks rounds to the nearer; exactly halfway rounds to
    /// the later. `None` for a beat before 0, NaN, or whose tick is past
    /// `u64::MAX`; nothing saturates. Past 2^53 ticks an `f64` beat no longer
    /// names every tick, and the nearest one it does name is returned.
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    /// use tutti_units::Beat;
    /// use tutti_ump::TicksPerQuarterNote;
    ///
    /// assert_eq!(Tick::from_beat(Beat::new(1.5), TicksPerQuarterNote::of(480)), Some(Tick(720)));
    /// // 0.5 tick at 128 ticks per quarter rounds up.
    /// assert_eq!(Tick::from_beat(Beat::new(1.0 / 256.0), TicksPerQuarterNote::of(128)), Some(Tick(1)));
    /// assert_eq!(Tick::from_beat(Beat::new(-1.0), TicksPerQuarterNote::of(480)), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn from_beat(beat: Beat, ticks_per_quarter: TicksPerQuarterNote) -> Option<Self> {
        // `round` takes a tie away from zero, which is later for a beat at or
        // after 0. A beat in (-0.5 tick, 0) is before 0 and is refused.
        let ticks = (beat.get() * f64::from(ticks_per_quarter.get())).round();
        // `u64::MAX as f64` is 2^64, the first value out of range.
        (beat.get() >= 0.0 && ticks < u64::MAX as f64).then_some(Self(ticks as u64))
    }
}

/// The format word of an SMF header (SMF 1.0 section 2.1).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SmfFormat {
    /// Format 0: one track holding everything.
    SingleTrack,
    /// Format 1: simultaneous tracks of one timeline, the first usually
    /// carrying the tempo map.
    Parallel,
    /// Format 2: independent sequences, each with its own timeline. Their
    /// tracks are not merged and share no tempo map.
    Sequential,
    /// A format the specification does not define (3 and up): "A program
    /// encountering an unknown format ID may still read other MTrk chunks it
    /// finds from the file" (SMF 1.0 section 2.1). Its tracks are kept apart
    /// as a format 2 file's are, since nothing says they share a timeline,
    /// and the word is written back as it was read.
    Other(OtherSmfFormat),
}

tutti_units::ranged! {
    /// The format word of an SMF whose format the specification does not
    /// define: `3..=65535`. The words 0, 1 and 2 are [`SmfFormat`]'s named
    /// formats, so one format is never spelled two ways.
    #[derive(Eq, Hash)]
    OtherSmfFormat(u16), OtherSmfFormatOutOfRange, "within 3..=65535", |raw| raw >= 3
}

impl SmfFormat {
    /// The format word of the header.
    pub(crate) fn word(self) -> u16 {
        match self {
            Self::SingleTrack => 0,
            Self::Parallel => 1,
            Self::Sequential => 2,
            Self::Other(other) => other.get(),
        }
    }

    /// The format a header's `word` names.
    pub(crate) fn from_word(word: u16) -> Self {
        match word {
            0 => Self::SingleTrack,
            1 => Self::Parallel,
            2 => Self::Sequential,
            other => Self::Other(OtherSmfFormat::of(other)),
        }
    }

    /// Whether the tracks are simultaneous parts of one timeline (formats 0
    /// and 1) and not independent sequences.
    pub(crate) fn shares_timeline(self) -> bool {
        matches!(self, Self::SingleTrack | Self::Parallel)
    }
}

/// What a text meta event says (SMF 1.0 section 3.3 types `01` to `09`). The
/// copyright notice (`02`) and the Track Name (`03`) are the fields of a
/// [`Track`], not events.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextKind {
    /// `01`: any text.
    Text,
    /// `04`: the instrument a track is for.
    InstrumentName,
    /// `05`: a lyric syllable.
    Lyric,
    /// `06`: a marker, such as a rehearsal letter or a section name.
    Marker,
    /// `07`: a cue point.
    CuePoint,
    /// `08`: a program (patch) name.
    ProgramName,
    /// `09`: a device (port) name.
    DeviceName,
}

impl TextKind {
    /// Every kind, the one list [`from_meta_type`](Self::from_meta_type) reads.
    const ALL: [Self; 7] = [
        Self::Text,
        Self::InstrumentName,
        Self::Lyric,
        Self::Marker,
        Self::CuePoint,
        Self::ProgramName,
        Self::DeviceName,
    ];

    /// The meta event type byte.
    pub(crate) fn meta_type(self) -> u8 {
        match self {
            Self::Text => 0x01,
            Self::InstrumentName => 0x04,
            Self::Lyric => 0x05,
            Self::Marker => 0x06,
            Self::CuePoint => 0x07,
            Self::ProgramName => 0x08,
            Self::DeviceName => 0x09,
        }
    }

    /// The kind a meta event type byte names.
    pub(crate) fn from_meta_type(meta_type: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.meta_type() == meta_type)
    }
}

/// A meta event of a Standard MIDI File (SMF 1.0 section 3.3), minus the End
/// of Track, which is a [`Track`]'s `end`, and the four that open a track
/// (Sequence Number, copyright notice, Track Name, SMPTE Offset), which are a
/// track's [`Lead`] fields.
///
/// Text is kept as bytes: the specification does not say which encoding it is
/// in, and a file must write back what it read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Meta {
    /// A text event.
    Text {
        /// Which of them.
        kind: TextKind,
        /// The text, as the file stores it.
        bytes: Box<[u8]>,
    },
    /// `51`: Set Tempo, microseconds per quarter note. The 24-bit wire value
    /// (a zero, no valid tempo, is kept to be written back). [`Meta::tempo`]
    /// and [`Meta::bpm`] cross to a [`Bpm`].
    Tempo {
        /// Microseconds per quarter note.
        micros_per_quarter: MicrosPerQuarter,
        /// The data past the three bytes this reads: empty unless the file
        /// has a longer event (SMF 1.0 section 3.3: a program ignores
        /// everything past what it knows). Kept so the file writes
        /// back as it was.
        extra: Box<[u8]>,
    },
    /// `58`: Time Signature, as the wire states it.
    TimeSignature {
        /// The numerator.
        numerator: u8,
        /// The denominator as a power of two: 2 is a quarter, 3 an eighth.
        denominator_power: u8,
        /// MIDI clocks in a metronome click.
        clocks_per_click: u8,
        /// Notated 32nd notes in a MIDI quarter note (24 clocks): 8 usually.
        thirty_seconds_per_quarter: u8,
        /// The data past the four bytes this reads, as `extra` of
        /// [`Tempo`](Self::Tempo). A longer Time Signature is real: compound
        /// signatures extend the event at its end (`FF 58 06 ...`, SMF 1.0
        /// section 3.3).
        extra: Box<[u8]>,
    },
    /// `59`: Key Signature.
    KeySignature {
        /// Sharps if positive, flats if negative. The byte is kept as read, so a
        /// value outside `-7..=7` (SMF 1.0 section 3.3) survives a round trip.
        sharps: i8,
        /// Whether the key is minor.
        minor: bool,
        /// The data past the two bytes this reads, as `extra` of
        /// [`Tempo`](Self::Tempo).
        extra: Box<[u8]>,
    },
    /// `20`: MIDI Channel Prefix, `FF 20 01 cc`: the channel of the SysEx and
    /// meta events that follow, until the next channel message or prefix
    /// ([`Track::events_with_channel`]).
    ChannelPrefix {
        /// The channel, `0..=15`.
        channel: MidiChannel,
        /// The data past the byte this reads, as `extra` of
        /// [`Tempo`](Self::Tempo).
        extra: Box<[u8]>,
    },
    /// `7F`: Sequencer-Specific, `FF 7F len data`: the data starts with a
    /// manufacturer id of one byte or, after a `00`, of three.
    SequencerSpecific {
        /// The id the data starts with.
        manufacturer: ManufacturerId,
        /// The data after the id.
        data: Box<[u8]>,
    },
    /// Any other meta event (port `21`, an unregistered type): its type byte
    /// and its data.
    Other {
        /// The type byte: never one of the typed variants', so a write and a
        /// read give the same event.
        kind: OtherMetaKind,
        /// The data, without its length.
        data: Box<[u8]>,
    },
    /// A meta event of a typed kind whose data the kind does not define, kept
    /// as the file has it ([`MalformedMeta`]).
    Malformed(MalformedMeta),
}

/// Microseconds in a second: a Set Tempo is microseconds per quarter note.
const MICROS_PER_SECOND: f64 = 1_000_000.0;

impl Meta {
    /// A Set Tempo for `bpm`, rounded to the wire's microsecond, or `None`
    /// when the rounded microseconds per quarter is outside `1..=16_777_215`:
    /// the wire holds 24 bits (SMF 1.0 section 3.3) and its zero means no
    /// valid tempo. That is a tempo above about 120 million BPM or below about 3.6.
    #[must_use]
    pub fn tempo(bpm: Bpm) -> Option<Self> {
        let micros = (bpm.beat_period().get() * MICROS_PER_SECOND).round();
        // `as` saturates, and a value past the field fails `new` below.
        let micros = MicrosPerQuarter::new(micros as u32).filter(|m| m.get() != 0)?;
        Some(Self::Tempo {
            micros_per_quarter: micros,
            extra: Box::default(),
        })
    }

    /// The tempo of a Set Tempo, `None` for any other event and for a zero
    /// (no valid tempo).
    pub fn bpm(&self) -> Option<Bpm> {
        match self {
            Self::Tempo {
                micros_per_quarter, ..
            } if micros_per_quarter.get() != 0 => Bpm::from_beat_period(Seconds::new(
                f64::from(micros_per_quarter.get()) / MICROS_PER_SECOND,
            )),
            _ => None,
        }
    }

    /// A text event of `kind`.
    pub fn text(kind: TextKind, text: &str) -> Self {
        Self::Text {
            kind,
            bytes: text.as_bytes().into(),
        }
    }

    /// The ASCII characters of a text event: the bytes with the top bit clear.
    /// SMF 1.0 section 3.3: text is printable ASCII for interchange, other
    /// codes using the high bit are for programs on one computer, and a
    /// program on a computer without non-ASCII characters ignores them. The
    /// event keeps its bytes whatever this view leaves out. `None` for an event
    /// that is not text.
    pub fn ascii_text(&self) -> Option<String> {
        match self {
            Self::Text { bytes, .. } => Some(
                bytes
                    .iter()
                    .filter(|b| b.is_ascii())
                    .map(|&b| char::from(b))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// A MIDI Channel Prefix event for `channel`.
    pub fn channel_prefix(channel: MidiChannel) -> Self {
        Self::ChannelPrefix {
            channel,
            extra: Box::default(),
        }
    }
}

/// An SMF `F0` System Exclusive event (SMF 1.0 section 3: a message always
/// ends with `F7`, so the reader knows it has read all of it; a divided
/// message's last packet ends with it).
///
/// A whole message is [`Complete`](Self::Complete), whose `F7` the writer
/// adds. A divided one is a [`Start`](Self::Start) and then `F7`
/// [`Escape`](FileEvent::Escape) events, the last ending in `F7`, with no
/// channel message or other `F0` event between ([`TracksError::SysExInterrupted`],
/// [`TracksError::SysExNotEnded`]).
///
/// The payload is 7-bit: `F0` and `F7` are not part of it, and a byte of 128 or
/// more is a status byte, so none can be built ([`complete`](Self::complete),
/// [`start`](Self::start)). A packet that ends in `F7` is a whole message on the
/// wire, so a `Start` never does. A file whose `F0` event holds an 8-bit byte
/// is read as an [`Escape`](FileEvent::Escape) of the bytes it transmits
/// ([`ReadRepair::SysExEscaped`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SysEx {
    /// A whole message: the bytes between the `F0` and the `F7`, which is not
    /// part of them.
    Complete(Box<[U7]>),
    /// The first packet of a divided message: the bytes after the `F0`.
    Start(Box<[U7]>),
}

impl SysEx {
    /// A whole message of `bytes`, `None` if one is 128 or more.
    #[must_use]
    pub fn complete(bytes: &[u8]) -> Option<Self> {
        seven_bit(bytes).map(Self::Complete)
    }

    /// The first packet of a divided message of `bytes`, `None` if one is 128
    /// or more (the `F7` that ends the message is in its last packet).
    #[must_use]
    pub fn start(bytes: &[u8]) -> Option<Self> {
        seven_bit(bytes).map(Self::Start)
    }

    /// The payload.
    #[must_use]
    pub fn data(&self) -> &[U7] {
        let (Self::Complete(data) | Self::Start(data)) = self;
        data
    }
}

/// `bytes` as 7-bit data, `None` if any is 128 or more.
fn seven_bit(bytes: &[u8]) -> Option<Box<[U7]>> {
    bytes.iter().map(|&b| U7::new(b)).collect()
}

/// One event of a track.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileEvent {
    /// A message. An SMF's channel message is [`Message::Midi1`] on the first Group; a
    /// Clip File's packets are as stored.
    Message(Message),
    /// A Clip File packet no definition covers (a reserved message type, an
    /// undefined status), kept as its words so it writes back unchanged.
    Unknown(Unknown),
    /// An SMF `F0` System Exclusive event: a whole message or the first packet
    /// of a divided one. Any length, unlike a packet.
    SysEx(SysEx),
    /// An SMF `F7` event: the continuation of a divided System Exclusive
    /// message (its last packet ends in `F7`), or an escape sequence of
    /// arbitrary bytes. Distinct from [`SysEx`](Self::SysEx) because the file
    /// distinguishes them.
    Escape(Box<[u8]>),
    /// An SMF meta event.
    Meta(Meta),
}

impl From<Message> for FileEvent {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}

impl From<Meta> for FileEvent {
    fn from(meta: Meta) -> Self {
        Self::Meta(meta)
    }
}

impl FileEvent {
    /// The message, for an event that is one.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(m) => Some(m),
            _ => None,
        }
    }

    /// The packet of an event that is one: a message, or a Clip File packet no
    /// definition covers. `None` for a SysEx, an escape or a meta event, which
    /// are an SMF's.
    pub(crate) fn ump(&self) -> Option<Ump> {
        match self {
            Self::Message(message) => Some(Ump::from(*message)),
            Self::Unknown(unknown) => Some(unknown.0),
            Self::SysEx(_) | Self::Escape(_) | Self::Meta(_) => None,
        }
    }
}

/// One of the four meta events that open an SMF track (SMF 1.0 section 3.3),
/// each a field of its file or track and so never an event placed late: a Sequence
/// Number, a Track Name, an SMPTE Offset and a copyright notice. A
/// [`ReadRepair`] names the one it changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lead {
    /// `FF 00 02 ssss`: the number of the track's sequence
    /// ([`LeadTrack::numbered`], [`Pattern::numbered`]).
    SequenceNumber(u16),
    /// `FF 03`: the track's name, as the file stores it ([`Track::named`]).
    TrackName(Box<[u8]>),
    /// `FF 54 05 hr mn se fr ff`: the SMPTE time the track starts at
    /// ([`LeadTrack::smpte_start`], [`Pattern::smpte_start`]).
    SmpteOffset(SmpteOffset),
    /// `FF 02`: the copyright notice, as the file stores it
    /// ([`LeadTrack::copyright`]).
    Copyright(Box<[u8]>),
}

impl Lead {
    /// The meta event's type byte and data.
    pub(crate) fn wire(&self) -> (u8, Vec<u8>) {
        match self {
            Self::SequenceNumber(number) => (0x00, number.to_be_bytes().to_vec()),
            Self::TrackName(name) => (0x03, name.to_vec()),
            Self::SmpteOffset(offset) => (0x54, offset.to_wire().to_vec()),
            Self::Copyright(notice) => (0x02, notice.to_vec()),
        }
    }
}

/// What a read did to a [`Lead`] event that was not as SMF 1.0 section 3.3 has
/// it ([`ReadRepair::Lead`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LeadRepair {
    /// The event was at tick 0 but not at the start of its track (a Track
    /// Name after a channel message, a copyright notice after a SysEx): it
    /// became the track's field and is written first.
    Moved,
    /// The event was after tick 0, or a Sequence Number or SMPTE Offset after
    /// a nonzero delta-time or a transmittable event: it is dropped.
    Late,
    /// The track already had one of its kind: the later is dropped.
    Duplicate,
    /// A Sequence Number outside the first track of a format 0 or 1 file, an
    /// SMPTE Offset outside the first track of a format 1 file, or a copyright
    /// notice outside the first track: it is dropped.
    NotInFirstTrack,
    /// A Sequence Number or SMPTE Offset longer than the bytes it defines (SMF
    /// 1.0 section 3.3: a program ignores everything past what it knows): the
    /// event is kept as the track's field, these bytes past its own are
    /// dropped. Listed beside the event's own entry when it is also misplaced.
    Truncated(Box<[u8]>),
}

/// One change a read made to the file so that it can be written
/// ([`MidiFile::repairs`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadRepair {
    /// A [`Lead`] event the file had out of place, with the tick it was at.
    Lead {
        /// The track.
        track: usize,
        /// The tick the event was at.
        at: Tick,
        /// The event: kept as the track's field when `placement` is
        /// [`Moved`](LeadRepair::Moved), dropped otherwise.
        lead: Lead,
        /// What was done.
        placement: LeadRepair,
    },
    /// The track held the tempo map or a timeline of its own and had no Set
    /// Tempo at tick 0 (SMF 1.0 section 2.2: a file specifies its tempo): the
    /// reader's 120 beats a minute was inserted as the track's tempo.
    DefaultTempo {
        /// The track.
        track: usize,
    },
    /// The track had no Time Signature at tick 0: 4/4 was inserted, with the
    /// 24 clocks a click and 8 32nd notes to the quarter of section 3.3's example.
    DefaultTimeSignature {
        /// The track.
        track: usize,
    },
    /// An `F0` event with a byte of 128 or more, which no [`SysEx`] holds: it
    /// became the [`Escape`](FileEvent::Escape) of the bytes it transmits, the
    /// `F0`, its data and the `F7` it ends with if it has one. A write keeps
    /// the bytes transmitted but not the file's own: the event is written as
    /// an `F7` escape whose data starts with the `F0`.
    SysExEscaped {
        /// The track.
        track: usize,
        /// The tick of the event.
        at: Tick,
    },
    /// A divided System Exclusive message that was interrupted by a channel
    /// message or another `F0` event, or never ended in `F7`: its first packet
    /// became a [`SysEx::Complete`] message, the `F7` the writer adds ending
    /// it. The `F7` events that followed are [`Escape`](FileEvent::Escape)s.
    SysExClosed {
        /// The track.
        track: usize,
        /// The tick of the message's first packet.
        at: Tick,
    },
}

impl ReadRepair {
    /// The track the change was made in.
    fn track(&self) -> usize {
        match self {
            Self::Lead { track, .. }
            | Self::DefaultTempo { track }
            | Self::DefaultTimeSignature { track }
            | Self::SysExEscaped { track, .. }
            | Self::SysExClosed { track, .. } => *track,
        }
    }
}

/// The opening metas of a track other than its name: what a [`Pattern`] or the
/// [`LeadTrack`] holds beside its [`Track`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Opening {
    /// The Sequence Number.
    pub(crate) number: Option<u16>,
    /// The SMPTE Offset.
    pub(crate) smpte_start: Option<SmpteOffset>,
}

impl Opening {
    /// The sequence number of the track at `index` in its file: its own, or
    /// the place it has in the file.
    fn number_at(self, index: usize) -> u16 {
        self.number
            .unwrap_or_else(|| u16::try_from(index).unwrap_or(u16::MAX))
    }
}

/// The first track of a format 0 or 1 file: a [`Track`] and the three metas
/// SMF 1.0 section 3.3 puts in the first track, the file's Sequence Number,
/// SMPTE Offset and copyright notice. They belong to the lead and to no other
/// type a file is built from, so a file cannot hold one anywhere else
/// ([`Layout::Format0`], [`Layout::Format1`]). The writer puts them at tick 0
/// before every event of the track, in tutti's order: copyright, Sequence
/// Number, Track Name, SMPTE Offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadTrack {
    track: Track,
    opening: Opening,
    copyright: Option<Box<[u8]>>,
}

impl LeadTrack {
    /// The lead of a file: `track` with its events and name, and none of the
    /// file's opening metas until [`numbered`](Self::numbered),
    /// [`smpte_start`](Self::smpte_start) and [`copyright`](Self::copyright)
    /// give them.
    #[must_use]
    pub fn new(track: Track) -> Self {
        Self {
            track,
            opening: Opening::default(),
            copyright: None,
        }
    }

    /// The file's Sequence Number is `number` (replacing the one it has),
    /// written at tick 0 where SMF 1.0 section 3.3 puts it: a format 0 or 1
    /// file holds one sequence, whose number is in its first track.
    #[must_use]
    pub fn numbered(mut self, number: u16) -> Self {
        self.opening.number = Some(number);
        self
    }

    /// The file starts at the SMPTE time `offset` (replacing the one it has):
    /// its SMPTE Offset is written at tick 0, where SMF 1.0 section 3.3 says
    /// it belongs, and in a format 1 file it is stored with the tempo map.
    #[must_use]
    pub fn smpte_start(mut self, offset: SmpteOffset) -> Self {
        self.opening.smpte_start = Some(offset);
        self
    }

    /// A copyright notice, written as the file's first event at tick 0 (SMF
    /// 1.0 section 3.3: the first event of the first track chunk). A file
    /// holds one notice: a second is appended to it on a new line.
    #[must_use]
    pub fn copyright(mut self, notice: &str) -> Self {
        self.copyright = Some(append_notice(self.copyright.take(), notice));
        self
    }
}

/// One sequence of a format 2 file: a [`Track`] with its own Sequence Number
/// and SMPTE Offset (SMF 1.0 section 3.3: in a format 2 file the number
/// identifies each pattern, and the offset says where its track chunk starts).
/// The writer puts them at tick 0 before every event, in tutti's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    track: Track,
    opening: Opening,
}

impl Pattern {
    /// The pattern `track`, numbered by its place in the file until
    /// [`numbered`](Self::numbered).
    #[must_use]
    pub fn new(track: Track) -> Self {
        Self {
            track,
            opening: Opening::default(),
        }
    }

    /// The pattern's Sequence Number is `number` (replacing the one it has),
    /// which a Cue message in a song refers to.
    #[must_use]
    pub fn numbered(mut self, number: u16) -> Self {
        self.opening.number = Some(number);
        self
    }

    /// The pattern starts at the SMPTE time `offset` (replacing the one it
    /// has).
    #[must_use]
    pub fn smpte_start(mut self, offset: SmpteOffset) -> Self {
        self.opening.smpte_start = Some(offset);
        self
    }
}

/// The tracks of an SMF by its format: each format holds the opening metas of
/// SMF 1.0 section 3.3 where the specification allows them, so none can be
/// misplaced ([`MidiFile::smf`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// Format 0: one track holding everything.
    Format0(LeadTrack),
    /// Format 1: simultaneous tracks of one timeline. The lead is the first
    /// track, which carries the file's opening metas and, by tutti's rule
    /// ([`TracksError::TempoMapNotInFirstTrack`]), the tempo map.
    Format1 {
        /// The first track.
        lead: LeadTrack,
        /// The tracks after it.
        tracks: Vec<Track>,
    },
    /// Format 2: independent sequences, each with its own timeline.
    Format2 {
        /// The copyright notice of the file, written as the first event of
        /// the first pattern (SMF 1.0 section 3.3). Notices of several pieces
        /// go together in this one.
        copyright: Option<String>,
        /// The patterns, in file order.
        patterns: Vec<Pattern>,
    },
}

impl Layout {
    /// The format, the tracks in file order, each one's opening metas, and
    /// the notice written in the first.
    fn into_parts(self) -> (SmfFormat, Vec<Track>, Vec<Opening>, Option<Box<[u8]>>) {
        match self {
            Self::Format0(lead) => (
                SmfFormat::SingleTrack,
                vec![lead.track],
                vec![lead.opening],
                lead.copyright,
            ),
            Self::Format1 { lead, tracks } => {
                let openings = vec![Opening::default(); tracks.len()];
                (
                    SmfFormat::Parallel,
                    std::iter::once(lead.track).chain(tracks).collect(),
                    std::iter::once(lead.opening).chain(openings).collect(),
                    lead.copyright,
                )
            }
            Self::Format2 {
                copyright,
                patterns,
            } => (
                SmfFormat::Sequential,
                patterns.iter().map(|p| p.track.clone()).collect(),
                patterns.iter().map(|p| p.opening).collect(),
                copyright.map(|notice| notice.as_bytes().into()),
            ),
        }
    }
}

/// `held` with `notice` on a new line after it.
fn append_notice(held: Option<Box<[u8]>>, notice: &str) -> Box<[u8]> {
    match held {
        Some(held) => [&*held, b"\n", notice.as_bytes()].concat().into(),
        None => notice.as_bytes().into(),
    }
}

/// A track: events in order, each at an absolute [`Tick`], and where the track
/// ends.
///
/// An SMF's track is one `MTrk` chunk and a Clip File's one sequence. Of the
/// four meta events SMF 1.0 section 3.3 puts at the start of a track, the Track
/// Name is the track's field ([`named`](Self::named)), not an event. The
/// Sequence Number, the SMPTE Offset and the copyright notice are held beside
/// the track by a [`LeadTrack`] or a [`Pattern`], where the specification
/// allows them, so a track a file hands out carries none of them and cannot be
/// placed where they do not belong. The writer puts them at tick 0 before every
/// event, in tutti's order: copyright (section 3.3: "the first event in the
/// first track chunk"), Sequence Number, Track Name, SMPTE Offset; the
/// specification does not order the other three among themselves. They are
/// written ahead of every other event at tick 0, so a track read with a Set
/// Tempo and then a Track Name at tick 0 writes back with the Track Name
/// first. So a track cannot hold one late. A Clip File's track has none of
/// them.
///
/// The builder methods ([`note`](Self::note), [`meta`](Self::meta),
/// [`push`](Self::push)) and [`Extend`] keep the events in tick order and the
/// end at or after the last one, so a track a caller builds is always ordered;
/// the fields are read through [`events`](Self::events), [`end`](Self::end),
/// [`header`](Self::header) and [`header_end`](Self::header_end).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Track {
    /// The Track Name's bytes.
    pub(crate) name: Option<Box<[u8]>>,
    /// A Clip File's configuration header (M2-116 section 6), in order.
    pub(crate) header: Vec<(Tick, FileEvent)>,
    /// Where the header ends, on the header's clock.
    pub(crate) header_end: Tick,
    /// The events, in file order. Their ticks do not decrease.
    pub(crate) events: Vec<(Tick, FileEvent)>,
    /// Where the track ends. Not before the last event.
    pub(crate) end: Tick,
}

impl Track {
    /// An empty track, ending at tick 0. Events are added with the builder
    /// ([`note`](Self::note), [`meta`](Self::meta),
    /// [`push`](Self::push)) or by [`collect`](Iterator::collect)ing or
    /// [`extend`](Extend::extend)ing `(Tick, FileEvent)` pairs.
    ///
    /// ```
    /// use tutti_midi_file::{Meta, Tick, Track};
    /// use tutti_units::Bpm;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let track = Track::new()
    ///     .meta(Tick(0), Meta::tempo(Bpm::of(90.0)).unwrap())
    ///     .note(Tick(0), Tick(480), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
    ///     .note(Tick(480), Tick(960), MidiChannel::FIRST, NoteNumber::of(64), U7::of(100));
    /// assert_eq!(track.end(), Tick(960));
    /// assert_eq!(track.events().len(), 5);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A track a decoder or converter assembled: parts that are in order by
    /// construction (each tick is the sum of the deltas before it, and each end
    /// is at or after its run's last event). Not checked in release, so only
    /// those call it; a track a caller builds goes through the ordering
    /// builders.
    pub(crate) fn from_read(
        header: Vec<(Tick, FileEvent)>,
        header_end: Tick,
        events: Vec<(Tick, FileEvent)>,
        end: Tick,
    ) -> Self {
        debug_assert!(header.is_sorted_by_key(|&(tick, _)| tick));
        debug_assert!(events.is_sorted_by_key(|&(tick, _)| tick));
        debug_assert!(header.last().is_none_or(|&(tick, _)| tick <= header_end));
        debug_assert!(events.last().is_none_or(|&(tick, _)| tick <= end));
        Self {
            header,
            header_end,
            events,
            end,
            ..Self::default()
        }
    }

    /// A Clip File's configuration header (M2-116 section 6): the messages
    /// between the file header and Start of Clip that configure a receiver
    /// (Set Profile On, Bank Select, Program Change, volume, System
    /// Exclusive, and at most one Set Tempo and one Set Time Signature), in
    /// order. They are *not* events of the sequence and do not sound at its
    /// start. Their ticks are the header's own clock, counted from the DCTPQ;
    /// the leading Set Profile On messages are at 0. Empty for an SMF.
    #[must_use]
    pub fn header(&self) -> &[(Tick, FileEvent)] {
        &self.header
    }

    /// Where the header ends, on the header's clock: the tick of Start of Clip,
    /// which is the last header message's tick plus the Delta Clockstamp before
    /// Start of Clip. M2-116 section 6.1: the Delta Clockstamps up to and
    /// including the one for Start of Clip add up to the time allocated to the
    /// Clip Configuration Header, which a receiver may need to configure itself.
    /// Not before the last header message. `Tick(0)` for an SMF, and for a
    /// header that asks for no time after its last message.
    #[must_use]
    pub fn header_end(&self) -> Tick {
        self.header_end
    }

    /// The events, in file order; their ticks do not decrease.
    #[must_use]
    pub fn events(&self) -> &[(Tick, FileEvent)] {
        &self.events
    }

    /// Where the track ends: an SMF's End of Track, a Clip File's End of
    /// Clip. Not before the last event.
    #[must_use]
    pub fn end(&self) -> Tick {
        self.end
    }

    /// Places `event` at `tick`, after the events already at that tick, and
    /// moves the track's end to `tick` if it is past it.
    fn place(&mut self, tick: Tick, event: FileEvent) {
        let at = self.events.partition_point(|&(t, _)| t <= tick);
        self.events.insert(at, (tick, event));
        self.end = self.end.max(tick);
    }

    /// A note: a Note On at `start` and its Note Off at `end`, on `channel`
    /// (a bare [`MidiChannel`] is in the first Group, as an SMF holds). The
    /// Note Off carries the MIDI release velocity of 64, which means none. An
    /// `end` before `start` is `start`: a note of no length.
    ///
    /// Events are placed in tick order, after the events already at their
    /// tick, so notes may be added in any order, overlap, or start together
    /// (a chord). At one tick the events are in the order they were added: a
    /// Note Off lands before the Note On of a note that starts where it ends
    /// only when its note was added first.
    #[must_use]
    pub fn note(
        mut self,
        start: Tick,
        end: Tick,
        channel: impl Into<Address>,
        note: NoteNumber,
        velocity: U7,
    ) -> Self {
        let address = channel.into();
        self.place(
            start,
            Message::midi1(address, Voice1::note_on(note, velocity)).into(),
        );
        self.place(
            end.max(start),
            Message::midi1(address, Voice1::note_off(note, U7::of(64))).into(),
        );
        self
    }

    /// A meta event at `tick`, placed in tick order as [`note`](Self::note) is.
    /// This is the one way to add a tempo: `.meta(tick, Meta::tempo(bpm))`.
    ///
    /// A second Set Tempo at the same tick is kept, after the first, and not
    /// refused (an SMF may hold any number): the later one governs the file's
    /// [`tempo_map`](MetricalView::tempo_map), as a sequencer applying them in order
    /// would.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_midi_file::{Meta, Tick, Track};
    /// use tutti_units::Bpm;
    ///
    /// let track = Track::new().meta(Tick(0), Meta::tempo(Bpm::of(90.0)).unwrap());
    /// assert_eq!(track.events().len(), 1);
    /// ```
    #[must_use]
    pub fn meta(mut self, tick: Tick, meta: Meta) -> Self {
        self.place(tick, meta.into());
        self
    }

    /// Appends `event` at `tick`: a message (a [`Message`] built with [`Message::midi1`] and its
    /// kin), a [`Meta`], or any [`FileEvent`]. Where [`note`](Self::note)
    /// places an event by its tick, this one is the strict door: the track's
    /// events do not go back in time. The track's end moves to `tick` if it is
    /// past it.
    ///
    /// # Errors
    ///
    /// [`OutOfOrder`] if `tick` is before the last event's, naming both; the
    /// event is not added.
    pub fn push(
        mut self,
        tick: Tick,
        event: impl Into<FileEvent>,
    ) -> std::result::Result<Self, OutOfOrder> {
        if let Some(&(last, _)) = self.events.last()
            && tick < last
        {
            return Err(OutOfOrder { last, at: tick });
        }
        self.events.push((tick, event.into()));
        self.end = self.end.max(tick);
        Ok(self)
    }

    /// The track ends at `end`, or where it ends already if that is later: the
    /// end is never before the last event, so a track stays as ordered as the
    /// constructors need it. An SMF's End of Track and a Clip File's End of
    /// Clip are written there.
    ///
    /// ```
    /// use tutti_midi_file::{Tick, Track};
    ///
    /// let track = Track::new().ending_at(Tick(1_920));
    /// assert_eq!(track.end(), Tick(1_920));
    /// assert_eq!(track.ending_at(Tick(0)).end(), Tick(1_920));
    /// ```
    #[must_use]
    pub fn ending_at(mut self, end: Tick) -> Self {
        self.end = self.end.max(end);
        self
    }

    /// The track's Track Name is `name` (replacing the one it has), written at
    /// tick 0 (SMF 1.0 section 3.3: it appears at time 0). The name of a format
    /// 0 track or of the first track of a format 1 file names the sequence.
    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        self.name = Some(name.as_bytes().into());
        self
    }

    /// The track's name as the file stores it (no encoding is declared), if it
    /// has one.
    #[must_use]
    pub fn name_bytes(&self) -> Option<&[u8]> {
        self.name.as_deref()
    }

    /// The events with the channel each belongs to (SMF 1.0 section 3.3, MIDI
    /// Channel Prefix): a channel message its own, and a SysEx or meta event
    /// the channel of the prefix before it, effective until the next channel
    /// message or prefix; `None` with no prefix in effect. A prefix event
    /// itself is `None`: it names the events that follow it.
    pub fn events_with_channel(
        &self,
    ) -> impl Iterator<Item = (Tick, &FileEvent, Option<MidiChannel>)> {
        let mut prefix = None;
        self.events.iter().map(move |(tick, event)| {
            let channel = match event {
                FileEvent::Message(Message::Midi1(address, _)) => {
                    prefix = None;
                    Some(address.channel)
                }
                FileEvent::Meta(Meta::ChannelPrefix { channel, .. }) => {
                    prefix = Some(*channel);
                    None
                }
                _ => prefix,
            };
            (*tick, event, channel)
        })
    }

    /// A Set Profile On at the start of the Clip Configuration Header, after
    /// the ones already there (M2-116 section 6.2: data meant for a Profile is
    /// tagged with a Set Profile On, with that Profile Id, at the beginning of
    /// the header). The message is at tick 0 and carries no Delta Clockstamp
    /// when written (section 6); it is a [`ProfileOnRequest`], whose MUIDs are
    /// Broadcast. A Clip File's header only: an SMF's is refused when the file
    /// is built.
    ///
    /// ```
    /// use tutti_midi_ci::{ProfileId, ProfileOnRequest, ProfileTarget};
    /// use tutti_midi_file::Track;
    ///
    /// let on = ProfileOnRequest { target: ProfileTarget::Group, profile: ProfileId::MPE };
    /// let track = Track::new().profile_on(on);
    /// assert_eq!(track.profiles(), vec![on]);
    /// ```
    #[must_use]
    pub fn profile_on(mut self, request: ProfileOnRequest) -> Self {
        let leading = clip_file::leading_profile_len(&packets(&self.header));
        let packets = clip_file::profile_packets(request).map(|ump| (Tick(0), to_clip::event(ump)));
        self.header.splice(leading..leading, packets);
        self
    }

    /// The Set Profile On messages at the head of the header, in order, which
    /// section 6.2 puts there. A header holds only ones that read: a file
    /// whose do not is refused when it is read, and
    /// [`profile_on`](Self::profile_on) writes only whole ones.
    #[must_use]
    pub fn profiles(&self) -> Vec<ProfileOnRequest> {
        let packets = packets(&self.header);
        let leading = clip_file::leading_profile_len(&packets);
        clip_file::profiles(packets[..leading].iter().map(|&(_, ump)| ump))
            .inspect(|request| {
                // A header holds only Set Profile On messages that read: a
                // read file's are checked by `check_header` when it is built,
                // and `profile_on` writes whole ones.
                debug_assert!(request.is_ok(), "a built header's profiles read");
            })
            .flatten()
            .collect()
    }

    /// The track's events as the packets a clip plays, at their beats for a
    /// file of division `ticks_per_quarter`, in the track's order. The rule is
    /// [`MetricalView::timeline`]'s.
    pub fn timeline(&self, ticks_per_quarter: TicksPerQuarterNote) -> SmfTimeline {
        self.timeline_by(|tick| tick.beats(ticks_per_quarter))
    }

    /// The track's events as the packets a clip plays, at the beats `beat_at`
    /// gives their ticks, in the track's order.
    fn timeline_by(&self, beat_at: impl Fn(Tick) -> Beat) -> SmfTimeline {
        SmfTimeline::of(
            self.events
                .iter()
                .map(|(tick, event)| (beat_at(*tick), event)),
        )
    }

    /// The track's name, if it has one and it is UTF-8 text
    /// ([`name_bytes`](Self::name_bytes) is the bytes).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        std::str::from_utf8(self.name.as_deref()?).ok()
    }

    /// The track's notes, a Note On paired with its Note Off, in order of
    /// onset and then `(group, channel, note)`.
    ///
    /// Pairing is keyed on `(group, channel, note)`: a Note Off only closes a
    /// Note On on the same group and channel, overlapping identical notes close
    /// last-on first-off, and a note still open at the track's end is dropped
    /// (its length is unknowable). A MIDI 1.0 Note On at velocity 0 is a Note
    /// Off, as the MIDI 1.0 specification says; a MIDI 2.0 one is neither a
    /// start nor an end.
    ///
    /// This walks the whole track, allocates the list and sorts it on every
    /// call: keep the result rather than calling it again.
    #[must_use]
    pub fn notes(&self) -> Vec<PairedNote> {
        use std::collections::BTreeMap;

        let mut held: BTreeMap<(MidiGroup, MidiChannel, NoteNumber), Vec<(Tick, NoteVelocity)>> =
            BTreeMap::new();
        let mut out: Vec<PairedNote> = Vec::new();

        for (tick, event) in &self.events {
            let Some(message) = event.message() else {
                continue;
            };
            let ump = Ump::from(*message);
            // `note_key_event` reads the status, group, channel and note bits of a
            // MIDI 1.0 or 2.0 channel-voice packet and counts a MIDI 1.0 Note
            // On at velocity 0 as an off, as `Ump::is_note_off` does.
            let Some(note_key_event @ (NoteKeyEvent::On(key) | NoteKeyEvent::Off(key))) =
                ump.note_key_event()
            else {
                continue;
            };
            let Address { group, channel } = key.address();
            let note = key.note();
            let map_key = (group, channel, note);
            if matches!(note_key_event, NoteKeyEvent::Off(_)) {
                if let Some((start, velocity)) = held.get_mut(&map_key).and_then(Vec::pop) {
                    out.push(PairedNote {
                        group,
                        channel,
                        note,
                        velocity,
                        start,
                        end: (*tick).max(start),
                    });
                }
            } else {
                // A MIDI 1.0 Note On translates (`channel_voice`), a MIDI 2.0 one
                // keeps its 16 bits; a MIDI 2.0 Note On at velocity 0 is
                // neither a start nor an end.
                if let Some((_, Voice2::NoteOn { velocity, .. })) = channel_voice(ump)
                    && velocity.get() > 0
                {
                    held.entry(map_key).or_default().push((*tick, velocity));
                }
            }
        }

        // Onset order, then key, so simultaneous notes have a stable order
        // rather than one that depends on when their Note Offs arrived.
        out.sort_by_key(|n| (n.start, n.group, n.channel, n.note));
        out
    }
}

/// A track of the events, in tick order: a stable sort, so events at one tick
/// stay in the order given. Ends at the last of them.
impl FromIterator<(Tick, FileEvent)> for Track {
    fn from_iter<I: IntoIterator<Item = (Tick, FileEvent)>>(events: I) -> Self {
        let mut track = Track::new();
        track.extend(events);
        track
    }
}

/// Adds the events, then puts the track in tick order (a stable sort, so
/// events at one tick stay in the order added) and moves its end to the last
/// event's if that is past it.
impl Extend<(Tick, FileEvent)> for Track {
    fn extend<I: IntoIterator<Item = (Tick, FileEvent)>>(&mut self, events: I) {
        self.events.extend(events);
        self.events.sort_by_key(|&(tick, _)| tick);
        if let Some(&(last, _)) = self.events.last() {
            self.end = self.end.max(last);
        }
    }
}

/// A note paired from its Note On and Note Off ([`Track::notes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairedNote {
    /// The UMP group (0 for an SMF). Part of the pairing key: groups are
    /// independent 16-channel spaces.
    pub group: MidiGroup,
    /// The channel.
    pub channel: MidiChannel,
    /// The key.
    pub note: NoteNumber,
    /// The Note On velocity at MIDI 2.0 width: a MIDI 1.0 velocity is translated
    /// by the specification's scaling, so this is lossless in both directions.
    pub velocity: NoteVelocity,
    /// Where the note starts.
    pub start: Tick,
    /// Where it ends; not before `start`.
    pub end: Tick,
}

/// The [`Lead`]s of each track in the order the writer puts them first at tick
/// 0: the copyright notice (the first track's), the Sequence Number, the Track
/// Name, the SMPTE Offset (tutti's order, see [`Track`]).
pub(crate) fn leads(
    tracks: &[Track],
    openings: &[Opening],
    copyright: Option<&[u8]>,
) -> Vec<Vec<Lead>> {
    tracks
        .iter()
        .zip(openings)
        .enumerate()
        .map(|(index, (track, opening))| {
            [
                copyright
                    .filter(|_| index == 0)
                    .map(|notice| Lead::Copyright(notice.into())),
                opening.number.map(Lead::SequenceNumber),
                track.name.clone().map(Lead::TrackName),
                opening.smpte_start.map(Lead::SmpteOffset),
            ]
            .into_iter()
            .flatten()
            .collect()
        })
        .collect()
}

/// The container a file was read from, with its division, and with it the
/// rules it is written by. An SMF's metrical division has 15 bits, a Clip
/// File's 16; an SMF may count in SMPTE time instead.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Container {
    Smf(SmfFormat, SmfDivision),
    Clip(TicksPerQuarterNote),
}

impl Container {
    /// The file's division as ticks per quarter note, `None` for SMPTE time.
    fn ticks_per_quarter(self) -> Option<TicksPerQuarterNote> {
        match self {
            Self::Clip(ticks_per_quarter) => Some(ticks_per_quarter),
            Self::Smf(_, SmfDivision::Metrical(ticks)) => Some(ticks.into()),
            Self::Smf(_, SmfDivision::Smpte { .. }) => None,
        }
    }

    /// Whether the tracks are on one timeline: a Clip File's, and an SMF of
    /// format 0 or 1.
    fn shares_timeline(self) -> bool {
        match self {
            Self::Smf(format, _) => format.shares_timeline(),
            Self::Clip(_) => true,
        }
    }
}

/// A MIDI file of either container, with every event it holds.
///
/// Fields are private and the tempo map is derived from the tracks (their
/// Set Tempo events), so the two never disagree; [`tracks`](Self::tracks) and
/// the accessors read it, and [`smf`](Self::smf) and [`clip`](Self::clip)
/// build one and check what a container can hold.
///
/// Format 2 SMFs keep their sequences apart, and so do those of a format above
/// 2: a format 0 or 1 file and a Clip File share one timeline and a format 2
/// file has one per track, so [`timebase`](Self::timebase) hands out a view
/// that merges the tracks ([`MetricalView`], [`SmpteView`]) for the first and
/// a view of independent sequences ([`MetricalSequences`], [`SmpteSequences`])
/// for the second, and a question about one timeline is asked only of a file
/// that has one.
///
/// An SMF in SMPTE time ([`SmfDivision::Smpte`]) counts its ticks in fractions
/// of a second and has no beat grid: [`timebase`](Self::timebase) says which
/// a file is, and only the metrical views have beats of their own, a tempo map
/// and a clip; an SMPTE file's seconds are placed on beats over a tempo map the
/// caller gives ([`SmpteView::timelines`]).
///
/// ```
/// use tutti_midi_file::{LeadTrack, Layout, Meta, MidiFile, SmfDivision, Tick, Track};
/// use tutti_units::{Bpm, Seconds};
/// use tutti_ump::{MidiChannel, NoteNumber, U7};
///
/// let track = Track::new()
///     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)).unwrap())
///     .note(Tick(0), Tick(480), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// let file = MidiFile::smf(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(track)))?;
///
/// let mut bytes = Vec::new();
/// file.write(&mut bytes)?;
/// let again = MidiFile::read(bytes)?;
/// assert_eq!(again, file);
/// let metrical = again.timebase().metrical().unwrap();
/// assert_eq!(metrical.seconds_at(Tick(480)), Seconds::new(0.5));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct MidiFile {
    container: Container,
    tracks: Vec<Track>,
    /// The Sequence Number and SMPTE Offset of each track, in track order.
    openings: Vec<Opening>,
    /// The copyright notice, written as the first event of the first track.
    copyright: Option<Box<[u8]>>,
    /// One map for a file of one timeline, one per track for a format 2 file;
    /// none for SMPTE time.
    tempo_maps: Vec<Arc<TempoMap>>,
    /// What a read changed to make the tracks writable.
    repairs: Vec<ReadRepair>,
}

/// Two files are equal when their container, division and tracks are; the
/// tempo maps are derived from the tracks and the report of a read is not
/// part of the file.
impl PartialEq for MidiFile {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container
            && self.tracks == other.tracks
            && self.openings == other.openings
            && self.copyright == other.copyright
    }
}

impl Eq for MidiFile {}

impl MidiFile {
    /// The extensions a MIDI file of either container is found under.
    ///
    /// `.mid` and `.midi` are the Standard MIDI File's; a Clip File "shall use
    /// the file extension .midi2" (M2-116 section 4.1). A reader handles all
    /// three the same way, detecting rather than trusting any of them.
    pub const EXTENSIONS: &'static [&'static str] = &["mid", "midi", "midi2"];

    /// An SMF of `division` over the tracks of `layout`, whose variant is the
    /// format.
    ///
    /// # Errors
    ///
    /// [`TracksError`] if a format 2 file has no pattern, a track's events are out of order or past its end, two
    /// events are further apart than the wire's 28-bit delta, a message is not
    /// a MIDI 1.0 channel message of the first Group, an event or the file is
    /// larger than the format's length fields hold (the error names which and
    /// where), the tempo events do not make a [`TempoMap`], or the events break
    /// a rule SMF 1.0 gives a writer: a SysEx message that does not end in `F7`
    /// or has a channel message between its packets. tutti's writer also
    /// refuses what the specification only recommends: the format 1 tempo map
    /// outside the first track. The placement of the Sequence Number, SMPTE
    /// Offset and copyright notice is the [`Layout`]'s. A file of format 3 or
    /// above cannot be authored, only written back as it was read.
    ///
    /// The file specifies its tempo and time signature (SMF 1.0 section 2.2:
    /// without them a reader assumes 120 beats a minute in 4/4): a Set Tempo
    /// and a Time Signature that no track of a format 0 or 1 file (they share
    /// one timeline) or that a format 2 pattern does not have at tick 0 are
    /// added at tick 0 of the first track or of the pattern, after the meta
    /// events that open it, as 120 BPM and 4/4 with 24 clocks a click and 8
    /// 32nd notes to the quarter. [`tracks`](Self::tracks) holds them. A file
    /// in SMPTE time gets them too, where they are inert. A format above 2
    /// cannot be authored; a read treats it as format 2, each track its own
    /// pattern, as SMF 1.0 section 2.1 lets a reader.
    #[must_use = "an Err means no file was built"]
    pub fn smf(division: SmfDivision, layout: Layout) -> Result<Self, TracksError> {
        let (format, tracks, openings, copyright) = layout.into_parts();
        Self::build(
            Container::Smf(format, division),
            tracks,
            openings,
            copyright,
            true,
        )
    }

    /// A Clip File of division `ticks_per_quarter` over one sequence and its
    /// [`header`](Track::header).
    ///
    /// # Errors
    ///
    /// [`TracksError`] if the events or the header's are out of order or the
    /// events past the track's end, the header ends before its last message, a
    /// Flex Data message is interrupted by another message of its Group or
    /// left open (M2-104 section 7.5.2: only its own Continue packets and
    /// System Real Time messages go between its Start and End; a message that
    /// starts in the header and ends in the events is left open), an event is
    /// not a packet (`SysEx`, `Escape` and `Meta` are SMF's), is a MIDI-CI
    /// Property Exchange message (sections 6.4 and 7.2) or is the file's own
    /// structure (a Delta Clockstamp, the DCTPQ, a Null, Start or End of
    /// Clip), the header breaks a rule of sections 6.1 to 6.1.2 (a second Set
    /// Tempo or Set Time Signature, one misplaced or not at delta 0), a gap
    /// needs more Null restarts than the writer holds, or the Set Tempo
    /// messages do not make a [`TempoMap`].
    ///
    /// Only this constructor holds a clip to M2-104 section 7.5.2: a file read
    /// with [`read`](Self::read) keeps the Flex Data messages as
    /// its sender interleaved them. The rule is checked on the header and on
    /// the events each by itself, so a message that starts in the header and
    /// ends in the events is reported open. Its exemption for JR Clock
    /// messages holds without a check, as a clip refuses every Utility
    /// message.
    pub fn clip(ticks_per_quarter: TicksPerQuarterNote, track: Track) -> Result<Self, TracksError> {
        for events in [&track.header, &track.events] {
            if let Some(at) = text_interrupted(events) {
                return Err(TracksError::TextInterrupted { at });
            }
        }
        Self::build(
            Container::Clip(ticks_per_quarter),
            vec![track],
            vec![Opening::default()],
            None,
            true,
        )
    }

    /// The file of `tracks`, held to the container's rules. `authoring` adds
    /// the rules a writer keeps ([`check_smf`]): a read keeps a file that
    /// breaks them.
    fn build(
        container: Container,
        tracks: Vec<Track>,
        openings: Vec<Opening>,
        copyright: Option<Box<[u8]>>,
        authoring: bool,
    ) -> Result<Self, TracksError> {
        if tracks.is_empty() {
            return Err(TracksError::NoTracks);
        }
        if tracks.len() > usize::from(u16::MAX) {
            return Err(TracksError::TooManyTracks {
                tracks: tracks.len(),
            });
        }
        for (i, track) in tracks.iter().enumerate() {
            if matches!(container, Container::Clip(_)) && track.name.is_some() {
                return Err(TracksError::Refused {
                    track: i,
                    part: TrackPart::Events,
                    refusal: Refusal::ClipSmfEvent,
                });
            }
            if !track.header.is_empty() || track.header_end != Tick(0) {
                if !matches!(container, Container::Clip(_)) {
                    return Err(TracksError::HeaderInSmf { track: i });
                }
                check_run(
                    container,
                    i,
                    TrackPart::Header,
                    &track.header,
                    track.header_end,
                )?;
                clip_file::check_header(&packets(&track.header))?;
            }
            check_run(container, i, TrackPart::Events, &track.events, track.end)?;
        }
        let mut tracks = tracks;
        let mut defaulted = Vec::new();
        match container {
            Container::Smf(format, _) => {
                if authoring {
                    check_smf(format, &tracks)?;
                }
                defaulted = specify_tempo_and_meter(format, &mut tracks);
                smf::measure(&tracks, &leads(&tracks, &openings, copyright.as_deref()))?;
            }
            Container::Clip(_) => {
                let track = &tracks[0];
                if clip_file::restart_bytes(track.header_end.max(track.end)) > u64::from(u32::MAX) {
                    return Err(TracksError::TooLarge);
                }
            }
        }
        // A tempo map is a grid of quarter notes: SMPTE time has none.
        let tempo_maps = match container.ticks_per_quarter() {
            None => Vec::new(),
            Some(ticks_per_quarter) if container.shares_timeline() => {
                vec![tempo_map(container, ticks_per_quarter, &tracks)?]
            }
            Some(ticks_per_quarter) => tracks
                .iter()
                .map(|t| tempo_map(container, ticks_per_quarter, std::slice::from_ref(t)))
                .collect::<Result<_, _>>()?,
        };
        Ok(Self {
            container,
            tracks,
            openings,
            copyright,
            tempo_maps,
            repairs: if authoring { Vec::new() } else { defaulted },
        })
    }

    /// Reads and decodes a complete MIDI file from `src` (any `impl Into<Asset>`:
    /// a path, bytes or an app's own source), detecting the container from its
    /// magic: `MThd` for an SMF, `SMF2CLIP` for a Clip File.
    ///
    /// ```
    /// # use tutti_midi_file::{LeadTrack, Layout, MidiFile, SmfDivision, Track};
    /// let file = MidiFile::smf(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(Track::new())))?;
    /// let mut bytes = Vec::new();
    /// file.write(&mut bytes)?;
    /// assert_eq!(MidiFile::read(bytes)?, file);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - [`ReadError::Io`] if the source cannot be opened or read.
    /// - [`ReadError::UnknownFormat`] when the magic matches neither container
    ///   (no decoder ran).
    /// - [`ReadError::Smf`] for a malformed or cut-off SMF, a zero division
    ///   included, with the track and byte offset it was found at.
    /// - [`ReadError::Clip`] for a malformed or cut-off Clip File, naming the
    ///   rule it broke and the word it was found at.
    /// - [`ReadError::Tracks`] if the tempo events do not make a
    ///   [`TempoMap`].
    pub fn read(src: impl Into<Asset>) -> Result<Self, ReadError> {
        use std::io::Read;

        let mut bytes = Vec::new();
        src.into()
            .open()
            .and_then(|mut reader| reader.read_to_end(&mut bytes))
            .map_err(ReadError::Io)?;
        Self::decode(&bytes)
    }

    /// Decodes `bytes`, detecting the container from its magic.
    fn decode(bytes: &[u8]) -> Result<Self, ReadError> {
        match MidiFileKind::of_header(bytes) {
            Some(MidiFileKind::StandardMidiFile) => {
                let parts = smf::read(bytes)?;
                let mut file = Self::build(
                    Container::Smf(parts.format, parts.division),
                    parts.tracks,
                    parts.openings,
                    parts.copyright,
                    false,
                )?;
                let defaulted = std::mem::take(&mut file.repairs);
                file.repairs = parts.repairs;
                file.repairs.extend(defaulted);
                file.repairs.sort_by_key(ReadRepair::track);
                Ok(file)
            }
            Some(MidiFileKind::ClipFile) => {
                let clip = clip_file::parse(&bytes[clip_file::CLIP_FILE_MAGIC.len()..])?;
                let events = |packets: Vec<(Tick, Ump)>| -> Vec<(Tick, FileEvent)> {
                    packets
                        .into_iter()
                        .map(|(tick, ump)| {
                            let event = match ump.decode() {
                                Ok(message) => FileEvent::Message(message),
                                Err(unknown) => FileEvent::Unknown(unknown),
                            };
                            (tick, event)
                        })
                        .collect()
                };
                let track = Track::from_read(
                    events(clip.header),
                    clip.header_end,
                    events(clip.events),
                    clip.end,
                );
                Ok(Self::build(
                    Container::Clip(clip.ticks_per_quarter),
                    vec![track],
                    vec![Opening::default()],
                    None,
                    false,
                )?)
            }
            None => Err(ReadError::UnknownFormat),
        }
    }

    /// The file as bytes, in its own container: every event written back at
    /// its tick. The constructors have checked everything the writer needs, so
    /// this cannot fail.
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self.container {
            Container::Smf(format, division) => smf::write(
                format,
                division,
                &self.tracks,
                &leads(&self.tracks, &self.openings, self.copyright.as_deref()),
            ),
            Container::Clip(ticks_per_quarter) => {
                let track = &self.tracks[0];
                clip_file::write(
                    ticks_per_quarter,
                    &packets(&track.header),
                    track.header_end,
                    &packets(&track.events),
                    track.end,
                )
            }
        }
    }

    /// Writes the file to `dst` (any `impl Into<Destination>`: a path, or a
    /// `&mut` [`Write`](std::io::Write) such as a `Vec<u8>`), in its own
    /// container: every event written back at its tick. Running status and the
    /// spelling of a Delta Clockstamp may differ from the file it was read
    /// from; a read of the result is equal to this file.
    ///
    /// A path is written through [`Staged`](tutti_platform::Staged): the bytes go to a temporary file
    /// beside the path and are put in place when whole, so a crash leaves no
    /// partial file under it and what was there intact. It replaces a file
    /// already there, as [`std::fs::write`] does;
    /// [`Destination::create_new`] refuses it instead, atomically. A writer
    /// receives the whole file in one `write_all`, and `create_new` does not
    /// apply to it.
    ///
    /// ```
    /// # use tutti_midi_file::{LeadTrack, Layout, MidiFile, SmfDivision, Track, WriteError};
    /// # use tutti_platform::Destination;
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("song.mid");
    /// let file = MidiFile::smf(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(Track::new())))?;
    /// file.write(&path)?;
    /// file.write(&path)?; // replaces
    /// assert!(matches!(
    ///     file.write(Destination::from(&path).create_new()),
    ///     Err(WriteError::Exists { .. })
    /// ));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`WriteError::Exists`] if something is at a path and `create_new` was
    /// called (the target is untouched, the staged file is kept, and `self`
    /// can be written again); [`WriteError::Stage`] if the temporary cannot be
    /// created or written; [`WriteError::Commit`] if the sync or the move
    /// fails (the staged file is kept); for a writer, [`WriteError::Writer`]
    /// if it fails.
    pub fn write<'a>(&self, dst: impl Into<Destination<'a>>) -> Result<(), WriteError> {
        let bytes = self.encode();
        match dst.into() {
            Destination::Path(staged) => {
                let path = staged.target().to_path_buf();
                let mut file = staged.open().map_err(WriteError::Stage)?;
                file.write_all(&bytes).map_err(WriteError::Stage)?;
                file.commit().map_err(|e| {
                    // Only a refused `create_new` commit is `AlreadyExists`.
                    if e.kind() == ErrorKind::AlreadyExists {
                        WriteError::Exists { path, source: e }
                    } else {
                        WriteError::Commit(e)
                    }
                })
            }
            Destination::Writer(writer) => writer.write_all(&bytes).map_err(WriteError::Writer),
        }
    }

    /// The SMF format word, `None` for a Clip File.
    #[must_use]
    pub fn format(&self) -> Option<SmfFormat> {
        match self.container {
            Container::Smf(format, _) => Some(format),
            Container::Clip(_) => None,
        }
    }

    /// An SMF's division word, `None` for a Clip File (whose division is
    /// always its metrical view's [`ticks_per_quarter`](MetricalView::ticks_per_quarter)).
    #[must_use]
    pub fn division(&self) -> Option<SmfDivision> {
        match self.container {
            Container::Smf(_, division) => Some(division),
            Container::Clip(_) => None,
        }
    }

    /// The file's time, as what its ticks count and whether its tracks share
    /// one timeline. The one place a file's kind is asked; each view answers
    /// only what its kind has.
    pub fn timebase(&self) -> Timebase<'_> {
        let shared = self.container.shares_timeline();
        match self.container {
            Container::Clip(ticks_per_quarter) => Timebase::Metrical(MetricalView {
                file: self,
                ticks_per_quarter,
            }),
            Container::Smf(_, SmfDivision::Metrical(ticks)) if shared => {
                Timebase::Metrical(MetricalView {
                    file: self,
                    ticks_per_quarter: ticks.into(),
                })
            }
            Container::Smf(_, SmfDivision::Metrical(ticks)) => {
                Timebase::MetricalSequences(MetricalSequences {
                    file: self,
                    ticks_per_quarter: ticks.into(),
                })
            }
            Container::Smf(
                _,
                SmfDivision::Smpte {
                    fps,
                    ticks_per_frame,
                },
            ) if shared => Timebase::Smpte(SmpteView {
                file: self,
                fps,
                ticks_per_frame,
            }),
            Container::Smf(
                _,
                SmfDivision::Smpte {
                    fps,
                    ticks_per_frame,
                },
            ) => Timebase::SmpteSequences(SmpteSequences {
                file: self,
                fps,
                ticks_per_frame,
            }),
        }
    }

    /// The tracks, as the file has them (a Clip File's is one).
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The [`Lead`]s of each track, as the writer puts them first.
    pub(crate) fn leads(&self) -> Vec<Vec<Lead>> {
        leads(&self.tracks, &self.openings, self.copyright.as_deref())
    }

    /// What reading this file changed to make it writable, in the order of
    /// the tracks: empty for a file built with [`smf`](Self::smf) or
    /// [`clip`](Self::clip) and for one whose bytes already keep the rules.
    ///
    /// SMF 1.0 binds what a file holds, not what a reader does with one that
    /// does not. Section 3.3: "Sequence Number and Sequence/Track Name events,
    /// if present, must appear at time 0"; the Sequence Number "must occur at
    /// the beginning of a track, before any nonzero delta-times, and before
    /// any transmittable MIDI events" and, "in a format 0 or 1 MIDI file, ...
    /// this number should be contained in the first (or only) track"; the
    /// SMPTE Offset "should be present at the beginning of the track" under
    /// the same words and "in a format 1 file, ... must be stored with the
    /// tempo map, and has no meaning in any of the other tracks"; the
    /// copyright notice "should be the first event in the first track chunk,
    /// at time 0". Section 3: "A syntactic system exclusive message must
    /// always end with an F7" and "there also must not be any transmittable
    /// MIDI events in between the packets of a multi-packet system exclusive
    /// message". None says what a program does with a file that breaks them,
    /// so a reader accepts the file, makes it writable, and records each change
    /// here. Section 2.2 asks every file to specify its tempo and time
    /// signature ("All MIDI Files should specify tempo and time signature. If
    /// they don't, the time signature is assumed to be 4/4, and the tempo 120
    /// beats per minute"), so a read adds the ones that no track of a format 0 or
    /// 1 file, or that a format 2 pattern (or a track of a format above 2),
    /// lacks at tick 0 ([`ReadRepair::DefaultTempo`],
    /// [`ReadRepair::DefaultTimeSignature`]), in an SMPTE division too, where
    /// they are inert.
    #[must_use]
    pub fn repairs(&self) -> &[ReadRepair] {
        &self.repairs
    }

    /// The file's copyright notice as the file stores it (no encoding is
    /// declared), if it has one: the first event of the first track (SMF 1.0
    /// section 3.3).
    #[must_use]
    pub fn copyright(&self) -> Option<&[u8]> {
        self.copyright.as_deref()
    }

    /// The Sequence Number of track `track`, if it has one: the lead's in a
    /// file of one timeline, a pattern's otherwise. `None` for no such track
    /// or number; the sequence views number a track with none by its place.
    #[must_use]
    pub fn sequence_number(&self, track: usize) -> Option<u16> {
        self.openings.get(track)?.number
    }

    /// The SMPTE time track `track` starts at: its SMPTE Offset. A file of one
    /// timeline holds only the lead's: in a format 1 file the offset is stored
    /// with the tempo map and "has no meaning in any of the other tracks"
    /// (SMF 1.0 section 3.3). `None` for no such track or offset.
    #[must_use]
    pub fn smpte_offset(&self, track: usize) -> Option<SmpteOffset> {
        self.openings.get(track)?.smpte_start
    }

    /// Where the tracks of a file of one timeline end: the longest track's end
    /// (an SMF's End of Track, a Clip File's End of Clip), at least where the
    /// last event lands.
    fn shared_end(&self) -> Tick {
        self.tracks.iter().map(|t| t.end).max().unwrap_or_default()
    }

    /// The events of every track merged by tick (a file of one timeline).
    fn merged(&self) -> Vec<(Tick, &FileEvent)> {
        let mut all: Vec<(Tick, &FileEvent)> = self
            .tracks
            .iter()
            .flat_map(|t| t.events.iter().map(|(tick, event)| (*tick, event)))
            .collect();
        // Stable: equal ticks keep track order, then the track's own.
        all.sort_by_key(|&(tick, _)| tick);
        all
    }
}

/// A file's time ([`MidiFile::timebase`]): what its ticks count, and whether
/// its tracks share one timeline. A file is one of the four as a whole, so the
/// choice is made once, here.
#[derive(Clone, Copy, Debug)]
pub enum Timebase<'a> {
    /// Ticks are fractions of a quarter note and the tracks share one
    /// timeline: an SMF of format 0 or 1 with a metrical [`SmfDivision`], and
    /// every Clip File. It has a beat grid, a tempo map and a clip.
    Metrical(MetricalView<'a>),
    /// Ticks are fractions of a second and the tracks share one timeline: an
    /// SMF of format 0 or 1 with an [`SmfDivision::Smpte`] division. It has no
    /// beat grid, so its beats come from a tempo map the caller gives
    /// ([`SmpteView::timelines`]).
    Smpte(SmpteView<'a>),
    /// Ticks are fractions of a quarter note and each track is an independent
    /// sequence with its own tempo map: an SMF of format 2, or above 2, with a
    /// metrical division. It has no one timeline, so no merged events and no
    /// clip.
    MetricalSequences(MetricalSequences<'a>),
    /// Ticks are fractions of a second and each track is an independent
    /// sequence: an SMF of format 2, or above 2, with an
    /// [`SmfDivision::Smpte`] division.
    SmpteSequences(SmpteSequences<'a>),
}

impl<'a> Timebase<'a> {
    /// The metrical view of one timeline, `None` for any other kind of file.
    #[must_use]
    pub fn metrical(self) -> Option<MetricalView<'a>> {
        match self {
            Self::Metrical(file) => Some(file),
            _ => None,
        }
    }
}

/// A [`MidiFile`] whose ticks are quarter notes and whose tracks share one
/// timeline ([`Timebase::Metrical`]): the beat grid, the tempo map its Set
/// Tempo events declare, and the clip it converts to.
#[derive(Clone, Copy, Debug)]
pub struct MetricalView<'a> {
    file: &'a MidiFile,
    ticks_per_quarter: TicksPerQuarterNote,
}

impl<'a> MetricalView<'a> {
    /// The division as ticks per quarter note.
    #[must_use]
    pub fn ticks_per_quarter(self) -> TicksPerQuarterNote {
        self.ticks_per_quarter
    }

    /// `tick` in quarter notes: the tick over the division.
    #[must_use]
    pub fn beat_at(self, tick: Tick) -> Beat {
        tick.beats(self.ticks_per_quarter)
    }

    /// The tick nearest `beat`: the inverse of [`beat_at`](Self::beat_at),
    /// over the division alone (tempo does not enter). Rounds as
    /// [`Tick::from_beat`] does: halfway goes to the later tick, and `None` is
    /// a beat before 0 or past the last tick.
    #[must_use]
    pub fn tick_at(self, beat: Beat) -> Option<Tick> {
        Tick::from_beat(beat, self.ticks_per_quarter)
    }

    /// Where the file ends: its longest track's end (an SMF's End of Track, a
    /// Clip File's End of Clip), at least where the last event lands.
    #[must_use]
    pub fn end(self) -> Tick {
        self.file.shared_end()
    }

    /// The tempo map of the file's timeline, built from its Set Tempo events
    /// (an SMF's meta events across all tracks, a Clip File's Flex Data Set
    /// Tempo messages), steps at the tick each lands on. A file that sets none
    /// at the start plays at 120 BPM until the first, the MIDI default. Two
    /// at one tick: the later governs, as a sequencer applying them in order
    /// would. The map is derived from the events and never set apart from them:
    /// a Set Tempo stays an event of its track, so a write gives back the exact
    /// event, and a zero (no valid tempo) is kept but makes no step. A Clip
    /// File's header Set Tempo is the tempo from the sequence's first tick,
    /// and a Set Tempo at tick 0 of the sequence overrides it.
    ///
    /// It is shared: `file.tempo_map().clone()` is a reference count.
    #[must_use]
    pub fn tempo_map(self) -> &'a Arc<TempoMap> {
        // A file of one timeline always has exactly one tempo map (the
        // constructor builds one for it).
        &self.file.tempo_maps[0]
    }

    /// The meter of the file's timeline: a change at the beat each Time
    /// Signature event lands on (a change that falls mid-bar makes the bar
    /// before it short), 4/4 from beat 0 where the file declares none there.
    /// A Time Signature no [`TimeSignature`] holds is not a change:
    /// [`refused_time_signatures`](Self::refused_time_signatures) lists it.
    #[must_use]
    pub fn meter_map(self) -> MeterMap {
        meter_of(self.events()).0
    }

    /// The Time Signature events of the timeline that
    /// [`meter_map`](Self::meter_map) leaves out, in order.
    #[must_use]
    pub fn refused_time_signatures(self) -> Vec<RefusedTimeSignature> {
        meter_of(self.events()).1
    }

    /// Seconds from the start of the file to `tick`, over its
    /// [`tempo_map`](Self::tempo_map): each stretch between two changes at
    /// its own tempo (a file's changes are all steps).
    ///
    /// What an offline render of the file lasts is `seconds_at(view.end())`.
    #[must_use]
    pub fn seconds_at(self, tick: Tick) -> Seconds {
        self.tempo_map().seconds_at(self.beat_at(tick))
    }

    /// Every event of the file's one timeline (a Clip File's
    /// [`header`](Track::header) is not on it) as `(beat, event)`, in order:
    /// the tracks merged by tick, ties in track order and then in the track's
    /// own order.
    pub fn events(self) -> impl Iterator<Item = (Beat, &'a FileEvent)> {
        let tpq = self.ticks_per_quarter;
        self.file
            .merged()
            .into_iter()
            .map(move |(tick, event)| (tick.beats(tpq), event))
    }

    /// The file's one timeline as the packets a clip plays, `(Beat, Ump)` in
    /// order ([`events`](Self::events)' order). Every beat is finite.
    ///
    /// The rule, one event at a time:
    ///
    /// - A [`Message`](FileEvent::Message) passes as its packet, as it is: an
    ///   SMF's channel message is MIDI 1.0 on the first Group, and a Clip File's
    ///   packets (Flex Data included) are as stored.
    /// - A [`SysEx`](FileEvent::SysEx) ending in `F7` goes out as its SysEx7
    ///   packets (M2-104 section 7.7), all at the event's beat.
    /// - A divided message (SMF 1.0 section 3.2: an `F0` event not ending in
    ///   `F7` starts it, the [`Escape`](FileEvent::Escape) events after it
    ///   continue it, and the one ending in `F7` ends it) goes out as a SysEx7
    ///   Start, Continues and an End, each packet at the beat of the event its
    ///   bytes are in, six bytes to a packet at most: the pauses between its
    ///   parts stay. No transmittable event goes between its parts (SMF 1.0
    ///   section 3.2), so one that does ends it unfinished, as does the next
    ///   `F0` event or the end of the file.
    /// - [`Meta`](FileEvent::Meta) events are not messages: the tempo is the
    ///   file's [`tempo_map`](Self::tempo_map), and the rest (text, time and
    ///   key signature, ...) are not on the timeline.
    ///
    /// What has no packet is in [`SmfTimeline::dropped`] with its
    /// [`TimelineDrop`], not lost silently: a bare `F7` escape (the arbitrary
    /// bytes SMF 1.0 section 3.2 allows, which no packet holds), a divided
    /// message left unfinished, one with a byte of 128 or more, and an
    /// [`Unknown`](FileEvent::Unknown) packet.
    ///
    /// The Clip File's [`header`](Track::header) is not on the timeline.
    ///
    /// ```
    /// use tutti_midi_file::{LeadTrack, Layout, MidiFile, SmfDivision, Tick, Track};
    /// use tutti_units::Beat;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let track = Track::new().note(Tick(480), Tick(960), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
    /// let file = MidiFile::smf(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(track)))?;
    /// let clip = file.timebase().metrical().unwrap().timeline();
    /// assert_eq!(clip.packets.len(), 2);
    /// assert_eq!(clip.packets[0].0, Beat::new(1.0));
    /// assert_eq!(clip.packets[1].0, Beat::new(2.0));
    /// assert!(clip.dropped.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn timeline(self) -> SmfTimeline {
        SmfTimeline::of(self.events())
    }
}

/// A Time Signature event that no [`TimeSignature`] holds
/// ([`MetricalView::refused_time_signatures`]): a numerator of 0, or a
/// denominator power of two past 32 bits.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct RefusedTimeSignature {
    /// Where the event lands.
    pub beat: Beat,
    /// The numerator, as the wire states it.
    pub numerator: u8,
    /// The denominator as a power of two, as the wire states it.
    pub denominator_power: u8,
}

/// The meter the Time Signature events declare, and the events no
/// [`TimeSignature`] holds.
fn meter_of<'a>(
    events: impl Iterator<Item = (Beat, &'a FileEvent)>,
) -> (MeterMap, Vec<RefusedTimeSignature>) {
    let (mut changes, mut refused) = (Vec::new(), Vec::new());
    for (beat, event) in events {
        let FileEvent::Meta(Meta::TimeSignature {
            numerator,
            denominator_power,
            ..
        }) = event
        else {
            continue;
        };
        let signature = 1u32
            .checked_shl(u32::from(*denominator_power))
            .and_then(|denominator| {
                TimeSignature::from_numbers(u32::from(*numerator), denominator)
            });
        match signature {
            Some(signature) => changes.push(MeterChange::new(beat, signature)),
            None => refused.push(RefusedTimeSignature {
                beat,
                numerator: *numerator,
                denominator_power: *denominator_power,
            }),
        }
    }
    (MeterMap::new(changes), refused)
}

/// A [`MidiFile`] of independent sequences whose ticks are quarter notes
/// ([`Timebase::MetricalSequences`]): each track is a sequence with its own
/// tempo map, and there is no one timeline to merge, convert to a clip or
/// read a time on.
#[derive(Clone, Copy, Debug)]
pub struct MetricalSequences<'a> {
    file: &'a MidiFile,
    ticks_per_quarter: TicksPerQuarterNote,
}

impl<'a> MetricalSequences<'a> {
    /// The division as ticks per quarter note.
    #[must_use]
    pub fn ticks_per_quarter(self) -> TicksPerQuarterNote {
        self.ticks_per_quarter
    }

    /// The sequences, one per track in track order.
    pub fn sequences(self) -> impl ExactSizeIterator<Item = MetricalSequence<'a>> {
        let Self {
            file,
            ticks_per_quarter,
        } = self;
        file.tracks.iter().zip(&file.tempo_maps).enumerate().map(
            move |(index, (track, tempo_map))| MetricalSequence {
                track,
                tempo_map,
                ticks_per_quarter,
                number: file.openings[index].number_at(index),
            },
        )
    }
}

/// One track of a [`MetricalSequences`] file, on its own beat grid and tempo
/// map.
#[derive(Clone, Copy, Debug)]
pub struct MetricalSequence<'a> {
    track: &'a Track,
    tempo_map: &'a Arc<TempoMap>,
    ticks_per_quarter: TicksPerQuarterNote,
    number: u16,
}

impl<'a> MetricalSequence<'a> {
    /// The sequence's number: its Sequence Number event's, or its place in
    /// the file when it has none. The specification says the sequences'
    /// locations in order are the default and names no base; tutti counts
    /// from 0. An explicit number equal to another sequence's number or place
    /// is not refused: the specification does not call numbers unique, and
    /// each sequence reports its own.
    #[must_use]
    pub fn number(self) -> u16 {
        self.number
    }

    /// The sequence's track.
    #[must_use]
    pub fn track(self) -> &'a Track {
        self.track
    }

    /// Where the sequence ends: its track's end.
    #[must_use]
    pub fn end(self) -> Tick {
        self.track.end
    }

    /// The tempo map of this sequence, built from the Set Tempo events of its
    /// track alone (see [`MetricalView::tempo_map`] for the rule).
    ///
    /// It is shared: `sequence.tempo_map().clone()` is a reference count.
    #[must_use]
    pub fn tempo_map(self) -> &'a Arc<TempoMap> {
        self.tempo_map
    }

    /// The meter of the sequence's track: a change at the beat each Time
    /// Signature event lands on (a change that falls mid-bar makes the bar
    /// before it short), 4/4 from beat 0 where the file declares none there.
    /// A Time Signature no [`TimeSignature`] holds is not a change:
    /// [`refused_time_signatures`](Self::refused_time_signatures) lists it.
    #[must_use]
    pub fn meter_map(self) -> MeterMap {
        meter_of(self.events()).0
    }

    /// The Time Signature events of the track that
    /// [`meter_map`](Self::meter_map) leaves out, in order.
    #[must_use]
    pub fn refused_time_signatures(self) -> Vec<RefusedTimeSignature> {
        meter_of(self.events()).1
    }

    /// `tick` in quarter notes: the tick over the division.
    #[must_use]
    pub fn beat_at(self, tick: Tick) -> Beat {
        tick.beats(self.ticks_per_quarter)
    }

    /// Seconds from the start of the sequence to `tick`, over its
    /// [`tempo_map`](Self::tempo_map).
    #[must_use]
    pub fn seconds_at(self, tick: Tick) -> Seconds {
        self.tempo_map.seconds_at(self.beat_at(tick))
    }

    /// The sequence's events as `(beat, event)`, in the track's order.
    pub fn events(self) -> impl Iterator<Item = (Beat, &'a FileEvent)> {
        let tpq = self.ticks_per_quarter;
        self.track
            .events
            .iter()
            .map(move |(tick, event)| (tick.beats(tpq), event))
    }
}

/// A [`MidiFile`] whose ticks are fractions of a second and whose tracks
/// share one timeline ([`Timebase::Smpte`]): a frame rate and the ticks in a
/// frame.
#[derive(Clone, Copy, Debug)]
pub struct SmpteView<'a> {
    file: &'a MidiFile,
    fps: MtcRate,
    ticks_per_frame: SmfTicksPerFrame,
}

impl<'a> SmpteView<'a> {
    /// Frames per second.
    #[must_use]
    pub fn fps(self) -> MtcRate {
        self.fps
    }

    /// The resolution within a frame.
    #[must_use]
    pub fn ticks_per_frame(self) -> SmfTicksPerFrame {
        self.ticks_per_frame
    }

    /// Where the file ends: its longest track's end, at least where the last
    /// event lands.
    #[must_use]
    pub fn end(self) -> Tick {
        self.file.shared_end()
    }

    /// Seconds from the start of the file to `tick`: the tick over the ticks
    /// in a second, `fps * ticks per frame`, with 29.97 as 30000/1001.
    #[must_use]
    pub fn seconds_at(self, tick: Tick) -> Seconds {
        smpte_seconds(self.fps, self.ticks_per_frame, tick)
    }

    /// Each track as the packets a clip plays, in track order, placed in beats
    /// over `tempo`: an event at `s` seconds lands on
    /// `tempo.beat_at_seconds(s)`, so a Set Tempo of the file moves nothing and
    /// the map alone sets the beats. The rule is [`Track::timeline`]'s.
    pub fn timelines<'t>(
        self,
        tempo: &'t TempoMap,
    ) -> impl ExactSizeIterator<Item = SmfTimeline> + use<'a, 't> {
        self.file.tracks.iter().map(move |track| {
            track.timeline_by(|tick| tempo.beat_at_seconds(self.seconds_at(tick)))
        })
    }

    /// Every event of the file's one timeline as `(seconds, event)`, in
    /// order: the tracks merged by tick, ties in track order and then in the
    /// track's own order.
    pub fn events(self) -> impl Iterator<Item = (Seconds, &'a FileEvent)> {
        self.file
            .merged()
            .into_iter()
            .map(move |(tick, event)| (self.seconds_at(tick), event))
    }
}

/// A [`MidiFile`] of independent sequences whose ticks are fractions of a
/// second ([`Timebase::SmpteSequences`]): a frame rate, the ticks in a frame
/// and no one timeline to merge.
#[derive(Clone, Copy, Debug)]
pub struct SmpteSequences<'a> {
    file: &'a MidiFile,
    fps: MtcRate,
    ticks_per_frame: SmfTicksPerFrame,
}

impl<'a> SmpteSequences<'a> {
    /// Frames per second.
    #[must_use]
    pub fn fps(self) -> MtcRate {
        self.fps
    }

    /// The resolution within a frame.
    #[must_use]
    pub fn ticks_per_frame(self) -> SmfTicksPerFrame {
        self.ticks_per_frame
    }

    /// The sequences, one per track in track order.
    pub fn sequences(self) -> impl ExactSizeIterator<Item = SmpteSequence<'a>> {
        let Self {
            file,
            fps,
            ticks_per_frame,
        } = self;
        file.tracks
            .iter()
            .enumerate()
            .map(move |(index, track)| SmpteSequence {
                track,
                fps,
                ticks_per_frame,
                number: file.openings[index].number_at(index),
            })
    }
}

/// One track of a [`SmpteSequences`] file.
#[derive(Clone, Copy, Debug)]
pub struct SmpteSequence<'a> {
    track: &'a Track,
    fps: MtcRate,
    ticks_per_frame: SmfTicksPerFrame,
    number: u16,
}

impl<'a> SmpteSequence<'a> {
    /// The sequence's number, as [`MetricalSequence::number`].
    #[must_use]
    pub fn number(self) -> u16 {
        self.number
    }

    /// The sequence's track.
    #[must_use]
    pub fn track(self) -> &'a Track {
        self.track
    }

    /// Where the sequence ends: its track's end.
    #[must_use]
    pub fn end(self) -> Tick {
        self.track.end
    }

    /// Seconds from the start of the sequence to `tick` (as
    /// [`SmpteView::seconds_at`]).
    #[must_use]
    pub fn seconds_at(self, tick: Tick) -> Seconds {
        smpte_seconds(self.fps, self.ticks_per_frame, tick)
    }

    /// The sequence as the packets a clip plays, placed in beats over
    /// `tempo` ([`SmpteView::timelines`]).
    pub fn timeline(self, tempo: &TempoMap) -> SmfTimeline {
        self.track
            .timeline_by(|tick| tempo.beat_at_seconds(self.seconds_at(tick)))
    }

    /// The sequence's events as `(seconds, event)`, in the track's order.
    pub fn events(self) -> impl Iterator<Item = (Seconds, &'a FileEvent)> {
        self.track
            .events
            .iter()
            .map(move |(tick, event)| (self.seconds_at(*tick), event))
    }
}

/// Seconds of `tick` in SMPTE time: the tick over the ticks in a second,
/// `fps * ticks per frame`, with 29.97 as 30000/1001. A Set Tempo moves no tick.
fn smpte_seconds(fps: MtcRate, ticks_per_frame: SmfTicksPerFrame, tick: Tick) -> Seconds {
    // The header's -29 is 29.97: the specification says "-29 corresponds to 30
    // drop frame", the 30 drop-frame rate being 30000/1001.
    let (frames, per) = fps.frame_rate().ratio();
    let ticks = frames * u64::from(ticks_per_frame.get());
    Seconds::new(tick.0 as f64 * per as f64 / ticks as f64)
}

/// Events as the packets a clip plays ([`MetricalView::timeline`]), and the events
/// that gave none.
#[derive(Clone, Debug, PartialEq, Default)]
#[must_use = "`dropped` lists the events with no packet"]
#[non_exhaustive]
pub struct SmfTimeline {
    /// The packets at their beats, in order.
    pub packets: Vec<(Beat, Ump)>,
    /// The [`SysEx`](FileEvent::SysEx), [`Escape`](FileEvent::Escape) and
    /// [`Unknown`](FileEvent::Unknown) events that have no packet, at their
    /// beats and with why, in order. A divided message that gave none is all
    /// of its parts.
    pub dropped: Vec<(Beat, FileEvent, TimelineDrop)>,
}

/// Why an event of [`MetricalView::timeline`] has no packet ([`SmfTimeline::dropped`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimelineDrop {
    /// An `F7` event with no divided message open: the escape of arbitrary
    /// bytes SMF 1.0 section 3.2 allows, which no SysEx7 packet holds.
    BareEscape,
    /// A part of a divided message that was never ended: a transmittable
    /// event, the next `F0` event or the end of the file came before its `F7`.
    Unfinished,
    /// A part of a message with a byte of 128 or more, which SysEx7 does not
    /// carry.
    NotSevenBit,
    /// A Clip File packet no definition covers ([`FileEvent::Unknown`]).
    UnknownPacket,
}

/// The bytes of a System Exclusive message run, `F7` terminated.
fn ends_in_f7(data: &[u8]) -> bool {
    data.last() == Some(&0xF7)
}

impl SmfTimeline {
    /// The packets of `events`, by [`MetricalView::timeline`]'s rule.
    fn of<'a>(events: impl Iterator<Item = (Beat, &'a FileEvent)>) -> Self {
        let mut timeline = Self::default();
        // The parts of the divided message being read: its `F0` event, then
        // the `F7` events after it.
        let mut open: Vec<(Beat, &FileEvent)> = Vec::new();
        for (beat, event) in events {
            match event {
                FileEvent::SysEx(sysex) => {
                    timeline.abandon(&mut open);
                    match sysex {
                        SysEx::Complete(_) => timeline.complete(&[(beat, event)]),
                        SysEx::Start(_) => open.push((beat, event)),
                    }
                }
                FileEvent::Escape(_) if open.is_empty() => {
                    timeline.discard(beat, event, TimelineDrop::BareEscape);
                }
                FileEvent::Escape(data) => {
                    open.push((beat, event));
                    if ends_in_f7(data) {
                        timeline.complete(&open);
                        open.clear();
                    }
                }
                FileEvent::Message(message) => {
                    timeline.abandon(&mut open);
                    timeline.packets.push((beat, Ump::from(*message)));
                }
                FileEvent::Unknown(_) => {
                    timeline.abandon(&mut open);
                    timeline.discard(beat, event, TimelineDrop::UnknownPacket);
                }
                FileEvent::Meta(_) => {}
            }
        }
        timeline.abandon(&mut open);
        timeline
    }

    fn discard(&mut self, beat: Beat, event: &FileEvent, why: TimelineDrop) {
        self.dropped.push((beat, event.clone(), why));
    }

    /// Reports the divided message left unfinished, if one is open.
    fn abandon(&mut self, open: &mut Vec<(Beat, &FileEvent)>) {
        for (beat, event) in open.drain(..) {
            self.dropped
                .push((beat, event.clone(), TimelineDrop::Unfinished));
        }
    }

    /// The packets of one message, whole in one event or divided into the
    /// `parts` (the last ending in `F7`): each part's bytes in packets of up to
    /// six at the part's beat, the first of the message a Start, the last an
    /// End, any between Continues, and a message of one packet a Complete.
    fn complete(&mut self, parts: &[(Beat, &FileEvent)]) {
        // The data of each part, `F7` of the last cut, as 7-bit bytes: a
        // message with any other byte has no packets.
        let payloads: Option<Vec<(Beat, Vec<U7>)>> = parts
            .iter()
            .enumerate()
            .map(|(i, &(beat, event))| {
                let bytes: Option<Vec<U7>> = match event {
                    FileEvent::SysEx(sysex) => Some(sysex.data().to_vec()),
                    // The last packet of a divided message ends in `F7`, which
                    // no SysEx7 packet holds.
                    FileEvent::Escape(data) => {
                        let data = if i + 1 == parts.len() {
                            &data[..data.len() - 1]
                        } else {
                            &data[..]
                        };
                        data.iter().map(|&b| U7::new(b)).collect()
                    }
                    _ => Some(Vec::new()),
                };
                bytes.map(|bytes| (beat, bytes))
            })
            .collect();
        let Some(payloads) = payloads else {
            for &(beat, event) in parts {
                self.discard(beat, event, TimelineDrop::NotSevenBit);
            }
            return;
        };
        // An empty part still sends a packet: its pause is the message's.
        let last = payloads.len() - 1;
        for (i, (beat, bytes)) in payloads.iter().enumerate() {
            let part = match i {
                0 if last == 0 => SysexPart::Only,
                0 => SysexPart::First,
                i if i == last => SysexPart::Last,
                _ => SysexPart::Middle,
            };
            for packet in Sysex7Packets::part(MidiGroup::FIRST, bytes, part) {
                self.packets.push((*beat, packet));
            }
        }
    }
}

/// The packets of `events` at their ticks: what the clip layer reads. An
/// SMF event has none; a Clip File's constructor has refused them.
fn packets(events: &[(Tick, FileEvent)]) -> Vec<(Tick, Ump)> {
    events
        .iter()
        .filter_map(|(tick, event)| event.ump().map(|ump| (*tick, ump)))
        .collect()
}

/// Why `container` cannot hold `event`, `None` if it can.
fn refuses(container: Container, event: &FileEvent) -> Option<Refusal> {
    match (container, event) {
        (Container::Smf(..), FileEvent::Message(Message::Midi1(address, _))) => {
            (address.group != MidiGroup::FIRST).then_some(Refusal::SmfGroup)
        }
        (Container::Smf(..), FileEvent::Message(_) | FileEvent::Unknown(_)) => {
            Some(Refusal::SmfPacket)
        }
        (Container::Smf(..), FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_)) => {
            None
        }
        (Container::Clip(_), FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_)) => {
            Some(Refusal::ClipSmfEvent)
        }
        (Container::Clip(_), FileEvent::Message(Message::Utility(_))) => Some(Refusal::ClipUtility),
        (
            Container::Clip(_),
            FileEvent::Message(Message::Stream(
                tutti_ump::Stream::StartOfClip | tutti_ump::Stream::EndOfClip,
            )),
        ) => Some(Refusal::ClipStructure),
        (Container::Clip(_), FileEvent::Message(message))
            if clip_file::property_exchange(message) =>
        {
            Some(Refusal::ClipPropertyExchange)
        }
        (Container::Clip(_), FileEvent::Message(_) | FileEvent::Unknown(_)) => None,
    }
}

/// The rules of one run of events (`part` of track `track`) and where it
/// `end`s: no gap past the container's delta, only events the container
/// holds, and not ending too long after its last event. The events are in
/// order and the end is not before the last of them, as a [`Track`] holds
/// them.
fn check_run(
    container: Container,
    track: usize,
    part: TrackPart,
    events: &[(Tick, FileEvent)],
    end: Tick,
) -> Result<(), TracksError> {
    // The widest gap the wire holds: 28 bits in an SMF. A Clip File's Delta
    // Clockstamp restarts its count with a Null message, so any gap is
    // expressible (M2-116 section 3.2.2).
    let max_gap = match container {
        Container::Smf(..) => 0x0FFF_FFFF,
        Container::Clip(_) => u64::MAX,
    };
    let mut at = Tick(0);
    for (tick, event) in events {
        if tick.0 - at.0 > max_gap {
            return Err(TracksError::Gap { track, at: *tick });
        }
        at = *tick;
        if let Some(refusal) = refuses(container, event) {
            return Err(TracksError::Refused {
                track,
                part,
                refusal,
            });
        }
    }
    if end.0 - at.0 > max_gap {
        return Err(TracksError::EndTooFar { track, end });
    }
    Ok(())
}

/// The rules a writer holds an SMF's tracks to that no type makes unwritable
/// (SMF 1.0 sections 2.2 and 3), that a reader keeps a file without: a divided
/// System Exclusive message ends in an `F7` packet with no channel message or
/// other `F0` event between its packets; the Set Tempo and Time Signature of a
/// format 1 file are in its first track.
fn check_smf(format: SmfFormat, tracks: &[Track]) -> Result<(), TracksError> {
    for (track, body) in tracks.iter().enumerate() {
        // The tick of the first packet of a divided message not yet ended by an `F7`.
        let mut open: Option<Tick> = None;
        for &(at, ref event) in &body.events {
            match event {
                FileEvent::SysEx(_) | FileEvent::Message(_) if open.is_some() => {
                    return Err(TracksError::SysExInterrupted { track, at });
                }
                FileEvent::SysEx(SysEx::Start(_)) => open = Some(at),
                FileEvent::Escape(data) if open.is_some() && ends_in_f7(data) => open = None,
                FileEvent::Meta(Meta::Tempo { .. } | Meta::TimeSignature { .. })
                    if format == SmfFormat::Parallel && track > 0 =>
                {
                    return Err(TracksError::TempoMapNotInFirstTrack { track, at });
                }
                _ => {}
            }
        }
        if let Some(at) = open {
            return Err(TracksError::SysExNotEnded { track, at });
        }
    }
    Ok(())
}

/// Gives each timeline the Set Tempo and Time Signature SMF 1.0 section 2.2 asks
/// every file to specify, at tick 0 of the timeline's first track after the
/// meta events that open it, unless a track of the timeline has one there
/// already. A timeline is all the tracks of a format 0 or 1 file, which share
/// one, and each track of any other format (a format above 2 is read as format
/// 2, each track its own pattern, as section 2.1 lets a reader do). What it
/// adds is what a reader assumes without them (120 beats a minute in 4/4, the
/// Time Signature example's 24 clocks a click and 8 32nd notes to the
/// quarter), so the file plays as it did. In an SMPTE division they are as
/// inert as any other tempo. Returns what it added.
fn specify_tempo_and_meter(format: SmfFormat, tracks: &mut [Track]) -> Vec<ReadRepair> {
    let timelines: Vec<std::ops::Range<usize>> = if format.shares_timeline() {
        std::iter::once(0..tracks.len()).collect()
    } else {
        (0..tracks.len()).map(|i| i..i + 1).collect()
    };
    let mut added = Vec::new();
    for timeline in timelines {
        let first = timeline.start;
        let has = |wanted: fn(&Meta) -> bool| {
            tracks[timeline.clone()].iter().any(|track| {
                track.events.iter().any(|(tick, event)| {
                    *tick == Tick(0) && matches!(event, FileEvent::Meta(meta) if wanted(meta))
                })
            })
        };
        let tempo = !has(|m| matches!(m, Meta::Tempo { .. }));
        let meter = !has(|m| matches!(m, Meta::TimeSignature { .. }));
        // After the meta events that open the track, so a Sequence Number
        // or a track-opening text stays first.
        let track = &mut tracks[first];
        let lead = track
            .events
            .iter()
            .take_while(|(tick, e)| *tick == Tick(0) && matches!(e, FileEvent::Meta(_)))
            .count();
        let missing = [
            tempo.then(default_tempo),
            meter.then(default_time_signature),
        ];
        for (offset, meta) in missing.into_iter().flatten().enumerate() {
            track.events.insert(lead + offset, (Tick(0), meta.into()));
        }
        if tempo {
            added.push(ReadRepair::DefaultTempo { track: first });
        }
        if meter {
            added.push(ReadRepair::DefaultTimeSignature { track: first });
        }
    }
    added
}

/// The tempo SMF 1.0 section 2.2 assumes when a file gives none.
const DEFAULT_TEMPO: Bpm = Bpm::of(120.0);

/// The Set Tempo for [`DEFAULT_TEMPO`]: 500 000 us per quarter.
fn default_tempo() -> Meta {
    Meta::Tempo {
        micros_per_quarter: MicrosPerQuarter::of(500_000),
        extra: Box::default(),
    }
}

/// The time signature SMF 1.0 section 2.2 assumes when a file gives none, 4/4,
/// with the clocks and 32nd notes of section 3.3's example (`FF 58 04 04 02 18 08`).
fn default_time_signature() -> Meta {
    Meta::TimeSignature {
        numerator: 4,
        denominator_power: 2,
        clocks_per_click: 24,
        thirty_seconds_per_quarter: 8,
        extra: Box::default(),
    }
}

/// Where a Flex Data message is interrupted (M2-104 section 7.5.2: within a
/// Group, only Continue packets of the message and System Real Time messages
/// go between its Start and End): the tick of the first message of a Group
/// with a message open that is not one of those, or a Continue or End with no
/// message of its own open, or of the last event if a message is left open.
fn text_interrupted(events: &[(Tick, FileEvent)]) -> Option<Tick> {
    let mut open: HashMap<MidiGroup, (FlexAddress, FlexTextKind)> = HashMap::new();
    for (tick, event) in events {
        let Some(ump) = event.ump() else {
            continue;
        };
        let Some(group) = ump.group() else {
            continue;
        };
        match ump.decode() {
            Ok(Message::Flex(
                _,
                Flex::Text {
                    address,
                    kind,
                    form,
                    ..
                },
            )) => {
                let key = (address, kind);
                match (form, open.get(&group).copied()) {
                    (Form::Complete, None) => {}
                    (Form::Start, None) => {
                        open.insert(group, key);
                    }
                    (Form::Continue, Some(of)) if of == key => {}
                    (Form::End, Some(of)) if of == key => {
                        open.remove(&group);
                    }
                    _ => return Some(*tick),
                }
            }
            Ok(Message::System(
                _,
                System::TimingClock
                | System::Start
                | System::Continue
                | System::Stop
                | System::ActiveSensing
                | System::Reset,
            )) => {}
            _ if open.contains_key(&group) => return Some(*tick),
            _ => {}
        }
    }
    if open.is_empty() {
        None
    } else {
        events.last().map(|(tick, _)| *tick)
    }
}

/// The tempo of a Flex Set Tempo message, `None` for any other message or a
/// field of `0` (M2-104 section 7.5.3).
fn set_tempo(message: &Message) -> Option<Bpm> {
    match message {
        Message::Flex(_, Flex::SetTempo(field)) => field.bpm(),
        _ => None,
    }
}

/// The tempo map of one timeline: `tracks`' Set Tempo events as steps.
fn tempo_map(
    container: Container,
    ticks_per_quarter: TicksPerQuarterNote,
    tracks: &[Track],
) -> Result<Arc<TempoMap>, tutti_tempo::TempoError> {
    // The default, which a timeline with no valid Set Tempo at its start plays
    // at: a Clip File with none, or a Set Tempo of zero.
    let default = DEFAULT_TEMPO;
    let mut changes: Vec<(Tick, Bpm)> = Vec::new();
    for track in tracks {
        // A Clip File's header Set Tempo counts from the start of the
        // sequence: the header runs before it, so its tempo is the one at tick
        // 0 (a Set Tempo at tick 0 of the sequence, later in the stream,
        // overrides it).
        for (_, event) in &track.header {
            if let (Container::Clip(_), FileEvent::Message(message)) = (container, event)
                && let Some(bpm) = set_tempo(message)
            {
                changes.push((Tick(0), bpm));
            }
        }
        for (tick, event) in &track.events {
            let bpm = match (container, event) {
                (Container::Smf(..), FileEvent::Meta(meta)) => meta.bpm(),
                (Container::Clip(_), FileEvent::Message(message)) => set_tempo(message),
                _ => None,
            };
            if let Some(bpm) = bpm {
                changes.push((*tick, bpm));
            }
        }
    }
    // Stable: two tracks setting a tempo on one tick keep track order, and the
    // later one governs.
    changes.sort_by_key(|&(tick, _)| tick);
    let mut points: Vec<(Tick, Bpm)> = Vec::with_capacity(changes.len() + 1);
    if changes.first().is_none_or(|&(tick, _)| tick != Tick(0)) {
        points.push((Tick(0), default));
    }
    for change in changes {
        match points.last_mut() {
            Some(last) if last.0 == change.0 => *last = change,
            _ => points.push(change),
        }
    }
    Ok(Arc::new(TempoMap::new(points.into_iter().map(
        |(tick, tempo)| TempoPoint {
            beat: tick.beats(ticks_per_quarter),
            tempo,
            transition: Transition::Step,
        },
    ))?))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TEMPO, Meta, SmfFormat, TextKind, default_tempo};

    /// The default Set Tempo is [`DEFAULT_TEMPO`]'s: SMF 1.0 section 2.2's
    /// 120 BPM is 500 000 us per quarter.
    #[test]
    fn the_default_set_tempo_is_the_default_tempo() {
        assert_eq!(Meta::tempo(DEFAULT_TEMPO), Some(default_tempo()));
    }

    /// SMF 1.0 section 3.3: the text meta types are 01 to 09. The copyright
    /// notice (02) and the Track Name (03) are a track's fields, so no kind
    /// has them; every kind is in the lookup's list under its own type.
    #[test]
    fn every_text_kind_round_trips_its_meta_type() {
        assert_eq!(
            TextKind::ALL.map(TextKind::meta_type),
            [0x01, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]
        );
        for kind in TextKind::ALL {
            assert_eq!(TextKind::from_meta_type(kind.meta_type()), Some(kind));
        }
        for lead_or_unknown in [0x00, 0x02, 0x03, 0x0A] {
            assert_eq!(TextKind::from_meta_type(lead_or_unknown), None);
        }
    }

    /// SMF 1.0 section 2.1: the format words 0, 1 and 2 are the named formats
    /// and every other word is kept as it is, so a word reads and writes back
    /// to itself.
    #[test]
    fn every_format_word_round_trips() {
        assert_eq!(SmfFormat::from_word(0), SmfFormat::SingleTrack);
        assert_eq!(SmfFormat::from_word(1), SmfFormat::Parallel);
        assert_eq!(SmfFormat::from_word(2), SmfFormat::Sequential);
        for word in [0, 1, 2, 3, 4, 0x1234, u16::MAX] {
            assert_eq!(SmfFormat::from_word(word).word(), word);
        }
        assert!(matches!(SmfFormat::from_word(3), SmfFormat::Other(_)));
    }
}
