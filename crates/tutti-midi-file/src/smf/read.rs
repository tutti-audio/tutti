//! The reader: SMF bytes into [`SmfParts`], built from `winnow` combinators.
//!
//! # Shape
//!
//! Every parser is `fn(&mut &[u8]) -> Res<T>` (or a function returning one),
//! composed with `?` and winnow's combinators; there is no cursor type and no
//! index arithmetic. Two passes, in this order:
//!
//! 1. **Framing** ([`chunk`], [`smf`]): the header chunk, then chunks of
//!    `type, length, body` until the input ends. A chunk cut off anywhere is
//!    [`SmfParseError::ChunkCutOff`]; an unknown chunk type is skipped (SMF 1.0 section
//!    2: a program expects alien chunks and treats them as absent).
//! 2. **Tracks** ([`track`]): each `MTrk` body into events.
//!
//! # Running status is a value
//!
//! [`event`] takes the running status and returns the next one beside the
//! event; [`steps`] threads it with `successors`. Nothing hides the status in
//! the input type, so one event is a function of its bytes and the status
//! before it. SMF 1.0 section 3 (Track Chunks): a channel event may omit its
//! status byte when it equals the previous event's, and a sysex or meta event
//! cancels the running status. Real-time and system-common statuses are not in
//! a file at all (section 3.4: `F1` to `F6` and `F8` to `FE` are undefined
//! there), so they are refused.
//!
//! # What decodes a message
//!
//! The combinators frame the file; a channel message is `tutti-ump`'s: its
//! data-byte count is `Voice1::data_len` and the status and data bytes become
//! a message through `parse_exact`, the one-message decode. Track
//! bytes are never fed to `Midi1Parser::feed` as a stream: in a file the delta
//! bytes would read as running-status data and `FF` (meta) as a real-time
//! Reset, so the streaming parser is wrong for file framing.
//!
//! # What this reads strictly
//!
//! A chunk that runs past the end, fewer tracks than the header declares
//! ([`SmfParseError::TracksCutOff`]); more tracks than declared, a format 0 file without
//! exactly one track, a second header chunk, a data byte with its top bit set,
//! a running status with none active, a variable-length quantity over four
//! bytes, an event cut off inside its chunk, a zero division, and an SMPTE
//! division with a frame rate of none of the four or no resolution
//! ([`SmfParseError`]). A track with no End of Track ends at its last event, and an End of Track that is
//! not the track's last event is refused (SMF 1.0 section 3.3: it is the last
//! event in the track).

use std::iter::successors;

use winnow::Parser;
use winnow::binary::{be_u16, be_u32, length_take, u8 as byte};
use winnow::combinator::{alt, dispatch, eof, fail, peek, repeat};
use winnow::error::ContextError;
use winnow::token::{literal, take};

use tutti_ump::{ManufacturerId, MidiChannel, MidiGroup, U7, Voice1, parse_exact};

use super::{SmfDivision, SmfParts};
use crate::file::split_extra;
use crate::file::{
    FileEvent, Lead, MalformedMeta, Meta, MicrosPerQuarter, Normalized, OtherMetaKind, Placement,
    SmfFormat, SmpteOffset, SysEx, TextKind, Tick, Track,
};
use crate::{ReadError, SmfParseError};

type Res<T> = std::result::Result<T, ContextError<SmfParseError>>;

/// The running status: the last channel status byte, if no sysex or meta
/// event has cancelled it.
type Status = Option<u8>;

/// A parser that always refuses, for `what`.
fn refuse<'a, T>(what: SmfParseError) -> impl Parser<&'a [u8], T, ContextError<SmfParseError>> {
    fail.context(what)
}

// ---- primitives ------------------------------------------------------------

