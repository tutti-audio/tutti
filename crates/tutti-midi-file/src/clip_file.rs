//! MIDI 2.0 **Clip File** (M2-116) byte codec: the wire half under
//! [`ClipFile`](crate::ClipFile).
//!
//! A Clip File is one sequence of Universal MIDI Packets. Layout (M2-116
//! sections 4 to 7, all big-endian):
//!
//! ```text
//! ["SMF2CLIP" 8 bytes] [Set Profile On]* [DCS=0] [DCTPQ] [ (DCS * UMP)* ]
//!   [DCS] [StartOfClip] [ (DCS * UMP)* ] [DCS] [EndOfClip]
//! ```
//!
//! Everything from the file header up to the DCS of Start of Clip is the
//! **Clip Configuration Header** (M2-116 section 6): leading Set Profile On
//! messages (no DCS), the DCTPQ, and any UMP messages that configure a
//! receiver (Bank Select, Program Change, volume, System Exclusive), each
//! behind its own DCS. It holds at most one Set Tempo and one Set Time
//! Signature (sections 6.1 to 6.1.2), both at delta 0. A leading Set Profile On
//! is a [`ProfileOnRequest`](tutti_midi_ci::ProfileOnRequest) (section
//! 6.2), and one that does not read is
//! refused. The DCS before Start of Clip is the header's last gap, and with the
//! others sets the total time allocated to the Clip Configuration Header
//! (section 6.1): it is kept ([`ParsedClip::header_end`]). The rest, from Start
//! of Clip to End of Clip, is the Sequence Data (section 7). No Property
//! Exchange message is in either (sections 6.4 and 7.2).
//!
//! Start of Clip is tick 0 of the sequence, and music begins there: the first
//! bar starts at Start of Clip (section 7.1), a pickup bar is that first bar
//! with a Set Time Signature of its own length and the bar after it starts
//! with a new Set Time Signature (section 7.1.3). The first Set Tempo and the
//! first Set Time Signature, at tick 0 right after Start of Clip, are written
//! under the Delta Clockstamp before Start of Clip (sections 7.1.1 and 7.1.2).
//!
//! Every UMP is governed by a **Delta Clockstamp** (DCS) giving the ticks since
//! the previous event: a Delta Clockstamp sets the time of all following
//! messages up to the next one (section 3.2.2), so events at one time may share a single Clockstamp. The writer gives each
//! event its own, except the parts of a Lyric or Ruby message, which share the
//! first's (see [`write`](fn@write)). The tick unit is declared once by the **DCTPQ** (Delta
//! Clockstamp Ticks Per Quarter Note). The stream is bracketed by Start of Clip
//! and End of Clip UMP Stream messages, and the DCS before End of Clip is the
//! clip's length.
//!
//! This module reads (a `winnow` grammar, in `read.rs`) and writes a clip as
//! absolute ticks and packets, with no decoding past the structure: [`ClipFile`](crate::ClipFile) is the model
//! over it.

use std::vec::Vec;

use crate::Tick;

use tutti_midi_ci::{NotSetProfileOn, is_property_exchange};
use tutti_ump::{
    DeltaTicks, Flex, FlexTextKind, Form, Message, Stream, SysexStatus, TicksPerQuarterNote, Ump,
    Utility,
};

mod profile;
mod read;

pub(crate) use profile::{profile_packets, profiles};
pub(crate) use read::{check_header, leading_profile_len, parse};

/// The 8-byte file header: ASCII "SMF2CLIP" (M2-116 section 5).
pub(crate) const CLIP_FILE_MAGIC: [u8; 8] = *b"SMF2CLIP";

