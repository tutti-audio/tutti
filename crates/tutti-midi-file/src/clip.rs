//! [`ClipFile`]: a MIDI 2.0 Clip File (M2-116) as its three parts, the
//! [`ClipHeader`] (section 6, "Clip Configuration Header"), the
//! [`ClipSequence`] (section 7, "Sequence Data") and the division that counts
//! their ticks.

use std::collections::HashMap;
use std::sync::Arc;

use tutti_midi_ci::ProfileOnRequest;
use tutti_music::MeterMap;
use tutti_platform::Destination;
use tutti_tempo::TempoMap;
use tutti_ump::{
    Address, Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, NoteNumber, Stream, System,
    TicksPerQuarterNote, U7, Ump, Unknown, Voice1,
};
use tutti_units::{Beat, Bpm, Seconds};

use crate::file::{Stated, meter_of, pair_notes, tempo_map_of};
use crate::midi_file::write_bytes;
use crate::ordered::Ordered;
use crate::{
    ClipBuildError, ClipFileError, ClipPart, ClipReadError, ClipRefusal, OutOfOrder, PairedNote,
    RefusedTimeSignature, Tick, WriteError, clip_file,
};

/// A Clip File's configuration header (M2-116 section 6): the messages
/// between the file header and Start of Clip that configure a receiver (Set
/// Profile On, Bank Select, Program Change, volume, System Exclusive, and at
/// most one Set Tempo and one Set Time Signature), in order. They are not
/// events of the sequence and do not sound at its start. Their ticks are the
/// header's own clock, counted from the DCTPQ; the leading Set Profile On
/// messages are at 0.
///
/// The header ends where Start of Clip is, on its own clock
/// ([`end`](Self::end)): M2-116 section 6.1 has the Delta Clockstamps up to
/// and including the one for Start of Clip add up to the time allocated to
/// the header, which a receiver may need to configure itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipHeader {
    run: Ordered<Ump>,
}

impl ClipHeader {
    /// An empty header, ending at tick 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A header a decoder assembled: in order by construction.
    pub(crate) fn from_read(messages: Vec<(Tick, Ump)>, end: Tick) -> Self {
        Self {
            run: Ordered::from_read(messages, end),
        }
    }

    /// Appends `message` at `tick`, which is not before the last message's.
    /// The header's end moves to `tick` if it is past it.
    ///
    /// # Errors
    ///
    /// [`OutOfOrder`] if `tick` is before the last message's, naming both; the
    /// message is not added.
    pub fn push(mut self, tick: Tick, message: impl Into<Ump>) -> Result<Self, OutOfOrder> {
        self.run.push(tick, message.into())?;
        Ok(self)
    }

    /// The header ends at `end`, or where it ends already if that is later:
    /// never before its last message.
    #[must_use]
    pub fn ending_at(mut self, end: Tick) -> Self {
        self.run.end_at_least(end);
        self
    }

    /// A Set Profile On at the start of the header, after the ones already
    /// there (M2-116 section 6.2: data meant for a Profile is tagged with a
    /// Set Profile On, with that Profile Id, at the beginning of the header).
    /// The message is at tick 0 and carries no Delta Clockstamp when written
    /// (section 6); it is a [`ProfileOnRequest`], whose MUIDs are Broadcast.
    ///
    /// ```
    /// use tutti_midi_ci::{ProfileId, ProfileOnRequest, ProfileTarget};
    /// use tutti_midi_file::ClipHeader;
    ///
    /// let on = ProfileOnRequest { target: ProfileTarget::Group, profile: ProfileId::MPE };
    /// let header = ClipHeader::new().profile_on(on);
    /// assert_eq!(header.profiles(), vec![on]);
    /// ```
    #[must_use]
    pub fn profile_on(mut self, request: ProfileOnRequest) -> Self {
        let leading = clip_file::leading_profile_len(self.run.events());
        for (offset, packet) in clip_file::profile_packets(request).enumerate() {
            self.run.insert_at(leading + offset, Tick(0), packet);
        }
        self
    }

