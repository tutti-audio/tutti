//! The Clip File reader: bytes into [`ParsedClip`], a grammar of `winnow`
//! combinators over decoded packets.
//!
//! # Input
//!
//! The bytes after `SMF2CLIP` are cut into Universal MIDI Packets once, by
//! `tutti_ump::split_stream`, and each packet is classified once, by
//! `Ump::decode`, into a [`Packet`]. The grammar then runs over a
//! `&[Packet]` token slice, so a rule matches a whole packet ("a DCTPQ", "a Set
//! Tempo") and no rule reads a bit field: a word is never seen here, only what
//! `tutti-ump` made of it. A byte input would put the 32-bit word framing and
//! the packet lengths (Table 4 of M2-104) into the grammar, which are
//! `tutti-ump`'s.
//!
//! # The grammar
//!
//! ```text
//! clip      := "SMF2CLIP" preamble dctpq config start event* end eof
//! preamble  := ( gap(no DCS) SetProfileOn )*          -- M2-116 section 6
//! dctpq     := gap(DCS 0) DCTPQ                        -- sections 3.2.1, 6
//! config    := [SetTempo @0] [SetTimeSignature @0] ( gap Message )*
//!                                                      -- sections 6.1 to 6.1.2
//! start     := gap StartOfClip                         -- section 7
//! event     := gap Message                             -- sections 3.2.2, 7.2
//! end       := gap EndOfClip                           -- section 7
//! eof                                                  -- section 7.3
//! gap       := ( DCS | Null | JR | undefined Utility )*
//! ```
//!
//! The magic, the 32-bit alignment and a packet cut off at the end are checked
//! on the bytes, before the grammar. A rule that a file breaks past the point
//! where another rule could still match is a [`Fault`] that cuts, so the error
//! names the rule broken rather than the first rule that did not match.

use std::vec::Vec;

use winnow::Parser;
use winnow::combinator::{alt, cut_err, fail, opt, peek, repeat};
use winnow::error::{ContextError, ModalResult};
use winnow::token::any;

use tutti_ump::{
    Flex, Message, MessageType, Stream, SysexStatus, TicksPerQuarterNote, Ump, Utility,
    split_stream,
};

use super::{ClipFileError, ParsedClip, property_exchange};
use crate::ReadError;
use tutti_midi_ci::is_set_profile_on;

/// Why a rule refused, carried as winnow context and mapped to
/// [`ClipFileError`] once, in [`error_of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    MissingDctpq,
    ZeroDctpq,
    DctpqNotFirst,
    DctpqDelta,
    ProfileOnDelta,
    PropertyExchange,
    DuplicateDctpq,
    MissingStartOfClip,
    DuplicateStartOfClip,
    DuplicateHeaderTempo,
    DuplicateHeaderTimeSignature,
    MisplacedHeaderTempo,
    MisplacedHeaderTimeSignature,
    MissingEndOfClip,
    TrailingData,
}

/// A [`Fault`] and where the rule that found it began: the packets that were
/// left of the input then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cause {
    fault: Fault,
    remaining: usize,
}

type Res<T> = ModalResult<T, ContextError<Cause>>;

/// What a packet is to the grammar, decided once by `Ump::decode`.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A Delta Clockstamp and its ticks (M2-116 section 3.2.2).
    Dcs(u32),
    /// The Null message, which restarts the delta count.
    Noop,
    /// The DCTPQ and its ticks per quarter note; `None` for the reserved 0.
    Dctpq(Option<TicksPerQuarterNote>),
    StartOfClip,
    EndOfClip,
    /// Padding that says nothing of timing or structure: a JR message, or a
    /// Utility message with an undefined status (a reader may ignore these).
    Pad,
    /// A message of the clip.
    Message(Message),
    /// A packet that does not decode, kept as its words.
    Raw,
}

/// One packet and what it is.
#[derive(Clone, Copy, Debug)]
struct Packet {
    ump: Ump,
    kind: Kind,
}