/// A clip as the wire states it: the tick unit, the events at absolute
/// ticks, and the tick the clip ends at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedClip {
    /// Ticks per quarter note (the DCTPQ), never zero.
    pub(crate) ticks_per_quarter: TicksPerQuarterNote,
    /// The header's messages after the file header, as `(tick, packet)`: the
    /// leading Set Profile On packets at 0, then each message behind its DCS,
    /// ticks counted from the DCTPQ (the header's own clock, which runs
    /// before the sequence).
    pub(crate) header: Vec<(Tick, Ump)>,
    /// Every packet between Start of Clip and End of Clip that is not itself
    /// structure (a DCS, a Null, a JR message, the DCTPQ), at its absolute
    /// tick, in file order.
    pub(crate) events: Vec<(Tick, Ump)>,
    /// The tick of End of Clip: the clip's length.
    pub(crate) end: Tick,
    /// The tick, on the header's clock, of Start of Clip: the header's last
    /// message's tick plus the Delta Clockstamp before Start of Clip.
    pub(crate) header_end: Tick,
}

/// Why a byte stream failed to parse as a MIDI Clip File (M2-116). Distinguishes
/// "this isn't a clip file" from "this clip file is malformed" so a caller can
/// react differently (e.g. try another importer vs. report corruption).
///
/// The structure rules are one variant each: [`DctpqNotFirst`](Self::DctpqNotFirst)
/// and [`DuplicateDctpq`](Self::DuplicateDctpq) (section 6),
/// [`DuplicateHeaderTempo`](Self::DuplicateHeaderTempo),
/// [`MisplacedHeaderTempo`](Self::MisplacedHeaderTempo),
/// [`DuplicateHeaderTimeSignature`](Self::DuplicateHeaderTimeSignature) and
/// [`MisplacedHeaderTimeSignature`](Self::MisplacedHeaderTimeSignature)
/// (sections 6.1 to 6.1.2), [`DctpqDelta`](Self::DctpqDelta) and
/// [`ProfileOnDelta`](Self::ProfileOnDelta) (sections 3.2.1 and 6),
/// [`PropertyExchange`](Self::PropertyExchange) (sections 6.4 and 7.2),
/// [`ProfileOn`](Self::ProfileOn) (section 6.2),
/// [`DuplicateStartOfClip`](Self::DuplicateStartOfClip) (section 7) and
/// [`TrailingData`](Self::TrailingData) (section 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ClipFileError {
    /// Fewer than 8 bytes, or the leading 8 bytes are not `SMF2CLIP`.
    #[error("not a MIDI Clip File (bad or missing SMF2CLIP magic)")]
    BadMagic,
    /// The body length is not a whole number of 32-bit UMP words.
    #[error("clip body is not 32-bit-word aligned")]
    Unaligned,
    /// A packet claims more words than remain in the stream.
    #[error("a message runs past the end of the clip file")]
    MessageCutOff,
    /// No DCTPQ (tick-unit declaration) in the file.
    #[error("clip file has no DCTPQ tick-unit declaration")]
    MissingDctpq,
    /// The DCTPQ declared zero ticks per quarter note.
    ///
    /// Distinct from [`MissingDctpq`](Self::MissingDctpq) because the file did
    /// declare one: it is unusable rather than absent, and a caller reporting
    /// corruption wants to say which. Every beat in the file is a tick count
    /// divided by this.
    #[error("clip file declares zero ticks per quarter note")]
    ZeroDctpq,
    /// A message other than a Set Profile On comes before the DCTPQ. M2-116
    /// section 6: the DCTPQ follows the Set Profile On messages, or the file
    /// header if there are none.
    #[error("clip file has a message before its DCTPQ")]
    DctpqNotFirst,
    /// The DCTPQ has no Delta Clockstamp right before it, or one that is not 0.
    /// Section 3.2.1 and section 6: the DCTPQ has a preceding Delta
    /// Clockstamp (see Section 3.2.2) of 0 ticks.
    #[error("clip file's DCTPQ has no Delta Clockstamp of 0 before it")]
    DctpqDelta,
    /// A Set Profile On message has a Delta Clockstamp before it. Section 6:
    /// a Set Profile On has none.
    #[error("clip file's Set Profile On has a Delta Clockstamp before it")]
    ProfileOnDelta,
    /// A Set Profile On message of the header that is not one: the MIDI-CI
    /// message of M2-101 section 7.8 is what section 6.2 puts there.
    #[error("clip header's Set Profile On is malformed")]
    ProfileOn(#[source] NotSetProfileOn),
    /// A MIDI-CI Property Exchange message (Universal System Exclusive `7E`,
    /// sub-ID#1 `0D`, sub-ID#2 `30` to `3F`). Section 6.4: Property Data for PE
    /// Resources is not included in a Clip File; section 7.2: the
    /// sequence data holds any UMP message but Property Exchange.
    #[error("clip file holds a Property Exchange message")]
    PropertyExchange,
    /// A second DCTPQ. The tick unit is declared once.
    #[error("clip file declares its DCTPQ more than once")]
    DuplicateDctpq,
    /// No Start of Clip message. M2-116 section 7: the sequence data
    /// has one Start of Clip message, first.
    #[error("clip file has no Start of Clip message")]
    MissingStartOfClip,
    /// A second Start of Clip. Section 7 asks for one.
    #[error("clip file has more than one Start of Clip message")]
    DuplicateStartOfClip,
    /// A second Set Tempo in the header. Section 6.1: the header holds at
    /// most one.
    #[error("clip header has more than one Set Tempo")]
    DuplicateHeaderTempo,
    /// A second Set Time Signature in the header. Section 6.1.2: at most one.
    #[error("clip header has more than one Set Time Signature")]
    DuplicateHeaderTimeSignature,
    /// The header's Set Tempo is not the first message after the DCTPQ, or its
    /// Delta Clockstamp is not 0. Section 6.1.1: it is first, with a DCS
    /// of 0 or sharing the DCTPQ's.
    #[error("clip header's Set Tempo is not first after the DCTPQ at delta 0")]
    MisplacedHeaderTempo,
    /// The header's Set Time Signature does not immediately follow the Set
    /// Tempo (or the DCTPQ, in a header with no Set Tempo), or its Delta
    /// Clockstamp is not 0. Section 6.1.2: it uses the Delta Clockstamp
    /// that precedes the DCTPQ or has a preceding one of 0 ticks.
    #[error("clip header's Set Time Signature is not first after the Set Tempo at delta 0")]
    MisplacedHeaderTimeSignature,
    /// No End of Clip message. Section 7: the sequence data
    /// has one End of Clip message, last.
    #[error("clip file has no End of Clip message")]
    MissingEndOfClip,
    /// Bytes follow the End of Clip, which must be last. Section 7.3: no
    /// data follows the End of Clip message.
    /// There is no multi-clip clip file: that is what the MIDI Container File
    /// is for.
    #[error("clip file has data following the End of Clip message")]
    TrailingData,
}

