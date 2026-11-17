//! [`MidiFile`]: one file model for both containers, lossless, with a tempo
//! map.
//!
//! `.mid` is worn by a Standard MIDI File and a MIDI 2.0 Clip File alike, so
//! which decoder runs is decided by the leading bytes ([`MidiFileKind::detect`]),
//! never by the name. Both read into the same shape: [`Track`]s of
//! `(Tick, FileEvent)`, the file's division, and the [`TempoMap`] its tempo
//! events declare. Ticks are integers, so a file read and written holds the
//! same events with no rounding; [`MidiFile::events`] is where they become
//! [`Beat`]s.
//!
//! The file layer does not translate. An SMF channel message is a MIDI 1.0
//! [`Message::Midi1`] on the first Group, and a MIDI 1.0 Note On at velocity 0 stays
//! one (it is a note-off by the MIDI 1.0 rule, which [`Track::notes`] applies);
//! a Clip File's packets are as stored. Translation runs where a clip is
//! loaded for playback. A file allocates and never reaches the audio thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tutti_midi_device::{NotSetProfileOn, ProfileOnRequest};
use tutti_midi_protocol::{channel_voice, flex_tempo};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{
    Address, Flex, FlexAddress, FlexTextKind, Form, KeyEvent, Message, MidiChannel, MidiGroup,
    NoteNumber, NoteVelocity, Sysex7Packet, SysexStatus, System, TicksPerQuarterNote, U7, Ump,
    Unknown, Voice2,
};

use crate::MidiFileKind;
use crate::smf::SmfDivision;
use crate::write::{Existing, WriteOptions};
use crate::{ConfigError, Error, IoStep, OutOfOrder, Refusal, Result, clip_file, smf};

mod meta_kind;
mod to_clip;

pub use meta_kind::{
    MalformedMeta, MicrosPerQuarter, MicrosPerQuarterOutOfRange, OtherMetaKind,
    OtherMetaKindOutOfRange,
};
pub use to_clip::{Altered, Converted, DropReason, Dropped, LyricChange, Moved, ToClipError};
use tutti_ump::Voice1;

/// A position in a file, in ticks of its division: whole numbers, so a file
/// written back lands every event on the tick it was read at.
///
/// A tick is a count, and the whole of `u64` is in range. Arithmetic is
/// checked: [`checked_add`](Tick::checked_add), [`ticks_since`](Tick::ticks_since)
/// and [`quarters`](Tick::quarters) answer `None` where `u64` would wrap.
/// [`MidiFile::beat_at`] is the conversion to a [`Beat`].
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
    /// (`ticks_per_quarter`, [`MidiFile::ticks_per_quarter`]).
    ///
    /// ```
    /// use tutti_midi_file::Tick;
    /// use tutti_types::Beat;
    /// use tutti_ump::TicksPerQuarterNote;
    ///
    /// assert_eq!(Tick(720).beats(TicksPerQuarterNote::of(480)), Beat(1.5));
    /// ```
    #[inline]
    #[must_use]
    pub fn beats(self, ticks_per_quarter: TicksPerQuarterNote) -> Beat {
        Beat(self.0 as f64 / f64::from(ticks_per_quarter.get()))
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
    /// use tutti_types::Beat;
    /// use tutti_ump::TicksPerQuarterNote;
    ///
    /// assert_eq!(Tick::from_beat(Beat(1.5), TicksPerQuarterNote::of(480)), Some(Tick(720)));
    /// // 0.5 tick at 128 ticks per quarter rounds up.
    /// assert_eq!(Tick::from_beat(Beat(1.0 / 256.0), TicksPerQuarterNote::of(128)), Some(Tick(1)));
    /// assert_eq!(Tick::from_beat(Beat(-1.0), TicksPerQuarterNote::of(480)), None);
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
}

/// What a text meta event says (SMF 1.0 section 3.3 types `01` to `09`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextKind {
    /// `01`: any text.
    Text,
    /// `02`: a copyright notice.
    Copyright,
    /// `03`: the track (or, in a format 0 file or the first track of a format
    /// 1 file, the sequence) name.
    TrackName,
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

/// A meta event of a Standard MIDI File (SMF 1.0 section 3.3), minus the End
/// of Track, which is a [`Track`]'s `end`.
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
        /// Sharps if positive, flats if negative.
        sharps: i8,
        /// Whether the key is minor.
        minor: bool,
        /// The data past the two bytes this reads, as `extra` of
        /// [`Tempo`](Self::Tempo).
        extra: Box<[u8]>,
    },
    /// Any other meta event (sequence number `00`, channel prefix `20`, port
    /// `21`, SMPTE offset `54`, sequencer specific `7F`, an unregistered
    /// type): its type byte and its data.
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