fn classify(ump: Ump) -> Packet {
    let kind = match ump.decode() {
        Ok(Message::Utility(Utility::DeltaClockstamp(ticks))) => Kind::Dcs(ticks.get()),
        Ok(Message::Utility(Utility::TicksPerQuarterNote(ticks))) => Kind::Dctpq(Some(ticks)),
        Ok(Message::Utility(Utility::ReservedTicksPerQuarterNote(_))) => Kind::Dctpq(None),
        Ok(Message::Utility(Utility::Noop)) => Kind::Noop,
        Ok(Message::Utility(Utility::JrClock(_) | Utility::JrTimestamp(_))) => Kind::Pad,
        Err(_) if ump.message_type() == MessageType::Utility => Kind::Pad,
        Ok(Message::Stream(Stream::StartOfClip)) => Kind::StartOfClip,
        Ok(Message::Stream(Stream::EndOfClip)) => Kind::EndOfClip,
        Ok(message) => Kind::Message(message),
        Err(_) => Kind::Raw,
    };
    Packet { ump, kind }
}

type Input<'a> = &'a [Packet];

/// A message and the ticks since the one before it.
#[derive(Clone, Copy, Debug)]
struct Timed {
    delta: u64,
    ump: Ump,
}

/// Refuses for `fault`, past every alternative.
fn halt<'a, T>(
    fault: Fault,
    remaining: usize,
) -> impl Parser<Input<'a>, T, winnow::error::ErrMode<ContextError<Cause>>> {
    cut_err(fail.context(Cause { fault, remaining }))
}

/// Refuses for `fault` at the packet the input is at.
fn halt_here<'a, T>(fault: Fault) -> impl FnMut(&mut Input<'a>) -> Res<T> {
    move |input| halt(fault, input.len()).parse_next(input)
}

// ---- the gap ---------------------------------------------------------------

/// The timing packets between two messages: a Delta Clockstamp sets the time of
/// all following messages up to the next one (M2-116 section 3.2.2), and a DCS *replaces* the
/// count since the last event, so only a Null message, which restarts the
/// count, carries what came before it forward.
#[derive(Clone, Copy, Debug, Default)]
struct Gap {
    /// The value of the last DCS, if no Null message came after it.
    dcs: Option<u32>,
    /// What Null messages have closed off.
    banked: u64,
    /// The last DCS's ticks.
    pending: u32,
}

impl Gap {
    fn ticks(self) -> u64 {
        self.banked + u64::from(self.pending)
    }

    fn then(mut self, kind: Kind) -> Self {
        match kind {
            Kind::Dcs(ticks) => {
                self.pending = ticks;
                self.dcs = Some(ticks);
            }
            Kind::Noop => {
                self.banked += u64::from(self.pending);
                self.pending = 0;
                self.dcs = None;
            }
            _ => {}
        }
        self
    }
}

fn is_timing(kind: &Kind) -> bool {
    matches!(kind, Kind::Dcs(_) | Kind::Noop | Kind::Pad)
}

fn gap(input: &mut Input<'_>) -> Res<Gap> {
    repeat(0.., any.verify(|p: &Packet| is_timing(&p.kind)))
        .fold(Gap::default, |gap, p: Packet| gap.then(p.kind))
        .parse_next(input)
}

// ---- Set Profile On --------------------------------------------------------

/// Whether `message` is the first packet of a Set Profile On message (MIDI-CI
/// sub-ID 2 `0x22`: Universal SysEx `7E <device> 0D 22 ...`), and which.
fn profile_on_status(message: &Message) -> Option<SysexStatus> {
    let Message::Sysex7(_, packet) = message else {
        return None;
    };
    let first = matches!(packet.status(), SysexStatus::Complete | SysexStatus::Start);
    (first && is_set_profile_on(packet.data())).then(|| packet.status())
}

fn sysex_part(status: SysexStatus) -> impl FnMut(&mut Input<'_>) -> Res<Ump> {
    move |input| {
        any.verify_map(|p: Packet| match p.kind {
            Kind::Message(Message::Sysex7(_, packet)) if packet.status() == status => Some(p.ump),
            _ => None,
        })
        .parse_next(input)
    }
}

/// The first packet of a Set Profile On message.
fn profile_first(input: &mut Input<'_>) -> Res<(Ump, SysexStatus)> {
    any.verify_map(|p: Packet| match p.kind {
        Kind::Message(message) => profile_on_status(&message).map(|status| (p.ump, status)),
        _ => None,
    })
    .parse_next(input)
}

