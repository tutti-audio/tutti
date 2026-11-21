//! MIDI 1.0 bytes and the UMP that carries them.

use tutti_midi_protocol::Midi1Translator;
use tutti_rt::{Counter, CounterWriter, counter};
use tutti_ump::{
    Message, Midi1Parser, MidiGroup, SYSEX7_MAX_PAYLOAD, SysexAssembler, SysexMessage, SysexStep,
    Ump, Utility, parse_exact,
};

/// MIDI 1.0 System Exclusive bytes read as SysEx7 packets (type 3) on the first
/// Group: a port is one cable. The input node reassembles the packets.
#[derive(Clone, Debug)]
pub(in crate::backend) struct ByteIn {
    parser: Midi1Parser,
}

impl ByteIn {
    /// A reader at the start of a stream.
    pub(in crate::backend) fn new() -> Self {
        Self {
            parser: Midi1Parser::new(MidiGroup::FIRST),
        }
    }

    /// The packets of the next bytes of a stream that carries System
    /// Exclusive: a message longer than one buffer continues in the next call.
    pub(in crate::backend) fn long<'a>(
        &'a mut self,
        bytes: &'a [u8],
    ) -> impl Iterator<Item = Ump> + 'a {
        self.parser.feed_all(bytes).map(Ump::from)
    }
}

/// What to put on the wire for a UMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend) enum Wire<'a> {
    /// A MIDI 1.0 channel voice or system message (type 2 or type 1).
    Short(Ump),
    /// A whole System Exclusive message.
    Long(SysexMessage<'a>),
}

/// UMP written as MIDI 1.0 bytes.
///
/// A MIDI 2.0 channel voice message is translated to MIDI 1.0 ([`Midi1Translator`]'s
/// Default Translation: a Registered or Assignable Controller is its four
/// Control Changes, a Program Change with a bank three messages). A message with
/// no MIDI 1.0 form (a Flex Data message, a per-note message) sends nothing and
/// is counted ([`new`](Self::new)); a NOOP carries nothing and is not. SysEx7
/// packets are joined into one message, which is sent when its last packet
/// arrives.
#[derive(Debug)]
pub(in crate::backend) struct ByteOut {
    midi1_translator: Midi1Translator,
    sysex: SysexAssembler<Box<[u8]>>,
    /// The messages that had no MIDI 1.0 form.
    unrepresentable: CounterWriter<u64>,
}

/// Whether `ump` is the Utility NOOP, which says nothing to send.
fn is_noop(ump: Ump) -> bool {
    matches!(ump.decode(), Ok(Message::Utility(Utility::Noop)))
}

/// The longest message sent: the payload tutti carries, with its delimiters.
const MAX_MESSAGE: usize = SYSEX7_MAX_PAYLOAD + 2;

impl ByteOut {
    /// A writer with no message open, and the count of the messages it was
    /// given that have no MIDI 1.0 form.
    pub(in crate::backend) fn new() -> (Self, Counter<u64>) {
        // One lane, one packet at a time: each message is sent before the next
        // packet is pushed.
        let room = SysexAssembler::<()>::room_for(MAX_MESSAGE, 1, 1);
        let (unrepresentable, count) = counter();
        let out = Self {
            midi1_translator: Midi1Translator::new(),
            sysex: SysexAssembler::new(vec![0; room].into_boxed_slice(), MAX_MESSAGE, 1),
            unrepresentable,
        };
        (out, count)
    }

    /// Hands `send` what `ump` puts on the wire: nothing for the packet of a
    /// message not yet whole, one message for a MIDI 1.0 message or the packet
    /// that completes a SysEx, up to four for a translated MIDI 2.0 message.
    ///
    /// Stops at the first `Err` `send` returns.
    pub(in crate::backend) fn encode<E>(
        &mut self,
        ump: Ump,
        mut send: impl FnMut(Wire<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.sysex.begin_block();
        match self.sysex.push(ump) {
            SysexStep::Held => Ok(()),
            SysexStep::Complete(done) => send(Wire::Long(self.sysex.message(done))),
            SysexStep::NotSysex => {
                let mut sent = Ok(());
                let mut made = false;
                self.midi1_translator.feed(ump, |bytes| {
                    // The translated bytes are one MIDI 1.0 message, which
                    // `parse_exact` reads back as its type 2 or type 1 UMP.
                    if sent.is_ok()
                        && let Some(message) = parse_exact(MidiGroup::FIRST, bytes.as_slice())
                    {
                        made = true;
                        sent = send(Wire::Short(Ump::from(message)));
                    }
                });
                if !made && sent.is_ok() && !is_noop(ump) {
                    self.unrepresentable.add(1);
                }
                sent
            }
        }
    }
}
