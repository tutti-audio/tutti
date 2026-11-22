//! [`MetricalView::to_clip`]: a Standard MIDI File as a MIDI 2.0 Clip File.
//!
//! M2-116 section 3.3: the SMF 1 Meta Events are replaced by new
//! messages in the UMP Format, mostly Flex Data (message type D), and
//! Appendix B lists them. This module is that replacement as a function.
//! What has no replacement is returned beside the clip, never dropped
//! silently.

use core::num::NonZeroU8;

use tutti_midi_ci::is_property_exchange;
use tutti_ump::{
    BarAccents, Flex, FlexAddress, FlexTempo, FlexTimeSignature, KeySignature, Letter, Message,
    Metronome, MidiGroup, PlainTextKind, SharpsFlats, SyllableError, SysexMessage, Text,
    TicksPerQuarterNote, Ump, flex_text_packets,
};

use unicode_normalization::UnicodeNormalization;

use super::{Container, Lead, SysEx};
use crate::{FileEvent, Meta, MetricalView, MicrosPerQuarter, MidiFile, TextKind, Tick, Track};

use lyrics::Lyrics;

mod lyrics;

/// Why a file cannot be a clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ToClipError {
    /// The file is a Clip File already.
    #[error("the file is a Clip File already")]
    AlreadyClip,
    /// A gap in the file needs more Null restarts than the writer holds
    /// ([`TracksError::TooLarge`](crate::TracksError::TooLarge)).
    #[error("the clip is larger than the writer holds")]
    TooLarge,
}

/// Why an SMF event has no place in the clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DropReason {
    /// No UMP message replaces it (M2-116 Appendix B lists none): Sequence
    /// Number, Marker, Cue Point, Instrument, Program and Device Name,
    /// Sequencer Specific, any other meta event, and an `F7` escape.
    NoSuccessor,
    /// SMPTE Offset. M2-116 Appendix B names an SMPTE Offset message as its
    /// successor, defined in M2-104, but M2-104 v1.1.2 defines no such
    /// message, so there is nothing to write it as yet.
    SuccessorUndefined,
    /// Channel Prefix and MIDI Port Prefix. They address the meta events
    /// that follow to a channel or port, which the UMP does in the packet: a
    /// Flex Data message carries its own address and a group names the port.
    /// This conversion addresses every meta message to the whole group, so
    /// the prefix has nothing left to say.
    ReplacedByAddressing,
    /// The name of a track that is not the sequence's: a clip has no tracks
    /// to name (SMF 1.0 section 3.3: only the first track of a format 1 file
    /// names the sequence).
    TrackName,
    /// A text that is not UTF-8. Flex Data text is UTF-8 (M2-104 sections
    /// 1.8 and 7.5.9) and the SMF text is in no declared encoding, so it
    /// cannot be carried as it is.
    TextNotUtf8,
    /// A text or lyric syllable Flex Data cannot carry: one with a NUL, which
    /// is its padding, or one past the 384 bytes of a message.
    Text(SyllableError),
    /// A lyric with nothing but breaks and hyphens: it has no syllable, and
    /// no syllable before it for the break to end (see [`MetricalView::to_clip`]).
    LyricWithoutSyllable,
    /// A Key Signature past 7 sharps or flats, which the Flex message does
    /// not hold.
    KeySignatureOutOfRange,
    /// A Time Signature with a denominator power of 0 (a whole note): the
    /// Flex field reads 0 as non-standard (M2-104 section 7.5.4).
    WholeNoteDenominator,
    /// A Time Signature with a numerator of 0, a bar of no beats: the Flex
    /// field holds a value from 1 to 256 (M2-104 section 7.5.4).
    ZeroNumerator,
    /// A System Exclusive event that is not one complete message from `F0` to
    /// `F7`: the first packet of a divided message (its `F0` part has no `F7`).
    /// SysEx7 carries only the complete one.
    SysexNotComplete,
    /// A MIDI-CI Property Exchange message (Universal System Exclusive
    /// `7E`, sub-ID#1 `0D`, sub-ID#2 `30` to `3F`, M2-101 section 8):
    /// Property Data for PE Resources is not included in a Clip File
    /// (M2-116 section 6.4), and the sequence data holds no Property Exchange
    /// message (7.2).
    PropertyExchange,
    /// A Set Tempo or Time Signature that a later one of its kind replaced:
    /// a clip holds one of each at a position, and the later event wins
    /// where both land on one (see [`MetricalView::to_clip`]).
    Superseded,
    /// A meta event of a typed kind whose data the kind does not define
    /// ([`Meta::Malformed`]): its value cannot be read.
    MalformedMeta,
    /// A Set Tempo of zero microseconds per quarter note: no valid tempo, and a
    /// Flex Set Tempo field of 0 is none either (M2-104 section 7.5.3). It
    /// is not the file's first tempo, and the clip's default 120 BPM stands
    /// where nothing else sets one.
    ZeroTempo,
    /// A text event with no bytes. SMF gives it no meaning, and the Flex
    /// message it would become does: a Complete Lyric packet filled with
    /// zero bytes specifies a melisma (M2-104 section 7.5.10.1).
    EmptyText,
}