/// One Set Profile On message: a Complete packet, or a Start, Continues and an
/// End, adjacent.
fn set_profile_on(input: &mut Input<'_>) -> Res<Vec<Ump>> {
    let (first, status) = profile_first.parse_next(input)?;
    if status == SysexStatus::Complete {
        return Ok(vec![first]);
    }
    let rest: Vec<Ump> = repeat(0.., sysex_part(SysexStatus::Continue)).parse_next(input)?;
    let end = sysex_part(SysexStatus::End).parse_next(input)?;
    Ok([vec![first], rest, vec![end]].concat())
}

/// How many packets at the head of `header` are Set Profile On messages at
/// tick 0: the ones a write puts before the DCTPQ, where they carry no DCS.
pub(crate) fn leading_profile_len(header: &[(u64, Ump)]) -> usize {
    let packets: Vec<Packet> = header
        .iter()
        .take_while(|(tick, _)| *tick == 0)
        .map(|&(_, ump)| classify(ump))
        .collect();
    let mut input = &packets[..];
    let _ = repeat::<_, _, (), _, _>(0.., set_profile_on).parse_next(&mut input);
    packets.len() - input.len()
}

/// A Set Profile On message with no DCS before it (M2-116 section 6: a Set Profile On
/// has none).
fn preamble_item(input: &mut Input<'_>) -> Res<Vec<Ump>> {
    let before = gap.parse_next(input)?;
    peek(profile_first).parse_next(input)?;
    let start = input.len();
    if before.dcs.is_some() {
        return halt(Fault::ProfileOnDelta, start).parse_next(input);
    }
    match set_profile_on.parse_next(input) {
        Ok(message) => Ok(message),
        // A message's parts are adjacent: a DCS cut it, or something else did.
        Err(_) => match input.first().map(|p| p.kind) {
            Some(Kind::Dcs(_)) => halt(Fault::ProfileOnDelta, start).parse_next(input),
            _ => halt(Fault::DctpqNotFirst, start).parse_next(input),
        },
    }
}

// ---- the DCTPQ -------------------------------------------------------------

/// What a file with no DCTPQ where one belongs is: stream order decides, as a
/// reader would meet it. A Property Exchange message first, a DCTPQ later
/// (so something came before it), or no DCTPQ at all.
fn fault_before_dctpq(input: Input<'_>) -> Fault {
    for packet in input {
        match packet.kind {
            Kind::Message(m) if property_exchange(&m) => return Fault::PropertyExchange,
            Kind::Dctpq(_) => return Fault::DctpqNotFirst,
            _ => {}
        }
    }
    Fault::MissingDctpq
}

fn dctpq_after_gap(input: &mut Input<'_>) -> Res<(Gap, Option<TicksPerQuarterNote>)> {
    (
        gap,
        any.verify_map(|p: Packet| match p.kind {
            Kind::Dctpq(ticks) => Some(ticks),
            _ => None,
        }),
    )
        .parse_next(input)
}

/// The DCTPQ: it has a preceding Delta Clockstamp (DCS) of 0 ticks
/// (M2-116 sections 3.2.1 and 6), after the Set Profile On messages and before any other, and
/// declaring ticks.
fn dctpq(input: &mut Input<'_>) -> Res<TicksPerQuarterNote> {
    let fault = fault_before_dctpq(input);
    let start = input.len();
    let (before, ticks) = alt((dctpq_after_gap, halt_here(fault))).parse_next(input)?;
    if before.dcs != Some(0) || before.ticks() != 0 {
        return halt(Fault::DctpqDelta, start).parse_next(input);
    }
    match ticks {
        Some(ticks) => Ok(ticks),
        None => halt(Fault::ZeroDctpq, start).parse_next(input),
    }
}

// ---- the configuration header ----------------------------------------------

fn is_set_tempo(flex: &Flex) -> bool {
    matches!(flex, Flex::SetTempo(_))
}

fn is_set_time_signature(flex: &Flex) -> bool {
    matches!(flex, Flex::SetTimeSignature(_))
}