    /// The Set Profile On messages at the head of the header, in order, which
    /// section 6.2 puts there. A header holds only ones that read: a file
    /// whose do not is refused when it is read, and
    /// [`profile_on`](Self::profile_on) writes only whole ones.
    #[must_use]
    pub fn profiles(&self) -> Vec<ProfileOnRequest> {
        let packets = self.run.events();
        let leading = clip_file::leading_profile_len(packets);
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

    /// The messages, in order, each at its tick on the header's own clock.
    #[must_use]
    pub fn messages(&self) -> &[(Tick, Ump)] {
        self.run.events()
    }

    /// The messages nothing of the file's timeline is built from, in order: all
    /// but a Set Tempo with a tempo, which is a step of
    /// [`ClipFile::tempo_map`], and a Set Time Signature, which is a change of
    /// [`ClipFile::meter_map`] or is listed in
    /// [`ClipFile::refused_time_signatures`]. A Set Tempo of 0 names no tempo
    /// and is one of them, as a Program Change and a Set Metronome are.
    pub fn unused_messages(&self) -> impl Iterator<Item = (Tick, Ump)> + '_ {
        self.messages()
            .iter()
            .copied()
            .filter(|&(_, ump)| matches!(Statement::of(ump), Statement::Unused))
    }

    /// Where the header ends, on its own clock: the tick of Start of Clip,
    /// which is the last message's tick plus the Delta Clockstamp before Start
    /// of Clip. Not before the last message; `Tick(0)` for a header that asks
    /// for no time.
    #[must_use]
    pub fn end(&self) -> Tick {
        self.run.end()
    }
}

/// A Clip File's Sequence Data (M2-116 section 7): packets in tick order, each
/// at an absolute [`Tick`] counted from Start of Clip, and where the clip ends.
///
/// The builders ([`note`](Self::note), [`push`](Self::push)) and [`Extend`]
/// keep the events in tick order and the end at or after the last one, so a
/// sequence a caller builds is always ordered. The events are packets as
/// stored: the file layer does not translate, and a MIDI 1.0 Note On at
/// velocity 0 stays one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipSequence {
    run: Ordered<Ump>,
}

impl ClipSequence {
    /// An empty sequence, ending at tick 0.
    ///
    /// ```
    /// use tutti_midi_file::{ClipSequence, Tick};
    /// use tutti_ump::{MidiChannel, NoteNumber, U7};
    ///
    /// let sequence = ClipSequence::new()
    ///     .note(Tick(0), Tick(96), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
    ///     .note(Tick(96), Tick(192), MidiChannel::FIRST, NoteNumber::of(64), U7::of(100));
    /// assert_eq!(sequence.end(), Tick(192));
    /// assert_eq!(sequence.events().len(), 4);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sequence a decoder or converter assembled: in order by construction.
    pub(crate) fn from_read(events: Vec<(Tick, Ump)>, end: Tick) -> Self {
        Self {
            run: Ordered::from_read(events, end),
        }
    }

    /// A note: a Note On at `start` and its Note Off at `end`, at `address` (a
    /// bare [`MidiChannel`](tutti_ump::MidiChannel) is in the first Group).
    /// The Note Off carries the MIDI release velocity of 64, which means none.
    /// An `end` before `start` is `start`: a note of no length.
    ///
    /// Events are placed in tick order, after the events already at their
    /// tick, so notes may be added in any order, overlap, or start together
    /// (a chord). At one tick the events are in the order they were added.
    #[must_use]
    pub fn note(
        mut self,
        start: Tick,
        end: Tick,
        address: impl Into<Address>,
        note: NoteNumber,
        velocity: U7,
    ) -> Self {
        let address = address.into();
        self.run.place(
            start,
            Message::midi1(address, Voice1::note_on(note, velocity)).into(),
        );
        self.run.place(
            end.max(start),
            Message::midi1(address, Voice1::note_off(note, U7::of(64))).into(),
        );
        self
    }

    /// Appends `message` at `tick`: a [`Message`] built with
    /// [`Message::midi1`] and its kin, or any [`Ump`]. Where
    /// [`note`](Self::note) places an event by its tick, this one is the
    /// strict door: the events do not go back in time. The end moves to
    /// `tick` if it is past it.
    ///
    /// # Errors
    ///
    /// [`OutOfOrder`] if `tick` is before the last event's, naming both; the
    /// event is not added.
    pub fn push(mut self, tick: Tick, message: impl Into<Ump>) -> Result<Self, OutOfOrder> {
        self.run.push(tick, message.into())?;
        Ok(self)
    }

