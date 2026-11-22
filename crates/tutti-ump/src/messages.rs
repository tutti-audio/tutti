//! Where one message ends in a run of packets: the cut a sink may make.
//!
//! A System Exclusive message (7-bit or 8-bit), a Mixed Data Set chunk, and a
//! text of the UMP Stream or of Flex Data are several packets. A batch of
//! packets cut inside one of them leaves its receiver with a message that
//! never ends, so a cut falls only between messages. [`spans`] splits a
//! batch there, and is the one place that knows where a message of several
//! packets ends.
//!
//! A packet that is not a message of several is a message on its own, as is a
//! continuation or an end with no start before it. A message of several packets
//! ends when every run opened inside it has closed: SysEx8 streams, and runs on
//! different Groups, interleave (M2-104 §7.7.1), so the message holds them
//! all. A Utility packet (a JR Timestamp or Clock), a System Real Time message
//! (§7.5.2 puts them inside Flex text and a SysEx run) and a packet of a Group
//! with no run open pass through an open run and belong to the message, so a cut
//! never falls between a packet and the timestamp that precedes the next. A
//! packet of a Group that has a run open and is not part of it ends the
//! message before it.

use crate::{
    Data128, Field, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockId, Message, MessageType,
    MidiGroup, Stream, Sysex8StreamId, SysexStatus, Ump,
};

/// Which message of several packets a packet belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Sysex7(MidiGroup),
    Sysex8(MidiGroup, Sysex8StreamId),
    FlexText(MidiGroup, FlexAddress, FlexTextKind),
    EndpointName,
    ProductInstanceId,
    BlockName(Field<FunctionBlockId>),
}

/// A packet's place in what it is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    /// A message of one packet.
    Whole,
    /// The first packet of a message of several.
    Opens(Run),
    /// A middle packet.
    Inside(Run),
    /// The last packet.
    Closes(Run),
    /// A Mixed Data Set chunk header: this many payload packets follow.
    Chunk(usize),
}

const fn sysex(status: SysexStatus, run: Run) -> Part {
    match status {
        SysexStatus::Start => Part::Opens(run),
        SysexStatus::Continue => Part::Inside(run),
        SysexStatus::End => Part::Closes(run),
        _ => Part::Whole,
    }
}

const fn form(form: Form, run: Run) -> Part {
    match form {
        Form::Complete => Part::Whole,
        Form::Start => Part::Opens(run),
        Form::Continue => Part::Inside(run),
        Form::End => Part::Closes(run),
    }
}

fn part(ump: Ump) -> Part {
    match ump.decode() {
        Ok(Message::Sysex7(group, packet)) => sysex(packet.status(), Run::Sysex7(group)),
        Ok(Message::Data128(group, Data128::Sysex8(packet))) => {
            sysex(packet.status(), Run::Sysex8(group, packet.stream_id()))
        }
        Ok(Message::Data128(group, Data128::Sysex8Abort { stream_id })) => {
            Part::Closes(Run::Sysex8(group, stream_id))
        }
        Ok(Message::Data128(_, Data128::MixedDataHeader(header))) => {
            Part::Chunk(usize::from(header.chunk_size.payload_umps()))
        }
        Ok(Message::Flex(
            group,
            Flex::Text {
                address,
                kind,
                form: at,
                ..
            },
        )) => form(at, Run::FlexText(group, address, kind)),
        Ok(Message::Stream(Stream::EndpointName(at, _))) => form(at, Run::EndpointName),
        Ok(Message::Stream(Stream::ProductInstanceId(at, _))) => form(at, Run::ProductInstanceId),
        Ok(Message::Stream(Stream::FunctionBlockName {
            block, form: at, ..
        })) => form(at, Run::BlockName(block)),
        _ => Part::Whole,
    }
}