/// A variable-length quantity (SMF 1.0 section 1.1): seven bits a byte,
/// most significant first, the top bit set on every byte but the last, and at
/// most four bytes (28 bits).
fn vlq(input: &mut &[u8]) -> Res<u32> {
    (
        repeat(0..=3, byte.verify(|b| b & 0x80 != 0))
            .fold(|| 0u32, |acc, b| acc << 7 | u32::from(b & 0x7F)),
        byte.verify(|b| b & 0x80 == 0),
    )
        .map(|(high, last)| high << 7 | u32::from(last))
        .context(SmfParseError::VariableLengthQuantity)
        .parse_next(input)
}

/// A length (a `u32` or a vlq) and that many bytes.
fn counted<'a>(
    length: impl Parser<&'a [u8], u32, ContextError<SmfParseError>>,
) -> impl Parser<&'a [u8], &'a [u8], ContextError<SmfParseError>> {
    length_take(length.map(|n| n as usize))
}

// ---- framing ---------------------------------------------------------------

/// What the header chunk says.
struct Header {
    format: SmfFormat,
    tracks: u16,
    division: SmfDivision,
}

/// The division word: ticks per quarter note, or (top bit set) an SMPTE
/// frame rate and ticks per frame (SMF 1.0 section 2.1).
fn division(input: &mut &[u8]) -> Res<SmfDivision> {
    let word = be_u16.parse_next(input)?;
    match SmfDivision::from_word(word) {
        Ok(division) => Ok(division),
        Err(cause) => refuse(cause).parse_next(input),
    }
}

/// The header chunk's body: format, track count, division. Bytes after the
/// first six are ignored (section 2.1: a reader skips what a later revision
/// adds).
fn header_body(input: &mut &[u8]) -> Res<Header> {
    let format = be_u16.map(SmfFormat::from_word).parse_next(input)?;
    let tracks = be_u16.parse_next(input)?;
    let division = division.parse_next(input)?;
    Ok(Header {
        format,
        tracks,
        division,
    })
}

/// The header chunk: `MThd`, a length of at least six, the body.
fn header(input: &mut &[u8]) -> Res<Header> {
    literal(b"MThd")
        .context(SmfParseError::MissingHeader)
        .parse_next(input)?;
    let mut body = counted(be_u32)
        .context(SmfParseError::ChunkCutOff)
        .parse_next(input)?;
    header_body
        .context(SmfParseError::ShortHeader)
        .parse_next(&mut body)
}

/// A chunk after the header: a track's body to decode later, or one skipped.
enum Chunk<'a> {
    Track(&'a [u8]),
    Alien,
}

/// `type, length, body`, framed and not yet read.
fn chunk<'a>(input: &mut &'a [u8]) -> Res<Chunk<'a>> {
    let (kind, body) = (take(4usize), counted(be_u32))
        .context(SmfParseError::ChunkCutOff)
        .parse_next(input)?;
    match kind {
        b"MTrk" => Ok(Chunk::Track(body)),
        b"MThd" => refuse(SmfParseError::SecondHeader).parse_next(input),
        _ => Ok(Chunk::Alien),
    }
}

/// The whole file, framed: the header and the offset and body of each `MTrk`
/// chunk.
fn smf(bytes: &[u8]) -> Result<(Header, Vec<(usize, &[u8])>), ReadError> {
    let mut input = bytes;
    let header = header
        .parse_next(&mut input)
        .map_err(|e| refused(None, 0, &faults(&e)))?;
    let mut tracks = Vec::new();
    while !input.is_empty() {
        let at = bytes.len() - input.len();
        match chunk.parse_next(&mut input) {
            Ok(Chunk::Track(body)) => tracks.push((at, body)),
            Ok(Chunk::Alien) => {}
            Err(e) => return Err(refused(None, at, &faults(&e))),
        }
    }
    Ok((header, tracks))
}

// ---- events ----------------------------------------------------------------

