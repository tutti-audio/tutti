//! MIDI 1.0 bytes and the UMP that carries them.

use tutti_ump::{
    Midi1Parser, MidiGroup, SYSEX7_MAX_PAYLOAD, SysexAssembler, SysexMessage, SysexStep, Ump,
};

/// MIDI 1.0 System Exclusive bytes read as SysEx7 packets (type 3) on the first
/// Group: a port is one cable. The ingest node reassembles the packets.
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
/// The UMP are already MIDI 1.0 (the output narrowed them for a MIDI 1.0
/// endpoint). A message of any other type (a Flex Data or a Utility message)
/// is consumed and sends nothing. SysEx7 packets are joined into one
/// message, which is sent when its last packet arrives.
#[derive(Debug)]
pub(in crate::backend) struct ByteOut {
    sysex: SysexAssembler<Box<[u8]>>,
}

/// The longest message sent: the payload tutti carries, with its delimiters.
const MAX_MESSAGE: usize = SYSEX7_MAX_PAYLOAD + 2;

impl ByteOut {
    /// A writer with no message open.
    pub(in crate::backend) fn new() -> Self {
        // One lane, one packet at a time: each message is sent before the next
        // packet is pushed.
        let room = SysexAssembler::<()>::room_for(MAX_MESSAGE, 1, 1);
        Self {
            sysex: SysexAssembler::new(vec![0; room].into_boxed_slice(), MAX_MESSAGE, 1),
        }
    }

    /// Hands `send` what `ump` puts on the wire: nothing for the packet of a
    /// message not yet whole, one message for a short message or the packet
    /// that completes a SysEx. Stops at the first `Err` `send` returns.
    pub(in crate::backend) fn encode<E>(
        &mut self,
        ump: Ump,
        mut send: impl FnMut(Wire<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.sysex.begin_block();
        match self.sysex.push(ump) {
            SysexStep::Held => Ok(()),
            SysexStep::Complete(done) => send(Wire::Long(self.sysex.message(done))),
            SysexStep::NotSysex if matches!(ump.message_type(), 1 | 2) => send(Wire::Short(ump)),
            SysexStep::NotSysex => Ok(()),
        }
    }
}