/// Whether a packet is a message of its own or goes with the packet before it.
///
/// A sender that makes several packets of one event (a MIDI 2.0 registered
/// controller translated to the Control Changes that select it and set its value)
/// marks all after the first, so that a cut does not fall between them: a
/// receiver left with the controller selected and no value is wrong until the
/// next one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Join {
    /// The packet begins its own message, or is one the wire frames itself.
    #[default]
    Alone,
    /// The packet belongs to the message of the packet before it.
    ToPrevious,
}

/// How many of the first packets of `items` are one message.
fn message_len<T>(items: &[T], ump: &impl Fn(&T) -> Ump, join: &impl Fn(&T) -> Join) -> usize {
    let framed = framed_len(items, ump);
    let joined = items[framed..]
        .iter()
        .take_while(|item| join(item) == Join::ToPrevious)
        .count();
    framed + joined
}

/// How many of the first packets of `items` the wire frames as one message.
fn framed_len<T>(items: &[T], ump: &impl Fn(&T) -> Ump) -> usize {
    let Some(first) = items.first() else {
        return 0;
    };
    match part(ump(first)) {
        Part::Whole | Part::Inside(_) | Part::Closes(_) => 1,
        Part::Chunk(payloads) => {
            1 + items[1..]
                .iter()
                .take(payloads)
                .take_while(|item| {
                    matches!(
                        ump(item).decode(),
                        Ok(Message::Data128(_, Data128::MixedDataPayload { .. }))
                    )
                })
                .count()
        }
        Part::Opens(run) => {
            let mut open = OpenRuns::new();
            open.push(run);
            // The last packet known to belong to the message; a packet that
            // only passes through is held with it only if another follows.
            let mut last = 0;
            for (i, item) in items.iter().enumerate().skip(1) {
                let packet = ump(item);
                match part(packet) {
                    Part::Inside(of) if open.contains(of) => last = i,
                    Part::Closes(of) if open.contains(of) => {
                        open.remove(of);
                        last = i;
                        if open.is_empty() {
                            return i + 1;
                        }
                    }
                    Part::Opens(of) if !open.contains(of) => {
                        if !open.push(of) {
                            break;
                        }
                        last = i;
                    }
                    // What passes through an open run: Utility messages, System
                    // Real Time messages (not the rest of MT 1, which a run
                    // does not hold), UMP Stream messages (groupless), and a
                    // packet of a Group with no run open. Anything else ends
                    // the message.
                    _ if passes(packet, &open) => {}
                    _ => break,
                }
            }
            last + 1
        }
    }
}

/// Whether `packet`, which is no part of the runs `open`, sits inside them.
fn passes(packet: Ump, open: &OpenRuns) -> bool {
    match packet.message_type() {
        MessageType::Utility | MessageType::Stream => true,
        // System Real Time: status 0xF8 and above.
        MessageType::System => packet.is_system_real_time() || !open.has_group_of(packet),
        _ => !open.has_group_of(packet),
    }
}

/// The runs open inside one message: a fixed set, so the split allocates
/// nothing. SysEx8 streams, and runs of other Groups, interleave.
struct OpenRuns {
    runs: [Option<Run>; Self::MAX],
}

impl OpenRuns {
    /// More runs than this open at once end the message before the next.
    const MAX: usize = 8;

    const fn new() -> Self {
        Self {
            runs: [None; Self::MAX],
        }
    }

    /// Opens `run`; `false` when the set is full.
    fn push(&mut self, run: Run) -> bool {
        match self.runs.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(run);
                true
            }
            None => false,
        }
    }

    fn contains(&self, run: Run) -> bool {
        self.runs.contains(&Some(run))
    }

    fn remove(&mut self, run: Run) {
        if let Some(slot) = self.runs.iter_mut().find(|slot| **slot == Some(run)) {
            *slot = None;
        }
    }

    fn is_empty(&self) -> bool {
        self.runs.iter().all(Option::is_none)
    }

    /// Whether a run is open on the Group `packet` is on.
    fn has_group_of(&self, packet: Ump) -> bool {
        packet.group().is_some_and(|group| {
            self.runs
                .iter()
                .flatten()
                .any(|run| run.group() == Some(group))
        })
    }
}