/// The slowest tempo the wire holds: 24 bits of microseconds per quarter.
const MAX_MICROS_PER_QUARTER: u32 = 0x00FF_FFFF;

/// Microseconds in a minute: the numerator of the tempo relation, a Set Tempo
/// being microseconds per quarter note.
const MICROS_PER_MINUTE: f64 = 60_000_000.0;

impl Meta {
    /// A Set Tempo for `bpm`, rounded to the wire's microsecond. The wire
    /// holds 1 to 16 777 215 us per quarter: a tempo above 60 million BPM
    /// writes the shortest, 1 (the wire's zero means no valid tempo), and
    /// one below about 3.6 BPM the longest.
    pub fn tempo(bpm: Bpm) -> Self {
        let micros = (MICROS_PER_MINUTE / bpm.get()).round() as u32;
        Self::Tempo {
            micros_per_quarter: MicrosPerQuarter::of(micros.clamp(1, MAX_MICROS_PER_QUARTER)),
            extra: Box::default(),
        }
    }

    /// The tempo of a Set Tempo, `None` for any other event and for a zero
    /// (no valid tempo).
    pub fn bpm(&self) -> Option<Bpm> {
        match self {
            Self::Tempo {
                micros_per_quarter, ..
            } if micros_per_quarter.get() != 0 => {
                Bpm::new(MICROS_PER_MINUTE / f64::from(micros_per_quarter.get()))
            }
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
    /// An SMF `F0` System Exclusive event: the bytes after the `F0` and its
    /// length, normally ending in `F7`. Any length, unlike a packet.
    SysEx(Box<[u8]>),
    /// An SMF `F7` event: the continuation of a divided System Exclusive
    /// message, or an escape sequence of arbitrary bytes. Distinct from
    /// [`SysEx`](Self::SysEx) because the file distinguishes them.
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
}

/// A track: events in order, each at an absolute [`Tick`], and where the track
/// ends.
///
/// An SMF's track is one `MTrk` chunk and a Clip File's one sequence. Its name
/// is its Track Name meta event ([`name`](Self::name)), which stays where the
/// file put it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Track {
    /// A Clip File's configuration header (M2-116 section 6): the messages
    /// between the file header and Start of Clip that configure a receiver
    /// (Set Profile On, Bank Select, Program Change, volume, System
    /// Exclusive, and at most one Set Tempo and one Set Time Signature), in
    /// order. They are *not* events of the sequence and do not sound at its
    /// start. Their ticks are the header's own clock, counted from the DCTPQ;
    /// the leading Set Profile On messages are at 0. Empty for an SMF.
    pub header: Vec<(Tick, FileEvent)>,
    /// Where the header ends, on the header's clock: the tick of Start of Clip,
    /// which is the last header message's tick plus the Delta Clockstamp before
    /// Start of Clip. M2-116 section 6.1: the Delta Clockstamps up to and
    /// including the one for Start of Clip add up to the time allocated to the
    /// Clip Configuration Header, which a receiver may need to configure itself.
    /// Not before the last header message. `Tick(0)` for an SMF, and for a
    /// header that asks for no time after its last message.
    pub header_end: Tick,
    /// The events, in file order. Their ticks do not decrease.
    pub events: Vec<(Tick, FileEvent)>,
    /// Where the track ends: an SMF's End of Track, a Clip File's End of
    /// Clip. Not before the last event.
    pub end: Tick,
}

impl Track {
    /// An empty track, ending at tick 0. Events are added with the builder
    /// ([`note`](Self::note), [`meta`](Self::meta),
    /// [`push`](Self::push)), by [`collect`](Iterator::collect)ing or
    /// [`extend`](Extend::extend)ing `(Tick, FileEvent)` pairs, or by filling
    /// the public fields of a track a file layer reads.
    ///
    /// ```
    /// use tutti_midi_file::{Meta, Tick, Track};
    /// use tutti_types::Bpm;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let track = Track::new()
    ///     .meta(Tick(0), Meta::tempo(Bpm::of(90.0)))
    ///     .note(Tick(0), Tick(480), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
    ///     .note(Tick(480), Tick(960), MidiChannel::FIRST, NoteNumber::of(64), U7::of(100));
    /// assert_eq!(track.end, Tick(960));
    /// assert_eq!(track.events.len(), 5);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    /// [`tempo_map`](MidiFile::tempo_map), as a sequencer applying them in order
    /// would.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_midi_file::{Meta, Tick, Track};
    /// use tutti_types::Bpm;
    ///
    /// let track = Track::new().meta(Tick(0), Meta::tempo(Bpm::of(90.0)));
    /// assert_eq!(track.events.len(), 1);
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