/// Whether `message` starts a MIDI-CI Property Exchange message: a System
/// Exclusive 7 packet, first of its message, that begins `7E <device> 0D` and a
/// sub-ID#2 of `30` to `3F` (M2-101 Table 5, Property Exchange Messages). The
/// packets after the first are not told apart, as they say nothing of what the
/// message is; refusing the first refuses the message.
pub(crate) fn property_exchange(message: &Message) -> bool {
    let Message::Sysex7(_, packet) = message else {
        return false;
    };
    matches!(packet.status(), SysexStatus::Complete | SysexStatus::Start)
        && is_property_exchange(packet.data())
}

/// Serializes a clip: the leading Set Profile On packets (no DCS), the DCTPQ,
/// the rest of the header, Start of Clip, each event behind its Delta
/// Clockstamp, and End of Clip behind the DCS that makes the clip `end` ticks
/// long. `header` and `events` are absolute ticks in order (the header's on
/// its own clock), `events` none past `end`, and the header ends at `header_end`,
/// not before its last message: the Delta Clockstamp before Start of Clip is
/// the gap that makes it so.
///
/// **One Delta Clockstamp for the parts of a Lyric or Ruby message.**
/// M2-116 section 3 has a Delta Clockstamp precede every UMP, and
/// M2-104 sections 7.5.10 and 7.5.12 put all parts of a Lyric Data Message
/// under one Delta Clockstamp in a Clip File (the Ruby Data Message
/// likewise). They agree once the Clip File's own definition of a DCS is read:
/// it sets the time of all following messages up to the next one (M2-116
/// section 3.2.2; M2-104 section 7.2.3.2 lets simultaneous events share one), so a DCS precedes
/// every UMP in that it governs it. A zero DCS between the parts would be a
/// second Delta Clock Stamp, not a shared one. So the parts after the first are
/// written with none, and every other event keeps its own.
///
/// [`ClipFile`]: crate::ClipFile
pub(crate) fn write(
    ticks_per_quarter: TicksPerQuarterNote,
    header: &[(Tick, Ump)],
    header_end: Tick,
    events: &[(Tick, Ump)],
    end: Tick,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + (header.len() + events.len() + 4) * 8);
    out.extend_from_slice(&CLIP_FILE_MAGIC);

    // Set Profile On messages come first and carry no DCS (section 6).
    let pre = leading_profile_len(header);
    for (_, ump) in &header[..pre] {
        push_words(&mut out, ump.words());
    }

    // DCTPQ declares the tick unit (preceded by a zero DCS per section 3.2.1).
    push_words(&mut out, delta_clockstamp(0).words());
    push_words(&mut out, dctpq(ticks_per_quarter).words());

    // The rest of the header, each message behind its DCS.
    let mut at = Tick(0);
    for &(tick, ump) in &header[pre..] {
        push_delta(&mut out, tick.ticks_since(at).unwrap_or(0));
        push_words(&mut out, ump.words());
        at = tick;
    }

    // Start of Clip, behind the DCS that ends the header's allotted time.
    push_delta(&mut out, header_end.ticks_since(at).unwrap_or(0));
    push_words(&mut out, start_of_clip().words());

    // The timed event stream: each UMP preceded by its delta clockstamp(s),
    // except the first Set Tempo and the first Set Time Signature at the start
    // of the sequence, which use the one before Start of Clip (sections 7.1.1
    // and 7.1.2).
    // A DCS field is only 20 bits, so a delta beyond `DCS_MAX` is expressed as
    // DCS*NOOP restarts (M2-116 section 3.2.2): see `push_delta`.
    let mut at = Tick(0);
    let mut previous: Option<Ump> = None;
    let mut lead = Lead::Tempo;
    for &(tick, ump) in events {
        let rides = lead.rides_start(tick, ump);
        let shares = tick == at && (rides || previous.is_some_and(|p| continues_text(p, ump)));
        if !shares {
            push_delta(&mut out, tick.ticks_since(at).unwrap_or(0));
        }
        push_words(&mut out, ump.words());
        at = tick;
        previous = Some(ump);
    }

    // End of Clip, behind the DCS that is the clip's length.
    push_delta(&mut out, end.ticks_since(at).unwrap_or(0));
    push_words(&mut out, end_of_clip().words());
    out
}