impl Run {
    /// The Group the run is on, for the runs that have one.
    const fn group(self) -> Option<MidiGroup> {
        match self {
            Self::Sysex7(group) | Self::Sysex8(group, _) | Self::FlexText(group, ..) => Some(group),
            _ => None,
        }
    }
}

/// The packets of one message of a batch: what a cut keeps together.
#[derive(Debug, PartialEq, Eq)]
pub struct PacketSpan<'a, T>(&'a [T]);

impl<T> Clone for PacketSpan<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PacketSpan<'_, T> {}

impl<'a, T> PacketSpan<'a, T> {
    /// The packets, at least one.
    #[must_use]
    pub const fn packets(self) -> &'a [T] {
        self.0
    }

    /// How many packets.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.len()
    }

    /// Never: a span has at least one packet.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }
}

/// The messages of `items`, in order, each a [`PacketSpan`] of its packets. `ump`
/// reads the packet an item carries. A message of several packets whose last
/// the batch never reaches ends where the batch does.
///
///
/// A message holds at most 8 runs open at once (interleaved SysEx8 streams,
/// runs on other Groups); a ninth simultaneous opener ends the message before
/// it.
///
/// ```
/// use tutti_ump::{Message, Sysex7Packet, SysexStatus, Ump, spans};
///
/// let packet = |status, data: &[u8]| {
///     Ump::from(Message::Sysex7(
///         tutti_ump::MidiGroup::FIRST,
///         Sysex7Packet::new(status, data).unwrap(),
///     ))
/// };
/// let batch = [
///     packet(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
///     packet(SysexStatus::End, &[7]),
///     packet(SysexStatus::Complete, &[8]),
/// ];
/// let lens: Vec<_> = spans(&batch, |ump| *ump).map(|span| span.len()).collect();
/// assert_eq!(lens, [2, 1]);
/// ```
pub fn spans<T, F: Fn(&T) -> Ump>(items: &[T], ump: F) -> Spans<'_, T, F, fn(&T) -> Join> {
    spans_joined(items, ump, |_| Join::Alone)
}

/// [`spans`] for items that say whether they go with the one before them
/// ([`Join`]): `join` reads it. The same limit of 8 simultaneously open runs
/// holds: a ninth opener ends the message before it.
pub fn spans_joined<T, F: Fn(&T) -> Ump, J: Fn(&T) -> Join>(
    items: &[T],
    ump: F,
    join: J,
) -> Spans<'_, T, F, J> {
    Spans {
        rest: items,
        ump,
        join,
    }
}

/// The iterator of [`spans`].
#[derive(Clone, Debug)]
pub struct Spans<'a, T, F, J> {
    rest: &'a [T],
    ump: F,
    join: J,
}

impl<'a, T, F: Fn(&T) -> Ump, J: Fn(&T) -> Join> Iterator for Spans<'a, T, F, J> {
    type Item = PacketSpan<'a, T>;