/// What of an SMF track has no place in the clip: an event, or one of the meta
/// events that open the track ([`Lead`]), which a track holds as fields.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmfPart {
    /// An event of the track.
    Event(FileEvent),
    /// A field of the track, at tick 0.
    Lead(Lead),
}

/// One part of the SMF that is not in the clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropped {
    /// The track the part was in.
    pub track: usize,
    /// Where it was.
    pub tick: Tick,
    /// The part.
    pub part: SmfPart,
    /// Why it has no place.
    pub reason: DropReason,
}

/// What reading a lyric as karaoke text changed of it ([`Altered`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LyricChange {
    /// A `/` (line break) or `\` (paragraph break) at the start of the text
    /// became the end of the syllable before it, and left the text.
    BreakMark,
    /// Hyphens at the end of a syllable left the text: they mark that the word
    /// goes on, which the syllable with no word end already says (M2-104
    /// section 7.5.10). A genuine hyphen there is lost with them.
    HyphenStripped,
}

/// A lyric whose clip text is not its SMF text: where it was and what changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Altered {
    /// The track the event was in.
    pub track: usize,
    /// Where it was.
    pub tick: Tick,
    /// The event.
    pub event: FileEvent,
    /// What the clip's text changes of it.
    pub change: LyricChange,
}

/// A Set Tempo or Time Signature that conversion moved onto the grid the
/// Clip File requires (M2-104 sections 7.5.3 and 7.5.4: a Set Tempo sits on a
/// 1/24-quarter-note position and a Set Time Signature on a bar boundary).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Moved {
    /// The track the event was in.
    pub track: usize,
    /// Where the SMF had it.
    pub from: Tick,
    /// Where the clip has it.
    pub to: Tick,
    /// The event.
    pub event: FileEvent,
}

/// The result of [`MetricalView::to_clip`]: the clip and what it left out.
///
/// A conversion that returned the clip alone would lose events without a word
/// and one that failed on the first would make a file with one marker
/// unconvertible. `dropped` lists each event the clip lacks, in the order the
/// clip's sequence would have had them (by tick, then track, then the track's
/// own order), so a caller can show it, or refuse a file that loses
/// something it cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "`dropped` lists the events the clip lacks"]
#[non_exhaustive]
pub struct Converted {
    /// The Clip File.
    pub clip: MidiFile,
    /// What has no UMP form.
    pub dropped: Vec<Dropped>,
    /// The tempo and time signature events that were off their grid and sit
    /// on the nearest grid position in the clip, in the order of `from`. An
    /// event that took the position of an earlier one of its kind is in
    /// `dropped` as [`DropReason::Superseded`] instead.
    pub moved: Vec<Moved>,
    /// The lyrics whose text the clip changes, by event and change, in the
    /// order of `tick`: a lyric is not lost, and what it says is not what the
    /// file spelled.
    pub altered: Vec<Altered>,
}