/// Which of the two messages that ride the Delta Clockstamp of Start of Clip
/// (M2-116 sections 7.1.1 and 7.1.2) the writer looks for next: the sequence
/// data's first Set Tempo, then its first Set Time Signature, each at tick 0
/// and directly after the one before.
#[derive(Clone, Copy)]
enum Lead {
    Tempo,
    Signature,
    Done,
}

impl Lead {
    /// Whether `ump` at `tick` is the message looked for, and the look moved on:
    /// a message that is not it ends the look.
    fn rides_start(&mut self, tick: Tick, ump: Ump) -> bool {
        let flex = match ump.decode() {
            Ok(Message::Flex(_, flex)) if tick == Tick(0) => Some(flex),
            _ => None,
        };
        let (rides, next) = match (*self, flex) {
            (Self::Tempo, Some(Flex::SetTempo(_))) => (true, Self::Signature),
            (Self::Tempo | Self::Signature, Some(Flex::SetTimeSignature(_))) => (true, Self::Done),
            _ => (false, Self::Done),
        };
        *self = next;
        rides
    }
}

/// Whether `next` is a Lyric or Ruby packet that continues the message `previous`
/// is a part of: the same kind, Group and address, `previous` a Start or a
/// Continue and `next` a Continue or an End.
fn continues_text(previous: Ump, next: Ump) -> bool {
    let (
        Ok(Message::Flex(
            group_p,
            Flex::Text {
                address: addr_p,
                kind: kind_p,
                form: form_p,
                ..
            },
        )),
        Ok(Message::Flex(
            group_n,
            Flex::Text {
                address: addr_n,
                kind: kind_n,
                form: form_n,
                ..
            },
        )),
    ) = (previous.decode(), next.decode())
    else {
        return false;
    };
    matches!(kind_p, FlexTextKind::Lyrics | FlexTextKind::Ruby)
        && (group_p, addr_p, kind_p) == (group_n, addr_n, kind_n)
        && matches!(form_p, Form::Start | Form::Continue)
        && matches!(form_n, Form::Continue | Form::End)
}