/// The data bytes of a channel status (how many is `Voice1::data_len`'s) as a
/// message on the first Group. `parse_exact` owns the status bit layout.
fn channel<'a>(status: u8) -> impl Parser<&'a [u8], FileEvent, ContextError<SmfParseError>> {
    let count = Voice1::data_len(status).map_or(0, usize::from);
    take(count)
        .verify_map(move |data: &[u8]| {
            let bytes = match *data {
                [a] => [status, a, 0],
                [a, b] => [status, a, b],
                _ => return None,
            };
            parse_exact(MidiGroup::FIRST, &bytes).map(FileEvent::Message)
        })
        .context(SmfParseError::ChannelData)
}

/// What an event of a track is: an event of the model, one of the meta events
/// that open a track (a [`Lead`], which the model keeps as a field), or the End
/// of Track that ends the track.
#[derive(Clone)]
enum Item {
    Event(FileEvent),
    Lead(Lead),
    EndOfTrack,
}

/// A meta event's type byte and data as the model's [`Meta`] or [`Lead`], or the track's
/// end. A fixed-size event with more data than it defines is read for the
/// bytes it defines and keeps the rest as `extra` (SMF 1.0 section 3.3: a
/// program ignores what is past what it knows). An event of a typed kind (End
/// of Track, tempo, time and key signature) whose data is shorter than the
/// kind defines or has an undefined value, or whose type byte has its top bit
/// set, is kept as [`Meta::Malformed`]: [`Meta::Other`] holds no typed kind.
fn meta_of(kind: u8, data: &[u8]) -> Item {
    let text = |kind| Meta::Text {
        kind,
        bytes: data.into(),
    };
    let meta = match (kind, data) {
        (0x2F, []) => return Item::EndOfTrack,
        (0x51, &[a, b, c, ref extra @ ..]) => Meta::Tempo {
            micros_per_quarter: MicrosPerQuarter::from_wire([a, b, c]),
            extra: extra.into(),
        },
        (
            0x58,
            &[
                numerator,
                denominator_power,
                clocks_per_click,
                thirty_seconds_per_quarter,
                ref extra @ ..,
            ],
        ) => Meta::TimeSignature {
            numerator,
            denominator_power,
            clocks_per_click,
            thirty_seconds_per_quarter,
            extra: extra.into(),
        },
        (0x59, &[sharps, minor @ (0 | 1), ref extra @ ..]) => Meta::KeySignature {
            sharps: i8::from_be_bytes([sharps]),
            minor: minor == 1,
            extra: extra.into(),
        },
        (0x02, _) => return Item::Lead(Lead::Copyright(data.into())),
        (0x03, _) => return Item::Lead(Lead::TrackName(data.into())),
        (0x00, &[a, b, ..]) => return Item::Lead(Lead::SequenceNumber(u16::from_be_bytes([a, b]))),
        (0x54, _) => {
            if let Some((wire, _)) = split_extra(data)
                && let Some(offset) = SmpteOffset::from_wire(wire)
            {
                return Item::Lead(Lead::SmpteOffset(offset));
            }
            Meta::Malformed(MalformedMeta::new(kind, data))
        }
        _ => match (TextKind::from_meta_type(kind), OtherMetaKind::new(kind)) {
            (Some(text_kind), _) => text(text_kind),
            (None, Some(kind)) => Meta::Other {
                kind,
                data: data.into(),
            },
            (None, None) => fixed_or_id_meta(kind, data)
                .unwrap_or(Meta::Malformed(MalformedMeta::new(kind, data))),
        },
    };
    Item::Event(FileEvent::Meta(meta))
}