    /// A Set Profile On at the start of the Clip Configuration Header, after
    /// the ones already there (M2-116 section 6.2: data meant for a Profile is
    /// tagged with a Set Profile On, with that Profile Id, at the beginning of
    /// the header). The message is at tick 0 and carries no Delta Clockstamp
    /// when written (section 6); it is a [`ProfileOnRequest`], whose MUIDs are
    /// Broadcast. A Clip File's header only: an SMF's is refused when the file
    /// is built.
    ///
    /// ```
    /// use tutti_midi_device::{ProfileId, ProfileOnRequest, ProfileTarget};
    /// use tutti_midi_file::Track;
    ///
    /// let on = ProfileOnRequest { target: ProfileTarget::Group, profile: ProfileId::MPE };
    /// let track = Track::new().profile_on(on);
    /// assert_eq!(track.profiles(), Ok(vec![on]));
    /// ```
    #[must_use]
    pub fn profile_on(mut self, request: ProfileOnRequest) -> Self {
        let leading = clip_file::leading_profile_len(&header_packets(&self.header));
        let packets = clip_file::profile_packets(request).map(|ump| (Tick(0), to_clip::event(ump)));
        self.header.splice(leading..leading, packets);
        self
    }

    /// The Set Profile On messages at the head of the header, in order, which
    /// section 6.2 puts there. A file's header holds only ones that read, so
    /// this fails only for a track not built into a file.
    ///
    /// # Errors
    ///
    /// The [`NotSetProfileOn`] of the first message that does not read.
    pub fn profiles(&self) -> std::result::Result<Vec<ProfileOnRequest>, NotSetProfileOn> {
        let packets = header_packets(&self.header);
        let leading = clip_file::leading_profile_len(&packets);
        clip_file::profiles(packets[..leading].iter().map(|&(_, ump)| ump)).collect()
    }

    /// The track's events as the packets a clip plays, at their beats for a
    /// file of division `ticks_per_quarter`, in the track's order. The rule is
    /// [`MidiFile::timeline`]'s.
    pub fn timeline(&self, ticks_per_quarter: TicksPerQuarterNote) -> Timeline {
        Timeline::of(
            self.events
                .iter()
                .map(|(tick, event)| (tick.beats(ticks_per_quarter), event)),
        )
    }

