//! The writer: the reader's inverse, a fold of pure functions.
//!
//! [`measure`] is the arithmetic of what an SMF's length fields hold, run when
//! a file is built; [`write()`] is then infallible. An event is a pure function
//! to its bytes ([`encode`]); a track body is a fold of events over the
//! running status ([`body`]), which elides a channel status byte equal to the
//! one before it (SMF 1.0 section 3: running status; sysex and meta cancel
//! it); a chunk's length is computed from the body it frames ([`chunk`]),
//! never patched in afterwards.

use tutti_ump::{Message, Midi1Bytes};

use super::SmfDivision;
use crate::TracksError;
use crate::file::{FileEvent, Meta, SmfFormat, Tick, Track};

/// The widest value a variable-length quantity holds: 28 bits.
const VLQ_MAX: u64 = 0x0FFF_FFFF;

/// The bytes of the variable-length quantity of `n` (SMF 1.0 section 1.1).
fn vlq_len(n: u64) -> usize {
    1 + usize::from(n >= 1 << 7) + usize::from(n >= 1 << 14) + usize::from(n >= 1 << 21)
}

/// The variable-length quantity of `n`: seven bits a byte, most significant
/// first, the top bit set on all but the last. Only the low 28 bits are
/// written: [`measure`] and the constructor's checks have refused a wider
/// value.
fn vlq(n: u64) -> impl Iterator<Item = u8> {
    let n = n & VLQ_MAX;
    (1..4u32)
        .rev()
        .filter(move |i| n >> (7 * i) != 0)
        .chain(std::iter::once(0))
        .map(move |i| (n >> (7 * i)) as u8 & 0x7F | if i > 0 { 0x80 } else { 0 })
}

/// An event's bytes, status byte first.
enum Encoded {
    /// A channel message: the only kind running status applies to.
    Channel(Midi1Bytes),
    /// Sysex, escape or meta.
    Other(Vec<u8>),
}

impl Encoded {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Channel(b) => b.as_slice(),
            Self::Other(v) => v,
        }
    }
}

/// `lead`, the length of `data` as a vlq, then `data`.
fn counted(lead: &[u8], data: &[u8]) -> Vec<u8> {
    lead.iter()
        .copied()
        .chain(vlq(data.len() as u64))
        .chain(data.iter().copied())
        .collect()
}

/// A meta event's type byte and data.
fn meta_parts(meta: &Meta) -> (u8, Vec<u8>) {
    match meta {
        Meta::Text { kind, bytes } => (kind.meta_type(), bytes.to_vec()),
        Meta::Tempo {
            micros_per_quarter,
            extra,
        } => (0x51, [&micros_per_quarter.to_wire()[..], extra].concat()),
        Meta::TimeSignature {
            numerator,
            denominator_power,
            clocks_per_click,
            thirty_seconds_per_quarter,
            extra,
        } => (
            0x58,
            [
                &[
                    *numerator,
                    *denominator_power,
                    *clocks_per_click,
                    *thirty_seconds_per_quarter,
                ][..],
                extra,
            ]
            .concat(),
        ),
        Meta::KeySignature {
            sharps,
            minor,
            extra,
        } => (
            0x59,
            [&[sharps.cast_unsigned(), u8::from(*minor)][..], extra].concat(),
        ),
        Meta::SequenceNumber { number, extra } => {
            (0x00, [&number.to_be_bytes()[..], extra].concat())
        }
        Meta::ChannelPrefix { channel, extra } => (0x20, [&[channel.get()][..], extra].concat()),
        Meta::SmpteOffset { offset, extra } => (0x54, [&offset.to_wire()[..], extra].concat()),
        Meta::SequencerSpecific { manufacturer, data } => {
            let id = manufacturer.get();
            let [_, hi, lo] = manufacturer.sysex_bytes().map(tutti_ump::U7::get);
            let lead: &[u8] = if manufacturer.is_extended() {
                &[0, hi, lo]
            } else {
                &[id as u8]
            };
            (0x7F, [lead, data].concat())
        }
        Meta::Other { kind, data } => (kind.get(), data.to_vec()),
        Meta::Malformed(malformed) => (malformed.kind(), malformed.data().to_vec()),
    }
}

/// The length of a meta event's data: that of [`meta_parts`], without making it.
fn meta_data_len(meta: &Meta) -> usize {
    match meta {
        Meta::Text { bytes, .. } => bytes.len(),
        Meta::Tempo { extra, .. } => 3 + extra.len(),
        Meta::TimeSignature { extra, .. } => 4 + extra.len(),
        Meta::KeySignature { extra, .. } => 2 + extra.len(),
        Meta::SequenceNumber { extra, .. } => 2 + extra.len(),
        Meta::ChannelPrefix { extra, .. } => 1 + extra.len(),
        Meta::SmpteOffset { extra, .. } => 5 + extra.len(),
        Meta::SequencerSpecific { manufacturer, data } => {
            data.len() + if manufacturer.is_extended() { 3 } else { 1 }
        }
        Meta::Other { data, .. } => data.len(),
        Meta::Malformed(malformed) => malformed.data().len(),
    }
}