/// The MIDI Channel Prefix or Sequencer-Specific event of `kind` and `data`:
/// `None` for another kind, or for data the kind does not define (a channel
/// past 15, no manufacturer id).
fn fixed_or_id_meta(kind: u8, data: &[u8]) -> Option<Meta> {
    match kind {
        0x20 => split_extra(data).and_then(|([channel], extra)| {
            MidiChannel::new(channel).map(|channel| Meta::ChannelPrefix { channel, extra })
        }),
        0x7F => {
            // One id byte, or a `00` and two.
            let id_len = if data.first() == Some(&0) { 3 } else { 1 };
            let (id, rest) = data.split_at_checked(id_len)?;
            let mut bytes = [0u8; 3];
            bytes[..id_len].copy_from_slice(id);
            let [Some(a), Some(b), Some(c)] = bytes.map(U7::new) else {
                return None;
            };
            ManufacturerId::from_sysex_bytes([a, b, c]).map(|manufacturer| {
                Meta::SequencerSpecific {
                    manufacturer,
                    data: rest.into(),
                }
            })
        }
        _ => None,
    }
}

/// A meta event after its `FF`: type, length, data.
fn meta(input: &mut &[u8]) -> Res<Item> {
    (byte, counted(vlq))
        .map(|(kind, data)| meta_of(kind, data))
        .parse_next(input)
}

/// The `F0` event's data as the model's message: one that ends in `F7` is
/// whole, whose `F7` the model leaves to the writer, and one that does not is
/// the first packet of a divided message.
fn sysex(data: &[u8]) -> SysEx {
    match data.split_last() {
        Some((0xF7, whole)) => SysEx::Complete(whole.into()),
        _ => SysEx::Start(data.into()),
    }
}

/// One event after its delta, given the running status: the event and the
/// status after it. A status byte is read when the next byte has its top bit
/// set; otherwise the running one stands in and the byte is data.
fn event<'a>(
    running: Status,
) -> impl Parser<&'a [u8], (u32, Item, Status), ContextError<SmfParseError>> {
    let status = alt((
        byte.verify(|b| b & 0x80 != 0),
        peek(byte.verify(|b| b & 0x80 == 0)).verify_map(move |_| running),
    ))
    .context(SmfParseError::NoRunningStatus);
    (vlq, status).flat_map(|(delta, status)| {
        dispatch! { move |_: &mut &'a [u8]| Ok::<u8, ContextError<SmfParseError>>(status);
            0xFF => meta.map(|e| (e, None)),
            0xF0 => counted(vlq).map(|d| (Item::Event(FileEvent::SysEx(sysex(d))), None)),
            0xF7 => counted(vlq).map(|d| (Item::Event(FileEvent::Escape(d.into())), None)),
            0x80..=0xEF => channel(status).map(move |e| (Item::Event(e), Some(status))),
            _ => refuse(SmfParseError::SystemStatus),
        }
        .map(move |(event, status)| (delta, event, status))
    })
}

/// One step of a track: the event at `delta`, where it starts, the bytes
/// after it, and the status after it. `None` at the end of the bytes.
struct Step<'a> {
    at: usize,
    delta: u32,
    event: Item,
    rest: &'a [u8],
    status: Status,
}

/// A refusal inside a track body: where the event that was refused starts, and
/// the faults that refused it.
struct Failure {
    at: usize,
    faults: Vec<SmfParseError>,
}

fn step<'a>(body: &'a [u8], input: &'a [u8], running: Status) -> Result<Option<Step<'a>>, Failure> {
    let at = body.len() - input.len();
    alt((eof.value(None), event(running).map(Some)))
        .parse_peek(input)
        .map(|(rest, parsed)| {
            parsed.map(|(delta, event, status)| Step {
                at,
                delta,
                event,
                rest,
                status,
            })
        })
        .map_err(|e| Failure {
            at,
            faults: faults(&e),
        })
}

/// Every step of a track body, an error ending the sequence.
fn steps(body: &[u8]) -> impl Iterator<Item = Result<Step<'_>, Failure>> {
    successors(Some(step(body, body, None)), |prev| match prev {
        Ok(Some(s)) => Some(step(body, s.rest, s.status)),
        _ => None,
    })
    .map_while(std::result::Result::transpose)
}