// --- helpers ---------------------------------------------------------------

fn push_words(out: &mut Vec<u8>, words: &[u32]) {
    for w in words {
        out.extend_from_slice(&w.to_be_bytes());
    }
}

/// Widest value a single 20-bit Delta Clockstamp field can carry.
const DCS_MAX: u64 = 0x000F_FFFF;

fn delta_clockstamp(ticks: u64) -> Ump {
    Ump::from(Message::Utility(Utility::DeltaClockstamp(
        DeltaTicks::masked((ticks & DCS_MAX) as u32),
    )))
}

/// Emit `ticks` as Delta Clockstamps, restarting the count with a NOOP each
/// time a gap exceeds the 20-bit DCS field.
///
/// M2-116 section 3.2.2 (identically M2-104-UM section 7.2.3.2): when no
/// message has occurred during the previous 1,048,575 ticks, the writer inserts
/// a Delta Clockstamp followed by a Null message to restart the count, and the
/// next Delta Clockstamp counts the ticks since that Null message.
///
/// So consecutive DCSs do **not** accumulate: each one is the count since the
/// last event, and a NOOP is what makes a long gap expressible as a sequence of
/// restarts. `0` still emits exactly one (zero) DCS.
fn push_delta(out: &mut Vec<u8>, mut ticks: u64) {
    while ticks > DCS_MAX {
        push_words(out, delta_clockstamp(DCS_MAX).words());
        push_words(out, Ump::from(Message::Utility(Utility::Noop)).words());
        ticks -= DCS_MAX;
    }
    push_words(out, delta_clockstamp(ticks).words());
}

/// The bytes of the Delta Clockstamp and Null pairs that restart the count
/// over `ticks` of gap (8 bytes a restart): what a gap of that length costs to
/// write beyond its own Delta Clockstamp.
pub(crate) fn restart_bytes(ticks: Tick) -> u64 {
    ticks.0 / DCS_MAX * 8
}

fn dctpq(ticks_per_quarter: TicksPerQuarterNote) -> Ump {
    Ump::from(Message::Utility(Utility::TicksPerQuarterNote(
        ticks_per_quarter,
    )))
}

fn start_of_clip() -> Ump {
    Ump::from(Message::Stream(Stream::StartOfClip))
}

fn end_of_clip() -> Ump {
    Ump::from(Message::Stream(Stream::EndOfClip))
}