/// The distance in ticks between the positions of a grid of `numerator /
/// denominator` ticks that fall on a whole tick: the grid's step cannot
/// divide a tick, so the grid is every position that is one.
fn whole_tick_step(numerator: u64, denominator: u64) -> u64 {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    numerator / gcd(numerator, denominator)
}

/// The grid position nearest `tick` (a tie goes later), never before
/// `origin`, and the last position at or before `end` when the nearest is past
/// it. The arithmetic cannot overflow: a position past `u64::MAX` is past
/// `end`.
fn snap(tick: u64, origin: u64, step: u64, end: u64) -> u64 {
    let Some(past) = tick.checked_sub(origin).filter(|&d| d > 0) else {
        return origin;
    };
    let steps = past / step + u64::from(past % step >= step - step / 2);
    let at = steps
        .checked_mul(step)
        .and_then(|d| origin.checked_add(d))
        .filter(|&at| at <= end);
    at.unwrap_or_else(|| origin + end.saturating_sub(origin) / step * step)
}

/// Whether `event` is a Set Tempo with a tempo: not a zero, which has no Flex
/// form.
fn has_tempo(event: &FileEvent) -> bool {
    matches!(event, FileEvent::Meta(meta @ Meta::Tempo { .. }) if meta.bpm().is_some())
}

/// Whether a Time Signature converts: a whole-note denominator or a numerator
/// of 0 does not (see [`DropReason`]), so it has no bar grid either.
const fn has_bar_grid(numerator: u8, denominator_power: u8) -> bool {
    numerator != 0 && denominator_power != 0
}

/// The step in ticks between bar boundaries of a bar of `numerator` notes of
/// `2^power` per whole note: `4 * numerator * tpq / 2^power` ticks, reduced to
/// the whole-tick boundaries.
fn bar_step(ticks_per_quarter: TicksPerQuarterNote, numerator: u8, power: u8) -> u64 {
    let bar = 4 * u64::from(numerator) * u64::from(ticks_per_quarter.get());
    bar >> bar.trailing_zeros().min(u32::from(power))
}

/// Where in a tick's events a message goes: M2-116 sections 7.1.1 and 7.1.2
/// put the first Set Tempo, then the first Set Time Signature, at the Start
/// of Clip.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Tempo,
    Signature,
    Other,
}

pub(super) fn event(ump: Ump) -> FileEvent {
    match ump.decode() {
        Ok(message) => FileEvent::Message(message),
        Err(unknown) => FileEvent::Unknown(unknown),
    }
}

fn flex(flex: Flex) -> FileEvent {
    FileEvent::Message(Message::Flex(MidiGroup::FIRST, flex))
}

fn tempo(micros_per_quarter: MicrosPerQuarter) -> FileEvent {
    flex(Flex::SetTempo(FlexTempo::from_micros_per_quarter(
        micros_per_quarter.get(),
    )))
}

/// The four beats of the 4/4 bar a file with no time signature has.
const FOUR_BEATS: NonZeroU8 = NonZeroU8::new(4).unwrap();

fn signature(
    numerator: NonZeroU8,
    denominator: u8,
    thirty_seconds: u8,
    clocks_per_click: u8,
) -> [FileEvent; 2] {
    [
        flex(Flex::SetTimeSignature(FlexTimeSignature {
            numerator: numerator.get(),
            denominator,
            thirty_seconds,
        })),
        // SMF has the clocks per click only, so the bar is not divided:
        // An undivided bar has Bar Accent Part 1 equal to the beats in the
        // bar and Parts 2 and 3 zero, and a Subdivision Clicks value of 0
        // means none (M2-104 section 7.5.5).
        flex(Flex::SetMetronome(Metronome {
            clocks_per_click,
            accents: BarAccents::Undivided(numerator),
            subdivision_1: 0,
            subdivision_2: 0,
        })),
    ]
}