    /// The sequence ends at `end`, or where it ends already if that is later:
    /// the end is never before the last event. A Clip File's End of Clip is
    /// written there.
    #[must_use]
    pub fn ending_at(mut self, end: Tick) -> Self {
        self.run.end_at_least(end);
        self
    }

    /// The events, in order; their ticks do not decrease.
    #[must_use]
    pub fn events(&self) -> &[(Tick, Ump)] {
        self.run.events()
    }

    /// Where the sequence ends: the Clip File's End of Clip. Not before the
    /// last event.
    #[must_use]
    pub fn end(&self) -> Tick {
        self.run.end()
    }

    /// The sequence's notes, a Note On paired with its Note Off, in order of
    /// onset and then `(group, channel, note)`; the pairing is
    /// [`Track::notes`](crate::Track::notes)'s.
    ///
    /// This walks the whole sequence, allocates the list and sorts it on every
    /// call: keep the result rather than calling it again.
    #[must_use]
    pub fn notes(&self) -> Vec<PairedNote> {
        pair_notes(self.run.events().iter().copied())
    }
}

/// A sequence of the events, in tick order: a stable sort, so events at one
/// tick stay in the order given. Ends at the last of them.
impl FromIterator<(Tick, Ump)> for ClipSequence {
    fn from_iter<I: IntoIterator<Item = (Tick, Ump)>>(events: I) -> Self {
        let mut sequence = ClipSequence::new();
        sequence.extend(events);
        sequence
    }
}

/// Adds the events, then puts the sequence in tick order (a stable sort, so
/// events at one tick stay in the order added) and moves its end to the last
/// event's if that is past it.
impl Extend<(Tick, Ump)> for ClipSequence {
    fn extend<I: IntoIterator<Item = (Tick, Ump)>>(&mut self, events: I) {
        self.run.extend(events);
    }
}

/// A MIDI 2.0 Clip File (M2-116): the division that counts its ticks, a
/// [`ClipHeader`] and a [`ClipSequence`], and the `TempoMap` its Set Tempo
/// messages declare.
///
/// Fields are private and the tempo map is derived from the messages (the
/// header's Set Tempo and the sequence's Flex Data Set Tempo messages), so the
/// two never disagree. The packets are as stored: a Clip File read and written
/// holds the same packets, word for word.
///
/// ```
/// use tutti_midi_file::{ClipFile, ClipHeader, ClipSequence, Tick};
/// use tutti_ump::{MidiChannel, NoteNumber, TicksPerQuarterNote, U7};
///
/// let sequence = ClipSequence::new()
///     .note(Tick(0), Tick(96), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100));
/// let clip = ClipFile::new(TicksPerQuarterNote::of(96), ClipHeader::new(), sequence)?;
///
/// let mut bytes = Vec::new();
/// clip.write(&mut bytes)?;
/// assert_eq!(ClipFile::from_bytes(&bytes)?, clip);
/// assert_eq!(clip.end(), Tick(96));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct ClipFile {
    ticks_per_quarter: TicksPerQuarterNote,
    header: ClipHeader,
    sequence: ClipSequence,
    tempo_map: Arc<TempoMap>,
}

/// Two Clip Files are equal when their division, header and sequence are; the
/// tempo map is derived from them.
impl PartialEq for ClipFile {
    fn eq(&self, other: &Self) -> bool {
        self.ticks_per_quarter == other.ticks_per_quarter
            && self.header == other.header
            && self.sequence == other.sequence
    }
}

impl Eq for ClipFile {}

