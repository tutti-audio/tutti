use super::*;
use crate::virtual_endpoint::SourceBackend;
use tutti_fixtures::midi;
use tutti_ump::{Message, MidiChannel, Ump, Utility};

#[test]
fn send_jr_timestamp_word() {
    // A JR Timestamp is a single UMP Utility word — the whole point of this
    // endpoint (it has no MIDI 1.0 form). It must send without error.
    let src = UmpVirtualSource::new("Test UMP JR", VirtualProtocol::Midi2).expect("creates");
    let jr = Ump::from(Message::Utility(Utility::JrTimestamp(0x1234)));
    assert!(src.sender().send(&jr).is_ok());
}

#[test]
fn send_multiword_note_on() {
    // A MIDI 2.0 channel-voice note-on is two words.
    let src = UmpVirtualSource::new("Test UMP Note", VirtualProtocol::Midi2).expect("creates");
    let note = midi::note_on(MidiChannel::FIRST, 60, 0x8000);
    assert!(src.sender().send(&note).is_ok());
}