/// The key signature of `sharps` and `minor`: the sharps or flats, and the
/// tonic's letter (the field has no accidental: the sharps and flats say it).
///
/// A major key's tonic goes round the circle of fifths F C G D A E B as the
/// sharps rise from -1 (C at 0, G at 1); a minor key is the relative minor of
/// the major key 3 sharps higher (A minor, no sharps, shares its signature
/// with C major and the key 3 sharps up is A major), so its tonic is that
/// key's.
fn key_signature(sharps: i8, minor: bool) -> Option<KeySignature> {
    const CIRCLE: [Letter; 7] = [
        Letter::F,
        Letter::C,
        Letter::G,
        Letter::D,
        Letter::A,
        Letter::E,
        Letter::B,
    ];
    let sharps_flats = SharpsFlats::new(sharps)?;
    let major = i16::from(sharps) + if minor { 3 } else { 0 };
    Some(KeySignature {
        sharps_flats: Some(sharps_flats),
        tonic: Some(CIRCLE[(major + 1).rem_euclid(7) as usize]),
    })
}

/// The text of an SMF text event as message text, or why it has none.
///
/// Message text is NFC-normalized UTF-8 with no Byte Order Mark (M2-104
/// section 1.8): a leading BOM goes and the rest is
/// composed to NFC, which changes how the text is spelled, never what it says.
fn message_text(bytes: &[u8]) -> std::result::Result<String, DropReason> {
    if bytes.is_empty() {
        return Err(DropReason::EmptyText);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DropReason::TextNotUtf8)?;
    Ok(text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .nfc()
        .collect())
}

/// A text as its Flex packets, or why it has none.
fn text(kind: PlainTextKind, bytes: &[u8]) -> std::result::Result<Vec<FileEvent>, DropReason> {
    let text = Text::try_from(&*message_text(bytes)?).map_err(|e| DropReason::Text(e.into()))?;
    let packets = flex_text_packets(MidiGroup::FIRST, FlexAddress::Group, kind, &text);
    Ok(packets.map(event).collect())
}

/// What one of a track's [`Lead`]s becomes: its clip events, or the reason it
/// has none.
fn convert_lead(track: usize, lead: &Lead) -> std::result::Result<Vec<FileEvent>, DropReason> {
    match lead {
        // SMF 1.0 section 3.3: in a format 0 track, or the first track of a
        // format 1 file, the name is the sequence's, otherwise the track's. The
        // sequence is the Composition (Song) Name of Appendix B.
        Lead::TrackName(name) if track == 0 => text(PlainTextKind::CompositionName, name),
        Lead::TrackName(_) => Err(DropReason::TrackName),
        Lead::Copyright(notice) => text(PlainTextKind::Copyright, notice),
        Lead::SmpteOffset(_) => Err(DropReason::SuccessorUndefined),
        Lead::SequenceNumber(_) => Err(DropReason::NoSuccessor),
    }
}

