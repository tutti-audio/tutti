use tutti_midi_device::{CiMessage, Decoded, Muid};
use tutti_ump::MidiGroup;

/// Decodes `data` as a MIDI-CI payload (the bytes inside a SysEx, from `0x7E`).
///
/// A decoded message re-encodes to a payload that decodes to the same message
/// and encodes to the same bytes (reserved bits and trailing bytes are not
/// kept); a payload that is CI but unreadable has a NAK or none.
pub fn ci_decode(data: &[u8]) {
    match CiMessage::decode(data) {
        Decoded::Message(message) => {
            let _ = (message.header(), message.destination(), message.sub_id2());
            let bytes = message.encode();
            let again = CiMessage::decode(&bytes);
            assert_eq!(
                again,
                Decoded::Message(message.clone()),
                "{message:?} re-encoded to bytes that read differently"
            );
            if let Decoded::Message(again) = again {
                assert_eq!(again.encode(), bytes);
            }
            let _ = message.packets(MidiGroup::FIRST).count();
        }
        Decoded::Unread(unread) => {
            let _ = unread.nak(Muid::of(1));
        }
        Decoded::NotCi => {}
    }
}