    /// The track's name: its first Track Name event, if it is UTF-8 text.
    pub fn name(&self) -> Option<&str> {
        self.events.iter().find_map(|(_, event)| match event {
            FileEvent::Meta(Meta::Text {
                kind: TextKind::TrackName,
                bytes,
            }) => std::str::from_utf8(bytes).ok(),
            _ => None,
        })
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
            // `key_event` reads the status, group, channel and note bits of a
            // MIDI 1.0 or 2.0 channel-voice packet and counts a MIDI 1.0 Note
            // On at velocity 0 as an off, as `Ump::is_note_off` does.
            let Some(key_event @ (KeyEvent::On(key) | KeyEvent::Off(key))) = ump.key_event() else {
                continue;
            };
            let Address { group, channel } = key.address();
            let note = key.note();
            let map_key = (group, channel, note);
            if matches!(key_event, KeyEvent::Off(_)) {
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
                // A MIDI 1.0 Note On widens (`channel_voice`), a MIDI 2.0 one
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
    /// The Note On velocity at MIDI 2.0 width: a MIDI 1.0 velocity is widened
    /// by the specification's scaling, so this is lossless in both directions.
    pub velocity: NoteVelocity,
    /// Where the note starts.
    pub start: Tick,
    /// Where it ends; not before `start`.
    pub end: Tick,
}

/// The container a file was read from, with its division, and with it the
/// rules it is written by. An SMF's division has 15 bits, a Clip File's 16.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Container {
    Smf(SmfFormat, SmfDivision),
    Clip(TicksPerQuarterNote),
}

impl Container {
    /// The file's division as ticks per quarter note.
    fn ticks_per_quarter(self) -> TicksPerQuarterNote {
        match self {
            Self::Smf(_, division) => division.into(),
            Self::Clip(ticks_per_quarter) => ticks_per_quarter,
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
/// Format 2 SMFs keep their sequences apart: [`events`](Self::events) merges
/// the tracks of a format 0 or 1 file and of a Clip File, which share one
/// timeline, and has none for a format 2 file, whose tracks are independent
/// ([`tempo_map_of`](Self::tempo_map_of) is each one's own).
///
/// ```
/// use tutti_midi_file::{Meta, MidiFile, SmfDivision, SmfFormat, Tick, Track};
/// use tutti_types::{Beat, Bpm, Seconds};
/// use tutti_ump::{MidiChannel, NoteNumber, U7};
///
/// let track = Track::new()
///     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
///     .note(Tick(0), Tick(480), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// let file = MidiFile::smf(SmfFormat::SingleTrack, SmfDivision::of(480), vec![track])?;
///
/// let again = MidiFile::from_bytes(&file.to_bytes()?)?;
/// assert_eq!(again, file);
/// assert_eq!(again.seconds_at(Beat(1.0)), Seconds(0.5));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct MidiFile {
    container: Container,
    tracks: Vec<Track>,
    /// One map for a file of one timeline, one per track for a format 2 file.
    tempo_maps: Vec<Arc<TempoMap>>,
}

/// Two files are equal when their container, division and tracks are; the
/// tempo maps are derived from the tracks.
impl PartialEq for MidiFile {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container && self.tracks == other.tracks
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

    /// An SMF of `format` and `division` over `tracks`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] if there is no track, a format 0 file does not have
    /// exactly one, a track's events are out of order or past its end, two events are further apart than
    /// the wire's 28-bit delta, a message is not a MIDI 1.0 channel
    /// message of the first Group (the [`ConfigError`] names which and where); [`Error::Tempo`] if the tempo events do not make a
    /// [`TempoMap`].
    #[must_use = "an Err means no file was built"]
    pub fn smf(format: SmfFormat, division: SmfDivision, tracks: Vec<Track>) -> Result<Self> {
        Self::build(Container::Smf(format, division), tracks)
    }

    /// A Clip File of division `ticks_per_quarter` over one sequence and its
    /// [`header`](Track::header).
    ///
    /// # Errors
    ///
    /// [`Error::Config`] if the events or the
    /// header's are out of order or the events past the track's end, the header
    /// ends before its last message, a
    /// Flex Data message is interrupted by another message of its Group or
    /// left open (M2-104 section 7.5.2: only its own Continue packets and
    /// System Real Time messages go between its Start and End; a message that
    /// starts in the header and ends in the events is left open), or an
    /// event is not a packet (`SysEx`, `Escape` and `Meta` are SMF's), is a
    /// MIDI-CI Property Exchange message (sections 6.4 and 7.2) or is the
    /// file's own structure (a Delta Clockstamp, the DCTPQ, a Null, Start or End
    /// of Clip);
    /// [`Error::Clip`] if the header breaks a rule of sections 6.1 to 6.1.2
    /// (a second Set Tempo or Set Time Signature, one misplaced or not at delta
    /// 0);
    /// [`Error::Tempo`] if the Set Tempo messages do not make a
    /// [`TempoMap`].
    ///
    /// Only this constructor holds a clip to M2-104 section 7.5.2: a file read
    /// with [`from_bytes`](Self::from_bytes) keeps the Flex Data messages as
    /// its sender interleaved them. The rule is checked on the header and on
    /// the events each by itself, so a message that starts in the header and
    /// ends in the events is reported open. Its exemption for JR Clock
    /// messages holds without a check, as a clip refuses every Utility
    /// message.
    pub fn clip(ticks_per_quarter: TicksPerQuarterNote, track: Track) -> Result<Self> {
        for events in [&track.header, &track.events] {
            if let Some(at) = text_interrupted(events) {
                return Err(Error::Config(ConfigError::TextInterrupted { at }));
            }
        }
        Self::build(Container::Clip(ticks_per_quarter), vec![track])
    }

    fn build(container: Container, tracks: Vec<Track>) -> Result<Self> {
        let invalid = |why: ConfigError| Err(Error::Config(why));
        if tracks.is_empty() {
            return invalid(ConfigError::NoTracks);
        }
        if matches!(container, Container::Smf(SmfFormat::SingleTrack, _)) && tracks.len() != 1 {
            return invalid(ConfigError::SingleTrack {
                tracks: tracks.len(),
            });
        }
        if tracks.len() > usize::from(u16::MAX) {
            return invalid(ConfigError::TooManyTracks {
                tracks: tracks.len(),
            });
        }
        // The widest gap the wire holds: 28 bits in an SMF. A Clip File's
        // Delta Clockstamp restarts its count with a Null message, so any gap
        // is expressible (M2-116 section 3.2.2).
        let max_gap = match container {
            Container::Smf(..) => 0x0FFF_FFFF,
            Container::Clip(_) => u64::MAX,
        };
        for (i, track) in tracks.iter().enumerate() {
            if !track.header.is_empty() || track.header_end != Tick(0) {
                if !matches!(container, Container::Clip(_)) {
                    return invalid(ConfigError::HeaderInSmf { track: i });
                }
                let mut at = Tick(0);
                for (tick, event) in &track.header {
                    if *tick < at {
                        return invalid(ConfigError::HeaderOutOfOrder {
                            track: i,
                            at: *tick,
                        });
                    }
                    at = *tick;
                    if let Some(refusal) = refuses(container, event) {
                        return invalid(ConfigError::Refused {
                            track: i,
                            in_header: true,
                            refusal,
                        });
                    }
                }
                clip_file::check_header(&clip_packets(&track.header, true)?)?;
                if track.header_end < at {
                    return invalid(ConfigError::HeaderEndBeforeLast {
                        track: i,
                        end: track.header_end,
                    });
                }
            }
            let mut at = Tick(0);
            for (tick, event) in &track.events {
                if *tick < at {
                    return invalid(ConfigError::OutOfOrder {
                        track: i,
                        at: *tick,
                    });
                }
                if tick.0 - at.0 > max_gap {
                    return invalid(ConfigError::Gap {
                        track: i,
                        at: *tick,
                    });
                }
                at = *tick;
                if let Some(refusal) = refuses(container, event) {
                    return invalid(ConfigError::Refused {
                        track: i,
                        in_header: false,
                        refusal,
                    });
                }
            }
            if track.end < at {
                return invalid(ConfigError::EndBeforeLast {
                    track: i,
                    end: track.end,
                });
            }
            if track.end.0 - at.0 > max_gap {
                return invalid(ConfigError::EndTooFar {
                    track: i,
                    end: track.end,
                });
            }
        }
        let tempo_maps = match container {
            Container::Smf(SmfFormat::Sequential, _) => tracks
                .iter()
                .map(|t| tempo_map(container, std::slice::from_ref(t)))
                .collect::<Result<_>>()?,
            _ => vec![tempo_map(container, &tracks)?],
        };
        Ok(Self {
            container,
            tracks,
            tempo_maps,
        })
    }

    /// Decodes a complete MIDI file from its bytes, detecting the container
    /// from its magic: `MThd` for an SMF, `SMF2CLIP` for a Clip File.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownFormat`] when the magic matches neither container (no
    ///   decoder runs).
    /// - [`Error::Truncated`] when an SMF is cut off: a chunk that runs past
    ///   the end, or fewer tracks than the header declares.
    /// - [`Error::MidiUnsupportedTiming`] for an SMF in SMPTE division. A
    ///   timecode file has no beat grid to put a tempo map on, and importing
    ///   one means choosing a tempo, which is the caller's to say.
    /// - [`Error::MidiFileParse`] for any other malformed SMF, a zero division
    ///   included.
    /// - [`Error::Clip`] for a malformed Clip File, naming the rule it broke.
    /// - [`Error::Tempo`] if the tempo events do not make a [`TempoMap`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        match MidiFileKind::detect(bytes) {
            Some(MidiFileKind::StandardMidiFile) => {
                let parts = smf::read(bytes)?;
                Self::build(Container::Smf(parts.format, parts.division), parts.tracks)
            }
            Some(MidiFileKind::ClipFile) => {
                let clip = clip_file::parse(bytes)?;
                let events = |packets: Vec<(u64, Ump)>| -> Vec<(Tick, FileEvent)> {
                    packets
                        .into_iter()
                        .map(|(tick, ump)| {
                            let event = match ump.decode() {
                                Ok(message) => FileEvent::Message(message),
                                Err(unknown) => FileEvent::Unknown(unknown),
                            };
                            (Tick(tick), event)
                        })
                        .collect()
                };
                let track = Track {
                    header: events(clip.header),
                    events: events(clip.events),
                    header_end: Tick(clip.header_end),
                    end: Tick(clip.end),
                };
                Self::build(Container::Clip(clip.ticks_per_quarter), vec![track])
            }
            // Its own error rather than a parse failure: "this is not a MIDI
            // file" is a different thing to fix than "this MIDI file is
            // malformed".
            None => Err(Error::UnknownFormat),
        }
    }