impl ClipFile {
    /// A Clip File of division `ticks_per_quarter` over `header` and
    /// `sequence`.
    ///
    /// # Errors
    ///
    /// [`ClipBuildError`] if a Flex Data message is interrupted by another
    /// message of its Group or left open (M2-104 section 7.5.2: only its own
    /// Continue packets and System Real Time messages go between its Start
    /// and End; a message that starts in the header and ends in the sequence
    /// is left open), a message is a Utility message or is the file's own
    /// structure (Start or End of Clip), the header or the sequence holds a
    /// MIDI-CI Property Exchange message (sections 6.4 and 7.2), the header
    /// breaks a rule of sections 6.1 to 6.1.2 (a second Set Tempo or Set Time
    /// Signature, one misplaced or not at delta 0), a gap needs more Null
    /// restarts than the writer holds, or the Set Tempo messages do not make a
    /// [`TempoMap`].
    ///
    /// Only this constructor holds a clip to M2-104 section 7.5.2: a file read
    /// with [`from_bytes`](Self::from_bytes) keeps the Flex Data messages as
    /// its sender interleaved them. The rule is checked on the header and on
    /// the sequence each by itself, so a message that starts in the header and
    /// ends in the sequence is reported open. Its exemption for JR Clock
    /// messages holds without a check, as a clip refuses every Utility
    /// message.
    #[must_use = "an Err means no file was built"]
    pub fn new(
        ticks_per_quarter: TicksPerQuarterNote,
        header: ClipHeader,
        sequence: ClipSequence,
    ) -> Result<Self, ClipBuildError> {
        for (part, messages) in [
            (ClipPart::Header, header.messages()),
            (ClipPart::Sequence, sequence.events()),
        ] {
            if let Some(at) = text_interrupted(messages) {
                return Err(ClipBuildError::TextInterrupted { part, at });
            }
        }
        Self::build(ticks_per_quarter, header, sequence)
    }

    /// The file of `header` and `sequence`, held to the format's rules.
    fn build(
        ticks_per_quarter: TicksPerQuarterNote,
        header: ClipHeader,
        sequence: ClipSequence,
    ) -> Result<Self, ClipBuildError> {
        check_part(ClipPart::Header, header.messages())?;
        clip_file::check_header(header.messages())?;
        check_part(ClipPart::Sequence, sequence.events())?;
        if clip_file::restart_bytes(header.end().max(sequence.end())) > u64::from(u32::MAX) {
            return Err(ClipBuildError::TooLarge);
        }
        let tempo_map = tempo_map(ticks_per_quarter, &header, &sequence)?;
        Ok(Self {
            ticks_per_quarter,
            header,
            sequence,
            tempo_map,
        })
    }

