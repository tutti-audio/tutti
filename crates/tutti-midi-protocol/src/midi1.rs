//! The MIDI 1.0 messages a [`Message`] stands for.

use tutti_ump::{Message, Midi1Bytes, Ump, Voice1, Voice2};

/// The Default Translation; see [`Midi1Translator::with_mode`](crate::Midi1Translator::with_mode) for the alternates.
///
/// The MIDI 1.0 messages `message` stands for, in the order to send them:
/// none when it has no MIDI 1.0 form, three for a MIDI 2.0 Program Change with
/// a bank (CC 0 the bank MSB, CC 32 the LSB, then the Program Change; M2-104
/// Appendix D), one otherwise. Channel voice messages translate as `Voice1::try_from` does.
///
/// The bytes of the wire, for an output that speaks MIDI 1.0 and has no state
/// to keep: a stream that must keep Registered and Assignable Controllers is a
/// [`Midi1Translator`](crate::Midi1Translator). Takes a [`Message`] or the [`Ump`] it is (a
/// packet that does not decode has no MIDI 1.0 form).
pub fn midi1(packet: impl Into<Ump>) -> impl Iterator<Item = Midi1Bytes> {
    let none = None;
    let slots = match packet.into().decode() {
        Ok(message) => slots_of(message),
        Err(_) => [none; 3],
    };
    slots.into_iter().flatten()
}

fn slots_of(message: Message) -> [Option<Midi1Bytes>; 3] {
    let none = None;
    match message {
        Message::Midi1(..) | Message::System(..) => [message.midi1_bytes(), none, none],
        Message::Midi2(
            address,
            Voice2::ProgramChange {
                program,
                bank: Some(bank),
            },
        ) => {
            let [msb, lsb] = bank.bytes(address.channel);
            let program = Voice1::ProgramChange(program).bytes(address.channel);
            [Some(msb), Some(lsb), Some(program)]
        }
        Message::Midi2(address, voice) => [
            Voice1::try_from(voice)
                .ok()
                .map(|voice| voice.bytes(address.channel)),
            none,
            none,
        ],
        Message::Utility(_)
        | Message::Sysex7(..)
        | Message::Data128(..)
        | Message::Flex(..)
        | Message::Stream(_) => [none; 3],
    }
}