    /// Reads and decodes the MIDI file at `path`. See [`from_bytes`](Self::from_bytes).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be read; otherwise as
    /// [`from_bytes`](Self::from_bytes).
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path.as_ref()).map_err(Error::io(IoStep::Read))?)
    }

    /// The file as bytes, in its own container: every event written back at
    /// its tick. Running status and the spelling of a Delta Clockstamp may
    /// differ from the file it was read from; a read of the result is equal to
    /// this file.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooLarge`] (as [`Error::Config`]) if the encoder
    /// refuses (an SMF past 65535 tracks or an event of 256 MB, a Clip File
    /// whose gaps need over 4 GB of Null restarts).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        match self.container {
            Container::Smf(format, division) => smf::write(format, division, &self.tracks),
            Container::Clip(ticks_per_quarter) => {
                let track = &self.tracks[0];
                if clip_file::restart_bytes(track.header_end.max(track.end).0) > u64::from(u32::MAX)
                {
                    return Err(ConfigError::TooLarge.into());
                }
                Ok(clip_file::write(
                    ticks_per_quarter,
                    &clip_packets(&track.header, true)?,
                    track.header_end.0,
                    &clip_packets(&track.events, false)?,
                    track.end.0,
                ))
            }
        }
    }

    /// Writes [`to_bytes`](Self::to_bytes) to `path`, refusing anything
    /// already there. Use [`write_options`](Self::write_options) to replace one.
    ///
    /// The bytes are staged in a temporary file beside `path` and renamed into
    /// place, so a crash leaves no partial file under `path`.
    ///
    /// ```
    /// # use tutti_midi_file::{Error, MidiFile, SmfDivision, SmfFormat, Track};
    /// # let dir = tempfile::tempdir()?;
    /// # let path = dir.path().join("song.mid");
    /// let file = MidiFile::smf(SmfFormat::SingleTrack, SmfDivision::of(480), vec![Track::new()])?;
    /// file.write(&path)?;
    /// assert!(matches!(file.write(&path), Err(Error::Exists { .. })));
    /// file.write_options().replace().write(&path)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Exists`] if something is at `path` (it is untouched, and
    /// `self` can be written again); [`Error::Io`] if the staging or the
    /// rename fails. A filesystem without an atomic no-replace rename falls
    /// back to a hard link; on one with neither the write fails with
    /// [`Error::Io`] and never overwrites. Otherwise as
    /// [`to_bytes`](Self::to_bytes).
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        self.write_options().write(path)
    }

    /// How to write this file to a path: [`replace`](WriteOptions::replace)
    /// overwrites an existing file, and [`write`](WriteOptions::write) ends
    /// the call.
    pub fn write_options(&self) -> WriteOptions<'_> {
        WriteOptions {
            file: self,
            existing: Existing::Refuse,
        }
    }

    /// The division: ticks per quarter note.
    #[must_use]
    pub fn ticks_per_quarter(&self) -> TicksPerQuarterNote {
        self.container.ticks_per_quarter()
    }

    /// The tracks, as the file has them (a Clip File's is one).
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
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
    /// For a format 2 file, which has a timeline per sequence, this is the
    /// first sequence's; [`tempo_map_of`](Self::tempo_map_of) names one.
    ///
    /// It is shared: `file.tempo_map().clone()` is a reference count.
    pub fn tempo_map(&self) -> &Arc<TempoMap> {
        &self.tempo_maps[0]
    }

    /// The tempo map `track` is read on: the file's for a file of one timeline,
    /// the track's own for a format 2 file. `None` past the last track.
    pub fn tempo_map_of(&self, track: usize) -> Option<&Arc<TempoMap>> {
        if track >= self.tracks.len() {
            return None;
        }
        Some(self.tempo_maps.get(track).unwrap_or(&self.tempo_maps[0]))
    }

    /// `tick` in quarter notes: the tick over the division.
    #[must_use]
    pub fn beat_at(&self, tick: Tick) -> Beat {
        tick.beats(self.ticks_per_quarter())
    }

    /// The tick nearest `beat`: the inverse of [`beat_at`](Self::beat_at),
    /// over the division alone (a file's division is always ticks per quarter,
    /// and tempo does not enter). Rounds as [`Tick::from_beat`] does: halfway
    /// goes to the later tick, and `None` is a beat before 0 or past the last
    /// tick.
    #[must_use]
    pub fn tick_at(&self, beat: Beat) -> Option<Tick> {
        Tick::from_beat(beat, self.ticks_per_quarter())
    }

    /// Seconds from beat 0 to `at` over the [`tempo_map`](Self::tempo_map),
    /// each stretch between two changes at its own tempo (a ramp at its own
    /// slope: a file's changes are all steps). What an offline render of the
    /// file lasts: `seconds_at(end_beat())` is the span a bounce of the file renders (`Range::Until(end_beat())`).
    pub fn seconds_at(&self, at: Beat) -> Seconds {
        self.tempo_map().seconds_at(Beat(at.get().max(0.0)))
    }

    /// Where the file ends: its longest track's end (an SMF's End of Track, a
    /// Clip File's End of Clip), at least where the last event lands.
    pub fn end_beat(&self) -> Beat {
        let end = self.tracks.iter().map(|t| t.end).max().unwrap_or_default();
        self.beat_at(end)
    }

    /// Every event of the file's one timeline (a Clip File's
    /// [`header`](Track::header) is not on it) as `(position, event)`, in
    /// order: the tracks merged by tick, ties in track order and then in the
    /// track's own order.
    ///
    /// # Errors
    ///
    /// [`NoCommonTimeline`] for a format 2 file, whose sequences are
    /// independent and have no common timeline to merge on: read each
    /// [`Track`] with [`beat_at`](Self::beat_at).
    pub fn events(
        &self,
    ) -> std::result::Result<impl Iterator<Item = (Beat, &FileEvent)>, NoCommonTimeline> {
        if matches!(self.container, Container::Smf(SmfFormat::Sequential, _)) {
            return Err(NoCommonTimeline);
        }
        let mut all: Vec<(Tick, &FileEvent)> = self
            .tracks
            .iter()
            .flat_map(|t| t.events.iter().map(|(tick, event)| (*tick, event)))
            .collect();
        // Stable: equal ticks keep track order, then the track's own.
        all.sort_by_key(|&(tick, _)| tick);
        Ok(all
            .into_iter()
            .map(move |(tick, event)| (self.beat_at(tick), event)))
    }
}