    /// Decodes a complete Clip File from `bytes`: `SMF2CLIP` and the packets
    /// after it.
    ///
    /// A read leaves out what is no event: the Null messages (a write puts
    /// them back where a long gap needs them), the JR Clock and JR Timestamp
    /// messages, and the Utility messages M2-104 does not define. A packet no
    /// definition covers is kept as its words, so it writes back unchanged.
    ///
    /// # Errors
    ///
    /// [`ClipReadError::Parse`] for a malformed or cut-off file, naming the
    /// rule it broke and the word it was found at (a file that does not begin
    /// with the magic is [`ClipFileError::BadMagic`]), and
    /// [`ClipReadError::Build`] if the messages are not a file the
    /// constructor holds (a header whose Set Profile On does not read is
    /// [`ClipFileError::ProfileOn`] there, and the tempo messages must make a
    /// tempo map).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ClipReadError> {
        let Some(body) = bytes.strip_prefix(&clip_file::CLIP_FILE_MAGIC) else {
            return Err(ClipReadError::Parse {
                word: 0,
                cause: ClipFileError::BadMagic,
            });
        };
        let parsed = clip_file::parse(body)?;
        Ok(Self::build(
            parsed.ticks_per_quarter,
            ClipHeader::from_read(parsed.header, parsed.header_end),
            ClipSequence::from_read(parsed.events, parsed.end),
        )?)
    }

    /// Writes the file to `dst` (any `impl Into<Destination>`: a path, or a
    /// `&mut` [`Write`](std::io::Write) such as a `Vec<u8>`): every packet
    /// written back at its tick, the spelling of a Delta Clockstamp possibly
    /// different from the file it was read from; a read of the result is
    /// equal to this file. A path is written through
    /// [`Staged`](tutti_platform::Staged), as
    /// [`StandardMidiFile::write`](crate::StandardMidiFile::write) describes.
    ///
    /// # Errors
    ///
    /// [`WriteError`], as [`StandardMidiFile::write`](crate::StandardMidiFile::write).
    pub fn write<'a>(&self, dst: impl Into<Destination<'a>>) -> Result<(), WriteError> {
        let bytes = clip_file::write(
            self.ticks_per_quarter,
            self.header.messages(),
            self.header.end(),
            self.sequence.events(),
            self.sequence.end(),
        );
        write_bytes(&bytes, dst.into())
    }

    /// The division: ticks per quarter note (the DCTPQ).
    #[must_use]
    pub fn ticks_per_quarter(&self) -> TicksPerQuarterNote {
        self.ticks_per_quarter
    }

    /// The configuration header.
    #[must_use]
    pub fn header(&self) -> &ClipHeader {
        &self.header
    }

    /// The Sequence Data.
    #[must_use]
    pub fn sequence(&self) -> &ClipSequence {
        &self.sequence
    }

    /// The tempo map of the clip, built from its Set Tempo messages (the
    /// header's, then the sequence's Flex Data messages), steps at the tick
    /// each lands on. A clip that sets none at the start plays at 120 BPM
    /// until the first, the MIDI default. Two at one tick: the later governs,
    /// as a sequencer applying them in order would. The map is derived from
    /// the messages and never set apart from them, and a zero (no valid
    /// tempo) is kept but makes no step. The header's Set Tempo is the tempo
    /// from the sequence's first tick, and a Set Tempo at tick 0 of the
    /// sequence overrides it.
    ///
    /// The map is the file's data. Reading or loading the file leaves a
    /// session's tempo as it was: a project adopts the map only through an
    /// explicit call of its own. It is shared: `clip.tempo_map().clone()` is a
    /// reference count.
    #[must_use]
    pub fn tempo_map(&self) -> &Arc<TempoMap> {
        &self.tempo_map
    }

    /// The meter of the clip: a change at the beat each Flex Data Set Time
    /// Signature message lands on (a change that falls mid-bar makes the bar
    /// before it short), 4/4 from beat 0 where the file declares none there.
    /// The header's Set Time Signature counts from beat 0, and a sequence
    /// message at the start replaces it (as for
    /// [`tempo_map`](Self::tempo_map)). A Time Signature no
    /// [`TimeSignature`](tutti_music::TimeSignature) holds, a denominator
    /// field of 0 included (M2-104 section 7.5.4 reads it as non-standard), is
    /// not a change: [`refused_time_signatures`](Self::refused_time_signatures)
    /// lists it.
    ///
    /// Like the tempo map, the meter is the file's data and a project adopts
    /// it only through an explicit call of its own.
    #[must_use]
    pub fn meter_map(&self) -> MeterMap {
        meter_of(self.stated()).0
    }

    /// The Set Time Signature messages of the clip that
    /// [`meter_map`](Self::meter_map) leaves out, in order.
    #[must_use]
    pub fn refused_time_signatures(&self) -> Vec<RefusedTimeSignature> {
        meter_of(self.stated()).1
    }

    /// The time signatures the clip states and the beats they land on: the
    /// header's at beat 0 (a Set Time Signature there holds from the
    /// sequence's first beat, as its Set Tempo does), then the sequence's.
    fn stated(&self) -> impl Iterator<Item = (Beat, Stated)> + '_ {
        let tpq = self.ticks_per_quarter;
        let header = self
            .header
            .messages()
            .iter()
            .map(|&(_, ump)| (Beat::new(0.0), ump));
        let sequence = self
            .sequence
            .events()
            .iter()
            .map(move |&(tick, ump)| (tick.beats(tpq), ump));
        header
            .chain(sequence)
            .filter_map(|(beat, ump)| match Statement::of(ump) {
                Statement::Meter(stated) => Some((beat, stated)),
                _ => None,
            })
    }

    /// Where the clip ends: its sequence's end, at least where the last event
    /// lands.
    #[must_use]
    pub fn end(&self) -> Tick {
        self.sequence.end()
    }

    /// Seconds from the start of the clip to `tick`, over its
    /// [`tempo_map`](Self::tempo_map): each stretch between two changes at its
    /// own tempo (a file's changes are all steps).
    ///
    /// What an offline render of the file lasts is `seconds_at(clip.end())`.
    #[must_use]
    pub fn seconds_at(&self, tick: Tick) -> Seconds {
        self.tempo_map
            .seconds_at(tick.beats(self.ticks_per_quarter))
    }

    /// The sequence as the packets a clip plays, `(Beat, Ump)` in order, as
    /// stored. The header is not on the timeline. A packet no definition
    /// covers (a reserved message type, an undefined status) has no message to
    /// play: it is in [`ClipTimeline::unknown`], not lost silently.
    pub fn timeline(&self) -> ClipTimeline {
        let tpq = self.ticks_per_quarter;
        let mut timeline = ClipTimeline::default();
        for &(tick, ump) in self.sequence.events() {
            let beat = tick.beats(tpq);
            match ump.decode() {
                Ok(_) => timeline.packets.push((beat, ump)),
                Err(unknown) => timeline.unknown.push((beat, unknown)),
            }
        }
        timeline
    }
}