/// Settles a track's `lead` at `tick` into its field, and answers what the file
/// had wrong: the event with its [`Placement`] when it is not where SMF 1.0
/// section 3.3 puts it. `leading` says the track has had only meta events at
/// tick 0 so far.
fn settle(track: &mut Track, lead: Lead, tick: Tick, leading: bool) -> Option<(Lead, Placement)> {
    // A Sequence Number and an SMPTE Offset are at the very start; a name and
    // a notice only at time 0.
    let at_start_only = matches!(lead, Lead::SequenceNumber(_) | Lead::SmpteOffset(_));
    if tick != Tick(0) || (at_start_only && !leading) {
        return Some((lead, Placement::Late));
    }
    let taken = match lead {
        Lead::SequenceNumber(_) => track.number.is_some(),
        Lead::TrackName(_) => track.name.is_some(),
        Lead::SmpteOffset(_) => track.smpte_start.is_some(),
        Lead::Copyright(_) => track.copyright.is_some(),
    };
    if taken {
        return Some((lead, Placement::Duplicate));
    }
    let moved = (!at_start_only && !leading).then(|| (lead.clone(), Placement::Moved));
    match lead {
        Lead::SequenceNumber(number) => track.number = Some(number),
        Lead::TrackName(name) => track.name = Some(name),
        Lead::SmpteOffset(offset) => track.smpte_start = Some(offset),
        Lead::Copyright(notice) => track.copyright = Some(notice),
    }
    moved
}

/// Ends the divided System Exclusive messages of `events` that are interrupted
/// by a channel message or another `F0` event, or never end in an `F7` packet
/// (SMF 1.0 section 3): the first packet becomes a whole message.
fn close_sysex(track: usize, events: &mut [(Tick, FileEvent)], changes: &mut Vec<Normalized>) {
    let mut close = |events: &mut [(Tick, FileEvent)], index: usize| {
        let (at, event) = &mut events[index];
        if let FileEvent::SysEx(SysEx::Start(data)) = event {
            *event = FileEvent::SysEx(SysEx::Complete(std::mem::take(data)));
            changes.push(Normalized::SysExClosed { track, at: *at });
        }
    };
    let mut open: Option<usize> = None;
    for index in 0..events.len() {
        match &events[index].1 {
            FileEvent::SysEx(_) | FileEvent::Message(_) => {
                if let Some(first) = open.take() {
                    close(events, first);
                }
                if matches!(events[index].1, FileEvent::SysEx(SysEx::Start(_))) {
                    open = Some(index);
                }
            }
            FileEvent::Escape(data) if data.last() == Some(&0xF7) => open = None,
            _ => {}
        }
    }
    if let Some(first) = open {
        close(events, first);
    }
}