impl MidiFile {
    /// The file's one timeline as the packets a clip plays, `(Beat, Ump)` in
    /// order ([`events`](Self::events)' order). Every beat is finite. Refused for a
    /// format 2 file, which has no one timeline: read each [`Track`] with
    /// [`Track::timeline`].
    ///
    /// # Errors
    ///
    /// [`NoCommonTimeline`] for a format 2 file.
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
    /// What has no packet is in [`Timeline::dropped`] with its
    /// [`TimelineDrop`], not lost silently: a bare `F7` escape (the arbitrary
    /// bytes SMF 1.0 section 3.2 allows, which no packet holds), a divided
    /// message left unfinished, one with a byte of 128 or more, and an
    /// [`Unknown`](FileEvent::Unknown) packet.
    ///
    /// The Clip File's [`header`](Track::header) is not on the timeline.
    ///
    /// ```
    /// use tutti_midi_file::{MidiFile, SmfDivision, SmfFormat, Tick, Track};
    /// use tutti_types::Beat;
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let track = Track::new().note(Tick(480), Tick(960), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
    /// let file = MidiFile::smf(SmfFormat::SingleTrack, SmfDivision::of(480), vec![track])?;
    /// let clip = file.timeline()?;
    /// assert_eq!(clip.packets.len(), 2);
    /// assert_eq!(clip.packets[0].0, Beat(1.0));
    /// assert_eq!(clip.packets[1].0, Beat(2.0));
    /// assert!(clip.dropped.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn timeline(&self) -> std::result::Result<Timeline, NoCommonTimeline> {
        Ok(Timeline::of(self.events()?))
    }
}