/// A Clip File's sequence as the packets a clip plays ([`ClipFile::timeline`]),
/// and the packets that are no message.
#[derive(Clone, Debug, PartialEq, Default)]
#[must_use = "`unknown` lists the packets that are not played"]
#[non_exhaustive]
pub struct ClipTimeline {
    /// The packets at their beats, in order.
    pub packets: Vec<(Beat, Ump)>,
    /// The packets no definition covers, at their beats, in order.
    pub unknown: Vec<(Beat, Unknown)>,
}

/// What a Clip File's message says of its timeline: the one place that decides
/// which messages the tempo map and the meter are built from.
pub(crate) enum Statement {
    /// A Set Tempo with a tempo: a step of the tempo map. A Set Tempo of 0
    /// (M2-104 section 7.5.3) names no tempo and makes no step.
    Tempo(Bpm),
    /// A Set Time Signature: a change of the meter, or a time signature it
    /// refuses.
    Meter(Stated),
    /// A message the tempo map and the meter do not read.
    Unused,
}

impl Statement {
    pub(crate) fn of(ump: Ump) -> Self {
        match ump.decode() {
            Ok(Message::Flex(_, Flex::SetTempo(field))) => {
                field.bpm().map_or(Self::Unused, Self::Tempo)
            }
            Ok(Message::Flex(_, Flex::SetTimeSignature(field))) => {
                Self::Meter(Stated::flex(&field))
            }
            _ => Self::Unused,
        }
    }
}

/// The packets of `part` the format does not hold: a Utility message, the
/// file's own structure, a MIDI-CI Property Exchange message.
fn check_part(part: ClipPart, messages: &[(Tick, Ump)]) -> Result<(), ClipBuildError> {
    for (_, ump) in messages {
        let refusal = match ump.decode() {
            Ok(Message::Utility(_)) => ClipRefusal::Utility,
            Ok(Message::Stream(Stream::StartOfClip | Stream::EndOfClip)) => ClipRefusal::Structure,
            Ok(message) if clip_file::property_exchange(&message) => {
                return Err(ClipFileError::PropertyExchange.into());
            }
            _ => continue,
        };
        return Err(ClipBuildError::Refused { part, refusal });
    }
    Ok(())
}

/// The tempo map of a clip: its Set Tempo messages as steps.
fn tempo_map(
    ticks_per_quarter: TicksPerQuarterNote,
    header: &ClipHeader,
    sequence: &ClipSequence,
) -> Result<Arc<TempoMap>, tutti_tempo::TempoError> {
    let set_tempo = |ump: &Ump| match Statement::of(*ump) {
        Statement::Tempo(bpm) => Some(bpm),
        _ => None,
    };
    // The header runs before the sequence, so its tempo is the one at tick 0
    // (a Set Tempo at tick 0 of the sequence, later in the stream, overrides
    // it).
    let from_header = header
        .messages()
        .iter()
        .filter_map(|(_, ump)| set_tempo(ump).map(|bpm| (Tick(0), bpm)));
    let from_sequence = sequence
        .events()
        .iter()
        .filter_map(|(tick, ump)| set_tempo(ump).map(|bpm| (*tick, bpm)));
    tempo_map_of(
        ticks_per_quarter,
        from_header.chain(from_sequence).collect(),
    )
}

/// Where a Flex Data message is interrupted (M2-104 section 7.5.2: within a
/// Group, only Continue packets of the message and System Real Time messages
/// go between its Start and End): the tick of the first message of a Group
/// with a message open that is not one of those, or a Continue or End with no
/// message of its own open, or of the last event if a message is left open.
fn text_interrupted(events: &[(Tick, Ump)]) -> Option<Tick> {
    let mut open: HashMap<MidiGroup, (FlexAddress, FlexTextKind)> = HashMap::new();
    for &(tick, ump) in events {
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
                    _ => return Some(tick),
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
            _ if open.contains_key(&group) => return Some(tick),
            _ => {}
        }
    }
    if open.is_empty() {
        None
    } else {
        events.last().map(|&(tick, _)| tick)
    }
}