/// What one SMF event becomes: its clip events (in order) and where among its
/// tick's they go, or the reason it has none.
fn convert(ev: &FileEvent) -> std::result::Result<(Rank, Vec<FileEvent>), DropReason> {
    let other = |events| Ok((Rank::Other, events));
    match ev {
        // Already a UMP: a MIDI 1.0 channel message on the first Group.
        FileEvent::Message(_) | FileEvent::Unknown(_) => other(vec![ev.clone()]),
        // A divided message's first packet is not one complete message.
        FileEvent::SysEx(SysEx::Start(_)) => Err(DropReason::SysexNotComplete),
        FileEvent::SysEx(SysEx::Complete(body)) => {
            let body: Vec<u8> = body.iter().map(|b| b.get()).collect();
            if is_property_exchange(&body) {
                return Err(DropReason::PropertyExchange);
            }
            let whole: Vec<u8> = std::iter::once(0xF0).chain(body).chain([0xF7]).collect();
            let message =
                SysexMessage::from_framed(&whole).map_err(|_| DropReason::SysexNotComplete)?;
            other(message.packets(MidiGroup::FIRST).map(event).collect())
        }
        FileEvent::Escape(_) => Err(DropReason::NoSuccessor),
        FileEvent::Meta(meta) => match meta {
            Meta::Tempo {
                micros_per_quarter, ..
            } if micros_per_quarter.get() == 0 => Err(DropReason::ZeroTempo),
            Meta::Tempo {
                micros_per_quarter, ..
            } => Ok((Rank::Tempo, vec![tempo(*micros_per_quarter)])),
            Meta::TimeSignature {
                numerator,
                denominator_power,
                clocks_per_click,
                thirty_seconds_per_quarter,
                ..
            } => {
                if *denominator_power == 0 {
                    return Err(DropReason::WholeNoteDenominator);
                }
                let numerator = NonZeroU8::new(*numerator).ok_or(DropReason::ZeroNumerator)?;
                Ok((
                    Rank::Signature,
                    signature(
                        numerator,
                        *denominator_power,
                        *thirty_seconds_per_quarter,
                        *clocks_per_click,
                    )
                    .to_vec(),
                ))
            }
            Meta::KeySignature { sharps, minor, .. } => {
                let key =
                    key_signature(*sharps, *minor).ok_or(DropReason::KeySignatureOutOfRange)?;
                other(vec![flex(Flex::SetKeySignature(FlexAddress::Group, key))])
            }
            Meta::Text { kind, bytes } => {
                let kind = match kind {
                    // Any text is what the Unknown Metadata Text message is
                    // for (M2-104 Table 16, bank 1 status 0).
                    TextKind::Text => PlainTextKind::UnknownMetadata,
                    // The rest have no successor. A lyric is read with the
                    // lyrics around it (see `Source`), so it is not here.
                    _ => return Err(DropReason::NoSuccessor),
                };
                other(text(kind, bytes)?)
            }
            Meta::Malformed(_) => Err(DropReason::MalformedMeta),
            Meta::ChannelPrefix { .. } => Err(DropReason::ReplacedByAddressing),
            Meta::SequencerSpecific { .. } => Err(DropReason::NoSuccessor),
            Meta::Other { kind, .. } => Err(match kind.get() {
                0x21 => DropReason::ReplacedByAddressing,
                _ => DropReason::NoSuccessor,
            }),
        },
    }
}

/// What an event of the SMF converts from: the event, or for a lyric the
/// clip events read with the lyrics around it.
enum Source {
    Event,
    Lyric(
        std::result::Result<Vec<FileEvent>, DropReason>,
        Vec<LyricChange>,
    ),
}