/// A format 2 file has no one timeline: its sequences are independent
/// ([`MidiFile::events`], [`MidiFile::timeline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a format 2 file has no common timeline: its sequences are independent")]
pub struct NoCommonTimeline;

/// Events as the packets a clip plays ([`MidiFile::timeline`]), and the events
/// that gave none.
#[derive(Clone, Debug, PartialEq, Default)]
#[must_use = "`dropped` lists the events with no packet"]
#[non_exhaustive]
pub struct Timeline {
    /// The packets at their beats, in order.
    pub packets: Vec<(Beat, Ump)>,
    /// The [`SysEx`](FileEvent::SysEx), [`Escape`](FileEvent::Escape) and
    /// [`Unknown`](FileEvent::Unknown) events that have no packet, at their
    /// beats and with why, in order. A divided message that gave none is all
    /// of its parts.
    pub dropped: Vec<(Beat, FileEvent, TimelineDrop)>,
}

/// Why an event of [`MidiFile::timeline`] has no packet ([`Timeline::dropped`]).
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

/// The most data bytes of a SysEx7 packet (M2-104 section 7.7).
const SYSEX7_PACKET_BYTES: usize = 6;

impl Timeline {
    /// The packets of `events`, by [`MidiFile::timeline`]'s rule.
    fn of<'a>(events: impl Iterator<Item = (Beat, &'a FileEvent)>) -> Self {
        let mut timeline = Self::default();
        // The parts of the divided message being read: its `F0` event, then
        // the `F7` events after it.
        let mut open: Vec<(Beat, &FileEvent)> = Vec::new();
        for (beat, event) in events {
            match event {
                FileEvent::SysEx(data) => {
                    timeline.abandon(&mut open);
                    if ends_in_f7(data) {
                        timeline.complete(&[(beat, event)]);
                    } else {
                        open.push((beat, event));
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
                let (FileEvent::SysEx(data) | FileEvent::Escape(data)) = event else {
                    return Some((beat, Vec::new()));
                };
                let data = if i + 1 == parts.len() {
                    &data[..data.len() - 1]
                } else {
                    data
                };
                data.iter()
                    .map(|&b| U7::new(b))
                    .collect::<Option<Vec<U7>>>()
                    .map(|bytes| (beat, bytes))
            })
            .collect();
        let Some(payloads) = payloads else {
            for &(beat, event) in parts {
                self.discard(beat, event, TimelineDrop::NotSevenBit);
            }
            return;
        };
        // An empty part still sends a packet: its pause is the message's.
        let chunks: Vec<(Beat, Vec<u8>)> = payloads
            .iter()
            .flat_map(|(beat, payload)| {
                let mut chunks: Vec<Vec<u8>> = payload
                    .chunks(SYSEX7_PACKET_BYTES)
                    .map(|chunk| chunk.iter().map(|u| u.get()).collect())
                    .collect();
                if chunks.is_empty() {
                    chunks.push(Vec::new());
                }
                chunks.into_iter().map(move |chunk| (*beat, chunk))
            })
            .collect();
        let last = chunks.len() - 1;
        for (i, (beat, chunk)) in chunks.into_iter().enumerate() {
            let status = match i {
                0 if last == 0 => SysexStatus::Complete,
                0 => SysexStatus::Start,
                i if i == last => SysexStatus::End,
                _ => SysexStatus::Continue,
            };
            // A chunk is at most `SYSEX7_PACKET_BYTES` bytes, each a `U7`.
            let packet = Sysex7Packet::new(status, &chunk)
                .expect("a chunk of at most six bytes below 128 is a packet");
            self.packets
                .push((beat, Ump::from(Message::Sysex7(MidiGroup::FIRST, packet))));
        }
    }
}

/// The packets of a Clip File's events (`in_header`: its header's) at their
/// ticks.
///
/// # Errors
///
/// [`ConfigError::Refused`] for an SMF event: the constructor refuses it
/// first.
fn clip_packets(events: &[(Tick, FileEvent)], in_header: bool) -> Result<Vec<(u64, Ump)>> {
    events
        .iter()
        .map(|(tick, event)| match event {
            FileEvent::Message(m) => Ok((tick.0, Ump::from(*m))),
            FileEvent::Unknown(u) => Ok((tick.0, u.0)),
            FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_) => {
                Err(ConfigError::Refused {
                    track: 0,
                    in_header,
                    refusal: Refusal::ClipSmfEvent,
                }
                .into())
            }
        })
        .collect()
}

