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
//! continuation or an end with no start before it. A Utility packet (a JR
//! Timestamp or Clock) inside a message of several packets belongs to it, so a
//! cut never falls between a packet and the timestamp that precedes the next.

use crate::{
    Data128, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockId, Message, MidiGroup, Stream,
    SysexStatus, Ump,
};

/// Which message of several packets a packet belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Sysex7(MidiGroup),
    Sysex8(MidiGroup, u8),
    FlexText(MidiGroup, FlexAddress, FlexTextKind),
    EndpointName,
    ProductInstanceId,
    BlockName(FunctionBlockId),
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

/// How many of the first packets of `items` are one message.
fn message_len<T>(items: &[T], ump: &impl Fn(&T) -> Ump) -> usize {
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
            // The last packet known to belong to the message; a Utility
            // packet is held with it only if another of its packets follows.
            let mut last = 0;
            for (i, item) in items.iter().enumerate().skip(1) {
                let packet = ump(item);
                if packet.message_type() == 0 {
                    continue;
                }
                match part(packet) {
                    Part::Inside(of) if of == run => last = i,
                    Part::Closes(of) if of == run => return i + 1,
                    _ => break,
                }
            }
            last + 1
        }
    }
}

/// The packets of one message of a batch: what a cut keeps together.
#[derive(Debug, PartialEq, Eq)]
pub struct Span<'a, T>(&'a [T]);

impl<T> Clone for Span<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Span<'_, T> {}

impl<'a, T> Span<'a, T> {
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

/// The messages of `items`, in order, each a [`Span`] of its packets. `ump`
/// reads the packet an item carries. A message of several packets whose last
/// the batch never reaches ends where the batch does.
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
pub fn spans<T, F: Fn(&T) -> Ump>(items: &[T], ump: F) -> Spans<'_, T, F> {
    Spans { rest: items, ump }
}

/// The iterator of [`spans`].
#[derive(Clone, Debug)]
pub struct Spans<'a, T, F> {
    rest: &'a [T],
    ump: F,
}

impl<'a, T, F: Fn(&T) -> Ump> Iterator for Spans<'a, T, F> {
    type Item = Span<'a, T>;

    fn next(&mut self) -> Option<Span<'a, T>> {
        let len = message_len(self.rest, &self.ump);
        if len == 0 {
            return None;
        }
        let (message, rest) = self.rest.split_at(len);
        self.rest = rest;
        Some(Span(message))
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
            Data128::Sysex8(Sysex8Packet::new(status, stream, &[1]).unwrap()),
        ))
    }

    fn name(at: Form) -> Ump {
        Ump::from(Message::Stream(Stream::EndpointName(at, [b'a'; 14])))
    }

    fn note() -> Ump {
        Ump([0x4090_3C00, 0x8000_0000, 0, 0])
    }

    fn lens(batch: &[Ump]) -> Vec<usize> {
        spans(batch, |ump| *ump).map(Span::len).collect()
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
    fn a_sysex8_run_is_not_closed_by_another_stream() {
        use SysexStatus::{End, Start};
        let batch = [
            sysex8(Start, 1),
            sysex8(Start, 2),
            sysex8(End, 2),
            sysex8(End, 1),
        ];
        assert_eq!(lens(&batch), [1, 2, 1]);
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