    fn next(&mut self) -> Option<PacketSpan<'a, T>> {
        let len = message_len(self.rest, &self.ump, &self.join);
        if len == 0 {
            return None;
        }
        let (message, rest) = self.rest.split_at(len);
        self.rest = rest;
        Some(PacketSpan(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ManufacturerId, MdsChunk, MdsChunkSize, MdsChunks, MdsIdentity, MixedDataHeader,
        Sysex7Packet, Sysex8Packet, U4,
    };

    const G: MidiGroup = MidiGroup::FIRST;

    fn sysex7(status: SysexStatus) -> Ump {
        Ump::from(Message::Sysex7(
            G,
            Sysex7Packet::new(status, &[1, 2]).unwrap(),
        ))
    }

    fn sysex8(status: SysexStatus, stream: u8) -> Ump {
        Ump::from(Message::Data128(
            G,
            Data128::Sysex8(Sysex8Packet::new(status, Sysex8StreamId::new(stream), &[1]).unwrap()),
        ))
    }

    fn name(at: Form) -> Ump {
        Ump::from(Message::Stream(Stream::EndpointName(at, [b'a'; 14])))
    }

    fn note() -> Ump {
        Ump([0x4090_3C00, 0x8000_0000, 0, 0])
    }

    fn lens(batch: &[Ump]) -> Vec<usize> {
        spans(batch, |ump| *ump).map(PacketSpan::len).collect()
    }

    #[test]
    fn a_sysex7_run_from_start_to_end_is_one_message() {
        use SysexStatus::{Continue, End, Start};
        let batch = [
            sysex7(Start),
            sysex7(Continue),
            sysex7(Continue),
            sysex7(End),
            note(),
        ];
        assert_eq!(lens(&batch), [4, 1]);
    }

    #[test]
    fn packets_joined_to_the_one_before_are_its_message() {
        let batch = [
            (note(), Join::Alone),
            (note(), Join::ToPrevious),
            (note(), Join::ToPrevious),
            (note(), Join::Alone),
        ];
        let lens: Vec<_> = spans_joined(&batch, |p| p.0, |p| p.1)
            .map(PacketSpan::len)
            .collect();
        assert_eq!(lens, [3, 1]);
    }

    #[test]
    fn a_complete_packet_is_a_message_of_one() {
        let batch = [sysex7(SysexStatus::Complete), note(), note()];
        assert_eq!(lens(&batch), [1, 1, 1]);
    }

    #[test]
    fn a_run_the_batch_ends_inside_ends_with_the_batch() {
        use SysexStatus::{Continue, Start};
        assert_eq!(lens(&[note(), sysex7(Start), sysex7(Continue)]), [1, 2]);
    }

    #[test]
    fn a_run_another_message_interrupts_ends_before_it() {
        use SysexStatus::{Continue, Start};
        let batch = [sysex7(Start), sysex7(Continue), note(), sysex7(Continue)];
        assert_eq!(lens(&batch), [2, 1, 1]);
    }

    #[test]
    fn a_continuation_or_end_with_no_start_is_a_message_of_one() {
        use SysexStatus::{Continue, End};
        assert_eq!(lens(&[sysex7(Continue), sysex7(End)]), [1, 1]);
    }

    #[test]
    fn interleaved_sysex8_streams_are_one_message_until_both_close() {
        use SysexStatus::{End, Start};
        let batch = [
            sysex8(Start, 1),
            sysex8(Start, 2),
            sysex8(End, 2),
            sysex8(End, 1),
            note(),
        ];
        assert_eq!(lens(&batch), [4, 1]);
        let one_left_open = [sysex8(Start, 1), sysex8(Start, 2), sysex8(End, 2)];
        assert_eq!(lens(&one_left_open), [3], "the batch ends inside stream 1");
    }

    #[test]
    fn a_real_time_message_inside_a_sysex7_run_belongs_to_it() {
        use SysexStatus::{End, Start};
        let clock = Ump::from(Message::System(
            MidiGroup::FIRST,
            crate::System::TimingClock,
        ));
        let batch = [sysex7(Start), clock, sysex7(End), clock];
        assert_eq!(lens(&batch), [3, 1]);
    }

    #[test]
    fn a_packet_of_another_group_inside_a_run_belongs_to_it() {
        use SysexStatus::{End, Start};
        let elsewhere = Ump([0x4191_3C00, 0x8000_0000, 0, 0]);
        let batch = [sysex7(Start), elsewhere, sysex7(End)];
        assert_eq!(lens(&batch), [3]);
        let same_group = note();
        assert_eq!(lens(&[sysex7(Start), same_group, sysex7(End)]), [1, 1, 1]);
    }

    #[test]
    fn midi_time_code_on_the_runs_group_ends_it_but_real_time_does_not() {
        use SysexStatus::{End, Start};
        let mtc = Ump::from(Message::System(
            MidiGroup::FIRST,
            crate::System::TimeCode(crate::U7::of(0x21)),
        ));
        assert_eq!(lens(&[sysex7(Start), mtc, sysex7(End)]), [1, 1, 1]);
    }

    #[test]
    fn a_ump_stream_message_inside_a_run_belongs_to_it() {
        use SysexStatus::{End, Start};
        let stream = Ump::from(Message::Stream(Stream::StartOfClip));
        assert_eq!(lens(&[sysex7(Start), stream, sysex7(End)]), [3]);
    }

    #[test]
    fn a_ninth_open_run_ends_the_message_before_it() {
        use SysexStatus::Start;
        let batch: Vec<_> = (0..9).map(|stream| sysex8(Start, stream)).collect();
        assert_eq!(lens(&batch), [8, 1]);
    }

    #[test]
    fn a_jr_clock_inside_a_flex_text_belongs_to_it() {
        let text = |form| {
            Ump::from(Message::Flex(
                G,
                Flex::Text {
                    address: FlexAddress::Group,
                    kind: FlexTextKind::Lyrics,
                    form,
                    bytes: [b'a'; 12],
                },
            ))
        };
        let clock = Ump::from(Message::Utility(crate::Utility::JrClock(9)));
        assert_eq!(lens(&[text(Form::Start), clock, text(Form::End)]), [3]);
    }

    #[test]
    fn a_sysex8_run_is_closed_by_its_own_stream() {
        use SysexStatus::{Continue, End, Start};
        let batch = [sysex8(Start, 7), sysex8(Continue, 7), sysex8(End, 7)];
        assert_eq!(lens(&batch), [3]);
    }

    #[test]
    fn a_utility_packet_inside_a_run_belongs_to_it_and_one_after_it_does_not() {
        use SysexStatus::{End, Start};
        let stamp = Ump::from(Message::Utility(crate::Utility::JrTimestamp(5)));
        let batch = [sysex7(Start), stamp, sysex7(End), stamp, note()];
        assert_eq!(lens(&batch), [3, 1, 1]);
    }

    #[test]
    fn a_stream_text_is_one_message_from_start_to_end() {
        let batch = [
            name(Form::Start),
            name(Form::Continue),
            name(Form::End),
            name(Form::Complete),
        ];
        assert_eq!(lens(&batch), [3, 1]);
    }

    #[test]
    fn a_flex_text_is_one_message_from_start_to_end() {
        let text = |form| {
            Ump::from(Message::Flex(
                G,
                Flex::Text {
                    address: FlexAddress::Group,
                    kind: FlexTextKind::Lyrics,
                    form,
                    bytes: [b'a'; 12],
                },
            ))
        };
        assert_eq!(
            lens(&[text(Form::Start), text(Form::End), text(Form::Complete)]),
            [2, 1]
        );
    }

    #[test]
    fn a_mixed_data_chunk_is_its_header_and_the_payloads_it_declares() {
        let header = Ump::from(Message::Data128(
            G,
            Data128::MixedDataHeader(MixedDataHeader {
                mds_id: U4::of(3),
                chunk_size: MdsChunkSize::from_data_len(30).unwrap(),
                chunks: MdsChunks::new(1),
                chunk: MdsChunk::new(1),
                identity: MdsIdentity {
                    manufacturer: ManufacturerId::new(0x41).unwrap(),
                    device: 0,
                    sub_id_1: 0,
                    sub_id_2: 0,
                },
            }),
        ));
        let payload = Ump::from(Message::Data128(
            G,
            Data128::MixedDataPayload {
                mds_id: U4::of(3),
                bytes: [0; 14],
            },
        ));
        // 30 data bytes are 3 payload packets.
        let batch = [header, payload, payload, payload, payload, note()];
        assert_eq!(lens(&batch), [4, 1, 1]);
        assert_eq!(
            lens(&batch[..3]),
            [3],
            "a chunk the batch ends inside ends with it"
        );
    }
}