/// A Flex message of the kind `is`, at delta 0: M2-116 section 6.1.1 says the
/// Set Tempo comes first after the DCTPQ with the DCS before the DCTPQ
/// or a DCS of 0, and section 6.1.2 says the Set Time Signature uses the
/// Delta Clockstamp that precedes the DCTPQ or a preceding one of 0 ticks.
fn flex_at_zero<'a>(
    is: fn(&Flex) -> bool,
) -> impl Parser<Input<'a>, Timed, winnow::error::ErrMode<ContextError<Cause>>> {
    (gap, any).verify_map(move |(before, p): (Gap, Packet)| match p.kind {
        Kind::Message(Message::Flex(_, ref flex)) if is(flex) && before.ticks() == 0 => {
            Some(Timed {
                delta: 0,
                ump: p.ump,
            })
        }
        _ => None,
    })
}

/// A message of the clip, behind its gap, and what it is. A Property Exchange
/// message is refused (sections 6.4 and 7.2) and a DCTPQ is a second (section 6:
/// one); a Start or End of Clip is no message and backtracks to its own rule.
/// The packets left where its gap began come with it.
fn message(input: &mut Input<'_>) -> Res<(Timed, Kind, usize)> {
    let start = input.len();
    let (before, p) = (gap, any).parse_next(input)?;
    let timed = Timed {
        delta: before.ticks(),
        ump: p.ump,
    };
    match p.kind {
        Kind::Message(m) if property_exchange(&m) => {
            halt(Fault::PropertyExchange, start).parse_next(input)
        }
        Kind::Message(_) | Kind::Raw => Ok((timed, p.kind, start)),
        Kind::Dctpq(_) => halt(Fault::DuplicateDctpq, start).parse_next(input),
        _ => fail.parse_next(input),
    }
}

/// Any message of the header after the first two slots. One that is a Set Tempo
/// or a Set Time Signature has missed its slot: a second is a duplicate
/// (sections 6.1 and 6.1.2: at most one), the first is misplaced.
fn header_message<'a>(
    tempo: bool,
    signature: bool,
) -> impl Parser<Input<'a>, Timed, winnow::error::ErrMode<ContextError<Cause>>> {
    move |input: &mut Input<'a>| {
        let (timed, kind, start) = message.parse_next(input)?;
        match kind {
            Kind::Message(Message::Flex(_, ref flex)) if is_set_tempo(flex) => halt(
                if tempo {
                    Fault::DuplicateHeaderTempo
                } else {
                    Fault::MisplacedHeaderTempo
                },
                start,
            )
            .parse_next(input),
            Kind::Message(Message::Flex(_, ref flex)) if is_set_time_signature(flex) => halt(
                if signature {
                    Fault::DuplicateHeaderTimeSignature
                } else {
                    Fault::MisplacedHeaderTimeSignature
                },
                start,
            )
            .parse_next(input),
            _ => Ok(timed),
        }
    }
}

/// The messages that configure a receiver (M2-116 section 6.1), each behind its
/// own DCS (the header holds at most one Set Tempo and one Set Time
/// Signature): the Set Tempo first, the Set Time Signature right behind it (first,
/// if there is no Set Tempo), each at most once and at delta 0, then any other.
fn config(input: &mut Input<'_>) -> Res<Vec<Timed>> {
    let tempo = opt(flex_at_zero(is_set_tempo)).parse_next(input)?;
    let signature = opt(flex_at_zero(is_set_time_signature)).parse_next(input)?;
    let rest: Vec<Timed> =
        repeat(0.., header_message(tempo.is_some(), signature.is_some())).parse_next(input)?;
    Ok(tempo.into_iter().chain(signature).chain(rest).collect())
}

/// The Delta Clockstamp and the message that bracket the clip: the gap before
/// it, which for Start of Clip is the header's last (section 6.1: the DCSs
/// add up to the time allocated to the Clip Configuration Header).
fn bracket<'a>(
    which: fn(&Kind) -> bool,
    missing: Fault,
) -> impl Parser<Input<'a>, u64, winnow::error::ErrMode<ContextError<Cause>>> {
    alt((
        (gap, any.verify(move |p: &Packet| which(&p.kind))).map(|(before, _)| before.ticks()),
        halt_here(missing),
    ))
}

// ---- the sequence ----------------------------------------------------------

/// An event of the sequence (section 7.2: any UMP message but Property Exchange),
/// behind its DCS (section 3.2.2).
fn event(input: &mut Input<'_>) -> Res<Timed> {
    alt((message.map(|(timed, ..)| timed), second_start_of_clip)).parse_next(input)
}