/// The bytes of an event of `lead` bytes and `len` bytes of data under their
/// vlq length, `TooLarge` past the vlq.
fn counted_len(lead: usize, len: usize) -> Result<usize, TracksError> {
    if len as u64 > VLQ_MAX {
        return Err(TracksError::TooLarge);
    }
    Ok(lead + vlq_len(len as u64) + len)
}

/// One event as bytes. `None` for what an SMF cannot hold (a packet other than
/// MIDI 1.0 channel voice): the constructor has refused it.
fn encode(event: &FileEvent) -> Option<Encoded> {
    match event {
        FileEvent::Message(Message::Midi1(address, voice)) => {
            Some(Encoded::Channel(voice.bytes(address.channel)))
        }
        FileEvent::SysEx(data) => Some(Encoded::Other(counted(&[0xF0], data))),
        FileEvent::Escape(data) => Some(Encoded::Other(counted(&[0xF7], data))),
        FileEvent::Meta(meta) => {
            let (kind, data) = meta_parts(meta);
            Some(Encoded::Other(counted(&[0xFF, kind], &data)))
        }
        FileEvent::Message(_) | FileEvent::Unknown(_) => None,
    }
}

/// A track body's fold state: the bytes so far, the running status, and the
/// tick of the last event.
type Fold = (Vec<u8>, Option<u8>, Tick);

/// The delta from `from` to `to` as vlq bytes.
fn delta(from: Tick, to: Tick) -> impl Iterator<Item = u8> {
    vlq(to.0.saturating_sub(from.0))
}

/// Folds one event onto the body: its delta, then its bytes without a status
/// byte the running status already holds.
fn append((mut out, running, at): Fold, (tick, event): &(Tick, FileEvent)) -> Fold {
    let Some(encoded) = encode(event) else {
        return (out, running, at);
    };
    let Some((&status, data)) = encoded.bytes().split_first() else {
        return (out, running, at);
    };
    let is_channel = matches!(encoded, Encoded::Channel(_));
    out.extend(delta(at, *tick));
    out.extend(
        (!(is_channel && running == Some(status)))
            .then_some(status)
            .into_iter()
            .chain(data.iter().copied()),
    );
    (out, is_channel.then_some(status), *tick)
}

/// A track's body: its events, then End of Track at its `end`.
fn body(track: &Track) -> Vec<u8> {
    let (out, _, at) = track
        .events
        .iter()
        .fold((Vec::new(), None, Tick(0)), append);
    out.into_iter()
        .chain(delta(at, track.end))
        .chain([0xFF, 0x2F, 0x00])
        .collect()
}

/// A chunk: its type, the length of `body`, the body. [`measure`] has checked
/// that the length fits.
fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    kind.iter()
        .copied()
        .chain(len.to_be_bytes())
        .chain(body.iter().copied())
        .collect()
}

/// Whether `tracks` fit an SMF's length fields, by arithmetic over their
/// events and no allocation: every event's data is at most 28 bits long, and
/// every track chunk is at most 32 bits long.
///
/// # Errors
///
/// [`TracksError::TooLarge`] for an event or a chunk past its length field. A
/// caller has checked the rest ([`MidiFile`](crate::MidiFile)'s constructor):
/// events in order, a delta that fits the wire's 28 bits, only group-0 MIDI
/// 1.0 messages, at most 65535 tracks.
pub(crate) fn measure(tracks: &[Track]) -> Result<(), TracksError> {
    for track in tracks {
        let (mut at, mut running) = (Tick(0), None);
        let mut total = 0u64;
        for (tick, event) in &track.events {
            total += vlq_len(tick.0.saturating_sub(at.0)) as u64;
            at = *tick;
            let bytes = match event {
                FileEvent::Message(Message::Midi1(address, voice)) => {
                    let bytes = voice.bytes(address.channel);
                    let status = bytes.as_slice().first().copied();
                    let elided = usize::from(status.is_some() && status == running);
                    running = status;
                    bytes.as_slice().len() - elided
                }
                FileEvent::SysEx(data) | FileEvent::Escape(data) => {
                    running = None;
                    counted_len(1, data.len())?
                }
                FileEvent::Meta(meta) => {
                    running = None;
                    counted_len(2, meta_data_len(meta))?
                }
                FileEvent::Message(_) | FileEvent::Unknown(_) => 0,
            };
            total += bytes as u64;
        }
        // The End of Track: its delta, `FF 2F 00`.
        total += (vlq_len(track.end.0.saturating_sub(at.0)) + 3) as u64;
        if total > u64::from(u32::MAX) {
            return Err(TracksError::TooLarge);
        }
    }
    Ok(())
}

/// Writes `tracks` as an SMF of `format` and `division`.
///
/// Each track ends with an End of Track at its `end`. A caller has checked the
/// tracks with [`measure`] and the constructor's rules.
pub(crate) fn write(format: SmfFormat, division: SmfDivision, tracks: &[Track]) -> Vec<u8> {
    let count = u16::try_from(tracks.len()).unwrap_or(u16::MAX);
    let header: Vec<u8> = [format.word(), count, division.word()]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect();
    std::iter::once(chunk(b"MThd", &header))
        .chain(tracks.iter().map(|t| chunk(b"MTrk", &body(t))))
        .collect::<Vec<_>>()
        .concat()
}