impl MetricalView<'_> {
    /// A Standard MIDI File of format 0 or 1 (the only formats with one
    /// timeline) as a MIDI 2.0 Clip File (M2-116), its meta events replaced by the UMP messages of Appendix B,
    /// with the events that have no replacement beside it.
    ///
    /// The mapping, each row from M2-116 section 3.3 and Appendix B (fields
    /// from M2-104 section 7.5):
    ///
    /// | SMF | clip |
    /// |---|---|
    /// | channel message | the same [`Message::Midi1`] packet, the first Group |
    /// | Set Tempo `FF 51`, us per quarter | Flex Set Tempo, `us * 100` (10 ns units per quarter) |
    /// | Time Signature `FF 58` | Flex Set Time Signature (numerator, denominator power, 32nds per quarter) then Flex Set Metronome (clocks per click; the bar undivided, its one accent part the numerator; no subdivisions) |
    /// | Key Signature `FF 59` | Flex Set Key Signature: the sharps or flats, and the tonic's letter from the circle of fifths (a minor key's from the major key 3 sharps up) |
    /// | Copyright `FF 02` | Flex Copyright Notice |
    /// | Lyric `FF 05` | Flex Lyrics, a syllable a message, karaoke marks as section 7.5.10 has them (below) |
    /// | Track Name `FF 03` of format 0, or the first track of format 1 | Flex Composition Name (SMF 1.0 section 3.3: it names the sequence) |
    /// | Text `FF 01` | Flex Unknown Metadata Text |
    /// | `F0` System Exclusive, complete and 7-bit | SysEx7 packets |
    /// | divisions per quarter | the DCTPQ, and ticks carry over |
    /// | End of Track | End of Clip, at the longest track's end |
    ///
    /// Texts are split over Flex packets by [`tutti_ump::flex_text_packets`].
    ///
    /// A lyric's text is read as karaoke text and written as Lyric Data
    /// (M2-104 section 7.5.10: a syllable a message, and a space, a carriage
    /// return or a carriage return and line feed at the end of the text for
    /// the end of a word, line or paragraph): a `/` at the start of an event
    /// is a line break and a `\` a paragraph break, a space at either end of
    /// the text a word break, and each ends the syllable before it; hyphens
    /// at the end of a syllable are dropped (a genuine one there is lost), as
    /// the syllable that follows it with no word end before it continues the
    /// word already (section 7.5.10: hyphens are for hyphenated words); an
    /// event of several words is one
    /// message each, all at its tick. A lyric of only breaks ends the syllable
    /// before it and is not listed in [`Converted::dropped`]; one of only
    /// hyphens is, as [`DropReason::LyricWithoutSyllable`]. A lyric whose
    /// break mark or hyphens were taken out of its text is in
    /// [`Converted::altered`]. These karaoke
    /// conventions are those of the SMF Lyric Meta Event (RP-026), which the
    /// MIDI specifications this crate is held to do not contain.
    ///
    /// Nothing else is converted: [`DropReason`] lists what has no successor.
    ///
    /// Format 1's tracks merge into the clip's one sequence by tick, ties in
    /// track order and then the track's own, as [`events`](MetricalView::events) does.
    /// Placement follows M2-116 sections 7.1.1 and 7.1.2: at tick 0 the Set
    /// Tempo messages come first and the Set Time Signature messages (each
    /// with its Set Metronome) next, then the rest; a file with no Set Tempo
    /// at tick 0 gets the SMF default, 120 bpm (SMF 1.0 section 3.3: without a
    /// tempo, 120 beats per minute is assumed), and one with no
    /// Time Signature 4/4, 24 clocks per click, 8 32nds per quarter, so the
    /// clip says what a SMF reader assumed; whether tick 0 has one is decided
    /// after the events are placed, so a tempo that moves to tick 0 is the
    /// clip's first and no default joins it. A later tempo or time signature
    /// stays on its own tick when that is on the grid. A Set Tempo sits on a
    /// 1/24-quarter-note position and a Set Time Signature on a bar boundary
    /// (M2-104 sections 7.5.3 and 7.5.4, required in a file; M2-116 sections
    /// 6.1.1, 6.1.2 and 7.1), so an event off its grid moves to the nearest
    /// position (a tie later; the last position at or before the clip's end
    /// if the nearest is past it) and is listed in [`Converted::moved`]. A bar
    /// is counted from the previous Time Signature (as moved), the first from
    /// tick 0 in 4/4, and a Time Signature never moves before it. When the
    /// division does not make a grid position of every 1/24 quarter or every
    /// bar line a whole tick, the grid is the positions that are.
    ///
    /// A clip holds one Set Tempo and one Set Time Signature at a position.
    /// When a tempo (or a Time Signature) lands on the position of the
    /// previous one of its kind, whether both moved there or both were at
    /// that tick, the later wins and the earlier is in
    /// [`Converted::dropped`] as [`DropReason::Superseded`].
    /// A Time Signature that replaces another counts its bars from the signature
    /// before the replaced one.
    ///
    /// Moving an event changes its order against the events that stay: a
    /// moved tempo or time signature can sit after a note it preceded in the
    /// file, or before one it followed, by less than the grid step. The notes
    /// keep their own ticks and their order among themselves.
    ///
    /// The [header](Track::header) is empty: the sequence data carries the
    /// tempo, which the spec's section 7.1.1 asks.
    ///
    /// # Errors
    ///
    /// [`ToClipError::AlreadyClip`] for a Clip File, [`ToClipError::TooLarge`]
    /// if the writer cannot hold the clip.
    #[must_use = "the Converted's `dropped` lists the events the clip lacks"]
    pub fn to_clip(self) -> Result<Converted, ToClipError> {
        let Self {
            file,
            ticks_per_quarter: tpq,
        } = self;
        if matches!(file.container, Container::Clip(_)) {
            return Err(ToClipError::AlreadyClip);
        }

        // Track order, then the track's own: the stable sort by tick keeps it.
        let mut all: Vec<(Tick, usize, &FileEvent)> = file
            .tracks()
            .iter()
            .enumerate()
            .flat_map(|(i, t)| t.events.iter().map(move |(tick, ev)| (*tick, i, ev)))
            .collect();
        all.sort_by_key(|&(tick, ..)| tick);

        let end = file
            .tracks()
            .iter()
            .map(|t| t.end)
            .max()
            .unwrap_or_default();
        let tempo_step = whole_tick_step(u64::from(tpq.get()), 24);

        // Where each event lands: tempos and time signatures on their grids,
        // in order. A bar grid starts at the last time signature's position
        // (4/4 from tick 0 until one), and a position never precedes it, so
        // the positions do not decrease. An event that lands where the last
        // of its kind did replaces it.
        struct Landing<'a> {
            track: usize,
            from: Tick,
            to: Tick,
            event: &'a FileEvent,
            superseded: bool,
        }
        let mut landings: Vec<Landing<'_>> = Vec::with_capacity(all.len());
        // The bar grids of the signatures that stand, the last being current.
        let four_four = (0, bar_step(tpq, 4, 2));
        let mut grids = vec![four_four];
        let (mut last_tempo, mut last_signature) = (None::<usize>, None::<usize>);
        for (from, track, event) in all {
            // A zero tempo is no tempo (a Flex field of 0 is none, M2-104
            // section 7.5.3): it is dropped and replaces no other.
            let is_tempo = has_tempo(event);
            let bar = match event {
                FileEvent::Meta(Meta::TimeSignature {
                    numerator,
                    denominator_power,
                    ..
                }) if has_bar_grid(*numerator, *denominator_power) => {
                    Some(bar_step(tpq, *numerator, *denominator_power))
                }
                _ => None,
            };
            let snapped = |grid: (u64, u64)| Tick(snap(from.0, grid.0, grid.1, end.0));
            let mut to = from;
            if is_tempo {
                to = snapped((0, tempo_step));
                if let Some(earlier) = last_tempo.filter(|&i| landings[i].to == to) {
                    landings[earlier].superseded = true;
                }
                last_tempo = Some(landings.len());
            } else if let Some(step) = bar {
                let current = grids.last().copied().unwrap_or(four_four);
                to = snapped(current);
                if let Some(earlier) = last_signature.filter(|&i| landings[i].to == to) {
                    // The earlier signature is replaced, so the later one
                    // counts its bars from the signature before it.
                    landings[earlier].superseded = true;
                    grids.pop();
                    to = snapped(grids.last().copied().unwrap_or(four_four));
                }
                last_signature = Some(landings.len());
                grids.push((to.0, step));
            }
            landings.push(Landing {
                track,
                from,
                to,
                event,
                superseded: false,
            });
        }

        // The defaults a SMF reader assumes, unless an event lands at tick 0.
        let live_at_start = |wanted: fn(&Meta) -> bool| {
            landings.iter().any(|l| {
                l.to == Tick(0)
                    && !l.superseded
                    && matches!(l.event, FileEvent::Meta(m) if wanted(m))
            })
        };
        let mut placed: Vec<(Tick, Rank, FileEvent)> = Vec::new();
        if !live_at_start(|m| m.bpm().is_some()) {
            placed.push((Tick(0), Rank::Tempo, tempo(MicrosPerQuarter::of(500_000))));
        }
        if !live_at_start(|m| matches!(m, Meta::TimeSignature { .. })) {
            for ev in signature(FOUR_BEATS, 2, 8, 24) {
                placed.push((Tick(0), Rank::Signature, ev));
            }
        }
        let mut lyrics = Lyrics::default();
        for (index, landing) in landings.iter().enumerate() {
            if let FileEvent::Meta(Meta::Text {
                kind: TextKind::Lyric,
                bytes,
            }) = landing.event
            {
                match message_text(bytes) {
                    Ok(text) => lyrics.push(index, &text),
                    Err(reason) => lyrics.refuse(index, reason),
                }
            }
        }
        let mut sources: Vec<Source> = landings.iter().map(|_| Source::Event).collect();
        for (index, events, changes) in lyrics.into_events() {
            sources[index] = Source::Lyric(events, changes);
        }
        let mut dropped = Vec::new();
        let mut moved = Vec::new();
        let mut altered = Vec::new();
        // The fields that open each track are at tick 0, ahead of its events.
        for (track, track_leads) in file.leads().into_iter().enumerate() {
            for lead in track_leads {
                match convert_lead(track, &lead) {
                    Ok(events) => {
                        placed.extend(events.into_iter().map(|e| (Tick(0), Rank::Other, e)))
                    }
                    Err(reason) => dropped.push(Dropped {
                        track,
                        tick: Tick(0),
                        part: SmfPart::Lead(lead),
                        reason,
                    }),
                }
            }
        }
        for (
            Landing {
                track,
                from,
                to,
                event,
                superseded,
            },
            source,
        ) in landings.into_iter().zip(sources)
        {
            let mut changes = Vec::new();
            let converted = if superseded {
                Err(DropReason::Superseded)
            } else {
                match source {
                    Source::Event => convert(event),
                    Source::Lyric(events, made) => {
                        changes = made;
                        events.map(|events| (Rank::Other, events))
                    }
                }
            };
            match converted {
                Ok((rank, events)) => {
                    if to != from {
                        moved.push(Moved {
                            track,
                            from,
                            to,
                            event: event.clone(),
                        });
                    }
                    altered.extend(changes.into_iter().map(|change| Altered {
                        track,
                        tick: from,
                        event: event.clone(),
                        change,
                    }));
                    // Only tick 0 is ordered by rank; a later tempo is where it was.
                    let rank = if to == Tick(0) { rank } else { Rank::Other };
                    placed.extend(events.into_iter().map(|e| (to, rank, e)));
                }
                Err(reason) => dropped.push(Dropped {
                    track,
                    tick: from,
                    part: SmfPart::Event(event.clone()),
                    reason,
                }),
            }
        }
        // The fields of a track are dropped before its events, so they sort
        // into the documented order (by tick, then track, then the track's own).
        dropped.sort_by_key(|d| (d.tick, d.track));
        // Stable: within a tick and rank, the merge order.
        placed.sort_by_key(|&(tick, rank, _)| (tick, rank));

        let sequence = Track::from_read(
            Vec::new(),
            Tick(0),
            placed.into_iter().map(|(tick, _, e)| (tick, e)).collect(),
            end,
        );
        // The sequence is ordered, holds packets only and has no header, so
        // the one rule it can break is the writer's size.
        let clip = MidiFile::clip(tpq, sequence).map_err(|_| ToClipError::TooLarge)?;
        Ok(Converted {
            clip,
            dropped,
            moved,
            altered,
        })
    }
}

#[cfg(test)]
mod tests;