/// A second Start of Clip: section 7 asks for one.
fn second_start_of_clip(input: &mut Input<'_>) -> Res<Timed> {
    let start = input.len();
    (
        gap,
        any.verify(|p: &Packet| matches!(p.kind, Kind::StartOfClip)),
    )
        .parse_next(input)?;
    halt(Fault::DuplicateStartOfClip, start).parse_next(input)
}

/// A clip as the grammar reads it.
struct Clip {
    ticks_per_quarter: TicksPerQuarterNote,
    preamble: Vec<Ump>,
    header: Vec<Timed>,
    /// The ticks of the DCS before Start of Clip.
    header_gap: u64,
    events: Vec<Timed>,
    /// The ticks of the DCS before End of Clip.
    end_gap: u64,
}

fn clip(input: &mut Input<'_>) -> Res<Clip> {
    let preamble: Vec<Vec<Ump>> = repeat(0.., preamble_item).parse_next(input)?;
    let ticks_per_quarter = dctpq.parse_next(input)?;
    let header = config.parse_next(input)?;
    let header_gap = bracket(
        |k| matches!(k, Kind::StartOfClip),
        Fault::MissingStartOfClip,
    )
    .parse_next(input)?;
    let events: Vec<Timed> = repeat(0.., event).parse_next(input)?;
    let end_gap =
        bracket(|k| matches!(k, Kind::EndOfClip), Fault::MissingEndOfClip).parse_next(input)?;
    // Section 7.3: no data follows the End of Clip message.
    if !input.is_empty() {
        return halt(Fault::TrailingData, input.len()).parse_next(input);
    }
    Ok(Clip {
        ticks_per_quarter,
        preamble: preamble.concat(),
        header,
        header_gap,
        events,
        end_gap,
    })
}

// ---- the edge --------------------------------------------------------------

/// The one place winnow's error becomes ours: the rule broken, and the packets
/// left of the input where the rule began. The grammar ends at End of Clip, so
/// a refusal that names no rule is a stream that ended before it. A rule that
/// ran out of packets where a cut-off last packet stands (`torn`) names that
/// instead.
fn error_of(e: &winnow::error::ErrMode<ContextError<Cause>>, torn: bool) -> (ClipFileError, usize) {
    let cause = match e {
        winnow::error::ErrMode::Backtrack(e) | winnow::error::ErrMode::Cut(e) => {
            e.context().next().copied()
        }
        winnow::error::ErrMode::Incomplete(_) => None,
    };
    let Cause { fault, remaining } = cause.unwrap_or(Cause {
        fault: Fault::MissingEndOfClip,
        remaining: 0,
    });
    (rule_of(fault, torn), remaining)
}

fn rule_of(fault: Fault, torn: bool) -> ClipFileError {
    match fault {
        Fault::MissingDctpq | Fault::MissingStartOfClip | Fault::MissingEndOfClip if torn => {
            ClipFileError::MessageCutOff
        }
        Fault::MissingDctpq => ClipFileError::MissingDctpq,
        Fault::ZeroDctpq => ClipFileError::ZeroDctpq,
        Fault::DctpqNotFirst => ClipFileError::DctpqNotFirst,
        Fault::DctpqDelta => ClipFileError::DctpqDelta,
        Fault::ProfileOnDelta => ClipFileError::ProfileOnDelta,
        Fault::PropertyExchange => ClipFileError::PropertyExchange,
        Fault::DuplicateDctpq => ClipFileError::DuplicateDctpq,
        Fault::MissingStartOfClip => ClipFileError::MissingStartOfClip,
        Fault::DuplicateStartOfClip => ClipFileError::DuplicateStartOfClip,
        Fault::DuplicateHeaderTempo => ClipFileError::DuplicateHeaderTempo,
        Fault::DuplicateHeaderTimeSignature => ClipFileError::DuplicateHeaderTimeSignature,
        Fault::MisplacedHeaderTempo => ClipFileError::MisplacedHeaderTempo,
        Fault::MisplacedHeaderTimeSignature => ClipFileError::MisplacedHeaderTimeSignature,
        Fault::MissingEndOfClip => ClipFileError::MissingEndOfClip,
        Fault::TrailingData => ClipFileError::TrailingData,
    }
}

