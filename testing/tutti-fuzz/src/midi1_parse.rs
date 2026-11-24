use tutti_midi_protocol::read;
use tutti_ump::{Message, Midi1Parser, MidiGroup, SysexMessage, Ump, parse_exact};

/// Feeds `data` as a MIDI 1.0 byte stream to the stream parser and as one
/// message to the stateless readers.
///
/// Every message the parser yields re-encodes to a packet that decodes to
/// itself, and a framed SysEx reads as a Universal one or as none.
pub(crate) fn midi1_parse(data: &[u8]) {
    let group = MidiGroup::FIRST;
    let mut parser = Midi1Parser::new(group);
    for (n, &byte) in data.iter().enumerate() {
        if n % 7 == 6 {
            parser.end_of_message();
        }
        if let Some(message) = parser.feed(byte) {
            let ump = tutti_ump::Ump::from(message);
            assert_eq!(ump.decode(), Ok(message), "{message:?}");
        }
    }
    let _: Vec<Message> = Midi1Parser::new(group).feed_all(data).collect();
    let _ = parse_exact(group, data);
    let _ = Ump::from_midi1_bytes(group, data);
    if let Ok(message) = SysexMessage::from_framed(data) {
        let _ = read(message);
    }
}