/// The packets of a header, at their ticks: what the clip layer reads.
fn header_packets(header: &[(Tick, FileEvent)]) -> Vec<(u64, Ump)> {
    header
        .iter()
        .filter_map(|(tick, event)| match event {
            FileEvent::Message(m) => Some((tick.0, Ump::from(*m))),
            FileEvent::Unknown(u) => Some((tick.0, u.0)),
            FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_) => None,
        })
        .collect()
}

impl TryFrom<&[u8]> for MidiFile {
    type Error = Error;

    /// [`MidiFile::from_bytes`].
    fn try_from(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes(bytes)
    }
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

/// Where a Flex Data message is interrupted (M2-104 section 7.5.2: within a
/// Group, only Continue packets of the message and System Real Time messages
/// go between its Start and End): the tick of the first message of a Group
/// with a message open that is not one of those, or a Continue or End with no
/// message of its own open, or of the last event if a message is left open.
fn text_interrupted(events: &[(Tick, FileEvent)]) -> Option<Tick> {
    let mut open: HashMap<MidiGroup, (FlexAddress, FlexTextKind)> = HashMap::new();
    for (tick, event) in events {
        let ump = match event {
            FileEvent::Message(message) => Ump::from(*message),
            FileEvent::Unknown(unknown) => unknown.0,
            FileEvent::SysEx(_) | FileEvent::Escape(_) | FileEvent::Meta(_) => continue,
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
        Message::Flex(_, Flex::SetTempo(field)) => flex_tempo(*field),
        _ => None,
    }
}

/// The tempo map of one timeline: `tracks`' Set Tempo events as steps.
fn tempo_map(container: Container, tracks: &[Track]) -> Result<Arc<TempoMap>> {
    let ticks_per_quarter = container.ticks_per_quarter();
    // The MIDI default, which a file that sets none at its start plays at.
    let default = Bpm::of(120.0);
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