/// Packets at absolute ticks: each message's delta added to the one before.
fn absolute(timed: &[Timed]) -> Vec<(u64, Ump)> {
    timed
        .iter()
        .scan(0u64, |tick, t| {
            *tick += t.delta;
            Some((*tick, t.ump))
        })
        .collect()
}

/// The refusal of `cause`, found `at` packets into `packets`.
fn refusal(packets: &[Packet], at: usize, cause: ClipFileError) -> ReadError {
    let word = packets[..at].iter().map(|p| p.ump.words().len()).sum();
    ReadError::Clip { word, cause }
}

/// Parses a Clip File's `body`, the bytes after its magic.
///
/// # Errors
///
/// [`ReadError::Clip`] naming the rule broken and the word it was found at.
pub(crate) fn parse(body: &[u8]) -> Result<ParsedClip, ReadError> {
    let Some(words) = words_of(body) else {
        let word = body.len() / 4;
        return Err(ReadError::Clip {
            word,
            cause: ClipFileError::Unaligned,
        });
    };
    // `split_stream` ends at a truncated packet without saying so: the words
    // it left are what the packets fall short of.
    let packets: Vec<Packet> = split_stream(&words).map(classify).collect();
    let taken: usize = packets.iter().map(|p| p.ump.words().len()).sum();
    let torn = taken < words.len();

    let mut input = &packets[..];
    let clip = clip.parse_next(&mut input).map_err(|e| {
        let (cause, remaining) = error_of(&e, torn);
        // A cut-off packet stands past the last whole one.
        let at = if cause == ClipFileError::MessageCutOff {
            packets.len()
        } else {
            packets.len() - remaining
        };
        refusal(&packets, at, cause)
    })?;
    if torn {
        return Err(ReadError::Clip {
            word: taken,
            cause: ClipFileError::TrailingData,
        });
    }

    let mut header: Vec<(u64, Ump)> = clip.preamble.into_iter().map(|ump| (0, ump)).collect();
    let after_preamble = absolute(&clip.header);
    let header_end = after_preamble.last().map_or(0, |&(tick, _)| tick) + clip.header_gap;
    header.extend(after_preamble);
    let events = absolute(&clip.events);
    let end = events.last().map_or(0, |&(tick, _)| tick) + clip.end_gap;
    Ok(ParsedClip {
        ticks_per_quarter: clip.ticks_per_quarter,
        header,
        events,
        end,
        header_end,
    })
}

/// The header's own rules (M2-116 sections 6.1 to 6.1.2) over `header`, the
/// grammar's `config` on the packets a write would put there, each behind a
/// Delta Clockstamp of its distance from the one before; the leading Set
/// Profile On messages stand before the DCTPQ and are not part of it.
///
/// # Errors
///
/// The [`ClipFileError`] naming the rule broken.
pub(crate) fn check_header(header: &[(u64, Ump)]) -> Result<(), ClipFileError> {
    let leading = leading_profile_len(header);
    for message in super::profiles(header[..leading].iter().map(|&(_, ump)| ump)) {
        message.map_err(ClipFileError::ProfileOn)?;
    }
    let mut at = 0;
    let packets: Vec<Packet> = header[leading..]
        .iter()
        .flat_map(|&(tick, ump)| {
            let delta = u32::try_from(tick.saturating_sub(at)).unwrap_or(u32::MAX);
            at = tick;
            [
                Packet {
                    ump,
                    kind: Kind::Dcs(delta),
                },
                classify(ump),
            ]
        })
        .collect();
    config
        .parse_next(&mut &packets[..])
        .map(|_| ())
        .map_err(|e| error_of(&e, false).0)
}

/// The big-endian 32-bit words of `bytes`, `None` if it is not a whole number
/// of them.
fn words_of(bytes: &[u8]) -> Option<Vec<u32>> {
    // `as_chunks`: the remainder is empty once the length is a multiple of 4,
    // and each chunk arrives as the `[u8; 4]` `from_be_bytes` takes.
    let (chunks, rest) = bytes.as_chunks::<4>();
    rest.is_empty()
        .then(|| chunks.iter().map(|c| u32::from_be_bytes(*c)).collect())
}