/// A track body: its events at absolute ticks, its end, and what was changed
/// to make it writable (`index` is the track's). The last event, if it is an
/// End of Track (`FF 2F 00`), is the track's end and not an event.
fn track(index: usize, body: &[u8]) -> Result<(Track, Vec<Normalized>), Failure> {
    let items = steps(body)
        .scan(0u64, |tick, step| {
            Some(step.map(|s| {
                *tick += u64::from(s.delta);
                (Tick(*tick), s.event, s.at)
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // SMF 1.0 section 3.3: an end-of-track event is the last
    // event in the track. One before the last is refused, not read as the
    // track's end: what follows it would be dropped, and the model is lossless.
    let count = items.len();
    let mut events = Vec::with_capacity(count);
    let mut leads = Track::new();
    let mut changes = Vec::new();
    // Only meta events, all at tick 0, so far.
    let mut leading = true;
    let mut end = None;
    for (position, (tick, item, at)) in items.into_iter().enumerate() {
        match item {
            Item::Event(event) => {
                leading &= tick == Tick(0) && matches!(event, FileEvent::Meta(_));
                events.push((tick, event));
            }
            Item::Lead(lead) => {
                changes.extend(
                    settle(&mut leads, lead, tick, leading).map(|(lead, placement)| {
                        Normalized::Lead {
                            track: index,
                            at: tick,
                            lead,
                            placement,
                        }
                    }),
                );
                leading &= tick == Tick(0);
            }
            Item::EndOfTrack if position + 1 == count => end = Some(tick),
            Item::EndOfTrack => {
                return Err(Failure {
                    at,
                    faults: vec![SmfParseError::EndOfTrackNotLast],
                });
            }
        }
    }
    close_sysex(index, &mut events, &mut changes);
    // A track with no End of Track ends at its last event.
    let end = end
        .or_else(|| events.last().map(|&(tick, _)| tick))
        .unwrap_or(Tick(0));
    let mut track = Track::from_read(Vec::new(), Tick(0), events, end);
    (track.number, track.name, track.smpte_start, track.copyright) =
        (leads.number, leads.name, leads.smpte_start, leads.copyright);
    Ok((track, changes))
}

// ---- the edge --------------------------------------------------------------

/// The faults a parser refused with, innermost first.
fn faults(e: &ContextError<SmfParseError>) -> Vec<SmfParseError> {
    e.context().copied().collect()
}

/// The one place winnow's error becomes ours: the error of the refusal
/// `faults` at byte `offset`. A refusal that names no rule ran out of bytes
/// inside an event.
fn refused(track: Option<usize>, offset: usize, faults: &[SmfParseError]) -> ReadError {
    ReadError::Smf {
        track,
        offset,
        cause: faults
            .first()
            .copied()
            .unwrap_or(SmfParseError::EventCutOff),
    }
}

/// Parses SMF bytes.
///
/// # Errors
///
/// - [`ReadError::Smf`] for anything malformed or cut off, with where.
pub(crate) fn read(bytes: &[u8]) -> Result<SmfParts, ReadError> {
    let (header, chunks) = smf(bytes)?;
    let declared = usize::from(header.tracks);
    // Fewer than declared is a cut file; more, or a format 0 with other
    // than one, disagrees with its own header.
    let smf_error = |offset, cause| ReadError::Smf {
        track: None,
        offset,
        cause,
    };
    if chunks.len() < declared {
        return Err(smf_error(bytes.len(), SmfParseError::TracksCutOff));
    }
    if chunks.len() > declared {
        return Err(smf_error(chunks[declared].0, SmfParseError::ExtraTracks));
    }
    if header.format == SmfFormat::SingleTrack && declared != 1 {
        return Err(smf_error(0, SmfParseError::FormatZeroTracks));
    }
    let (mut tracks, mut normalized) = (Vec::new(), Vec::new());
    for (index, &(at, body)) in chunks.iter().enumerate() {
        // The body starts after the chunk's type and length.
        let (read, changes) =
            track(index, body).map_err(|f| refused(Some(index), at + 8 + f.at, &f.faults))?;
        tracks.push(read);
        normalized.extend(changes);
    }
    // SMF 1.0 section 3.3: a format 0 or 1 file's one sequence is numbered in
    // its first track, a format 1 file's SMPTE Offset is stored with its tempo
    // map, and a copyright notice is in the first track.
    for (index, track) in tracks.iter_mut().enumerate().skip(1) {
        let mut outside = |lead: Option<Lead>| {
            normalized.extend(lead.map(|lead| Normalized::Lead {
                track: index,
                at: Tick(0),
                lead,
                placement: Placement::NotInFirstTrack,
            }));
        };
        if header.format.shares_timeline() {
            outside(track.number.take().map(Lead::SequenceNumber));
        }
        if header.format == SmfFormat::Parallel {
            outside(track.smpte_start.take().map(Lead::SmpteOffset));
        }
        outside(track.copyright.take().map(Lead::Copyright));
    }
    Ok(SmfParts {
        format: header.format,
        division: header.division,
        tracks,
        normalized,
    })
}
