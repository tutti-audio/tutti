use super::*;
use tutti_fixtures::midi;
use tutti_graph::EventInletNode;
use tutti_graph::EventKind;
use tutti_graph::contract::Direct;

use tutti_midi_protocol::{
    GeneralMidiMode, MmcCommands, MmcPosition, MmcSubframes, MmcTimeLabel, MtcRate, MtcTimeCode,
    Recipient, SysexDeviceId, TimecodeHours, TimecodeMinutes, TimecodeSeconds, Universal,
};
use tutti_ump::{Message, MidiGroup, NoteVelocity, SysexStatus, U7, U14, Ump};
use tutti_units::{Frame, SampleRate};

fn rig() -> (Direct<EventInletNode>, MidiKeys) {
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    (
        Direct::new(node, SampleRate::SR_48K, 64),
        MidiKeys::new(writer, MidiGroup::FIRST),
    )
}

fn sent(d: &Direct<EventInletNode>) -> Vec<Ump> {
    let out = tutti_graph::Node::<f32>::shape(&*d.node)
        .event_output(0)
        .expect("an event output");
    d.event_output(out)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(ump) => ump,
            _ => unreachable!(),
        })
        .collect()
}

/// The `note_on` / `note_off` helpers build the same packets the explicit
/// constructors do, and they come out of the inlet's port on the next block.
#[test]
fn the_key_helpers_match_explicit_events() {
    let (mut d, mut keys) = rig();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(NoteVelocity::from(U7::of(100)).get()),
    )
    .unwrap();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_on(
                MidiChannel::FIRST,
                60,
                NoteVelocity::from(U7::of(100)).get()
            ),
            midi::note_off(MidiChannel::FIRST, 60, 0),
        ]
    );
}

/// A Note On at velocity 0 is a MIDI 2.0 Note On (M2-104 §7.4.2), not a Note
/// Off.
#[test]
fn a_note_on_at_velocity_zero_stays_a_note_on() {
    let (mut d, mut keys) = rig();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::MIN,
    )
    .unwrap();
    d.block();
    assert_eq!(sent(&d), [midi::note_on(MidiChannel::FIRST, 60, 0)]);
}

/// A full ring refuses a helper's event with `Full` (the writer's own rule,
/// reached through the helper), so a note stuck for want of room is reported.
#[test]
fn a_full_ring_refuses_through_the_helper() {
    let (_d, mut keys) = rig();
    for _ in 0..LIVE_INLET_CAPACITY.slots() {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(1),
        )
        .unwrap();
    }
    assert_eq!(
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(PushError::Full)
    );
    assert_eq!(keys.counts().losses().lost, 1);
}

/// The helpers send on the Group the keys were made for, not always the first.
#[test]
fn the_helpers_send_on_the_keys_group() {
    let group = MidiGroup::new(2).expect("in range");
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let mut keys = MidiKeys::new(writer, group);
    assert_eq!(keys.group(), group);
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(5),
    )
    .unwrap();
    keys.note_off(Frame::new(0), MidiChannel::FIRST, NoteNumber::of(60))
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_on(tutti_ump::Address::new(group, MidiChannel::FIRST), 60, 5),
            midi::note_off(tutti_ump::Address::new(group, MidiChannel::FIRST), 60, 0),
        ]
    );
}

/// What the inlet's port carried, as `(offset, ump)`.
fn sent_at(d: &Direct<EventInletNode>) -> Vec<(u32, Ump)> {
    let out = tutti_graph::Node::<f32>::shape(&*d.node)
        .event_output(0)
        .expect("an event output");
    d.event_output(out)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(ump) => (e.offset.get(), ump),
            _ => unreachable!(),
        })
        .collect()
}

/// The payload bytes (between `F0` and `F7`) of the one SysEx message `umps`
/// are, checking that the packets run Start, Continue.., End.
fn payload_of(umps: &[Ump]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (i, ump) in umps.iter().enumerate() {
        let Ok(Message::Sysex7(group, packet)) = ump.decode() else {
            panic!("not a SysEx7 packet: {ump:?}");
        };
        assert_eq!(group, MidiGroup::FIRST);
        let status = packet.status();
        let (first, last) = (i == 0, i + 1 == umps.len());
        let expected = match (first, last) {
            (true, true) => SysexStatus::Complete,
            (true, false) => SysexStatus::Start,
            (false, true) => SysexStatus::End,
            (false, false) => SysexStatus::Continue,
        };
        assert_eq!(status, expected, "packet {i}");
        bytes.extend(packet.bytes().iter().map(|b| b.get()));
    }
    bytes
}

fn addressed(to: Recipient, message: Universal) -> Addressed {
    Addressed { to, message }
}

/// MMC Locate (RP-013 p.8, M2-104 section 7.7): `F0 7F dev 06 44 06 01 hr mn sc fr sf F7`, the
/// target time code of 01:02:03:05 at 25 fps with 25 subframes, the hours byte
/// carrying the rate code `01` above the hours.
#[test]
fn an_mmc_locate_is_the_spec_bytes() {
    let (mut d, mut keys) = rig();
    let position = MmcPosition {
        label: MmcTimeLabel::new(
            MtcTimeCode::new(
                MtcRate::Fps25,
                TimecodeHours::of(1),
                TimecodeMinutes::of(2),
                TimecodeSeconds::of(3),
                5,
            )
            .unwrap(),
        ),
        subframes: MmcSubframes::of(25),
    };
    keys.send_universal(
        Stamp::NextBlock,
        addressed(
            Recipient::Device(SysexDeviceId::of(5)),
            Universal::MachineControl(MmcCommands::locate(position, false)),
        ),
    )
    .unwrap();
    d.block();
    let umps: Vec<Ump> = sent(&d);
    let mut wire = vec![0xF0];
    wire.extend(payload_of(&umps));
    wire.push(0xF7);
    assert_eq!(
        wire,
        [
            0xF0, 0x7F, 0x05, 0x06, 0x44, 0x06, 0x01, 0x21, 0x02, 0x03, 0x05, 0x19, 0xF7
        ]
    );
}

/// The all-call is device `7F` (M1 p.40), and General MIDI System On is
/// Non-Real Time `09 01` (M1 p.52).
#[test]
fn an_all_call_addresses_device_7f() {
    let (mut d, mut keys) = rig();
    keys.send_universal(
        Stamp::NextBlock,
        addressed(
            Recipient::AllCall,
            Universal::GeneralMidi(GeneralMidiMode::On),
        ),
    )
    .unwrap();
    d.block();
    assert_eq!(payload_of(&sent(&d)), [0x7E, 0x7F, 0x09, 0x01]);
}

/// A message the ring has no room for whole is refused with `Full` and not a
/// packet of it is written, however near the ring is to taking them; one that
/// fits exactly is sent. The refusal is not a loss.
#[test]
fn a_full_refusal_writes_no_packet() {
    let (mut d, mut keys) = rig();
    let message = addressed(
        Recipient::AllCall,
        Universal::MtcFullMessage(
            tutti_midi_protocol::MtcFullMessage::new(
                MtcRate::Fps30,
                TimecodeHours::of(0),
                TimecodeMinutes::of(0),
                TimecodeSeconds::of(1),
                2,
            )
            .unwrap(),
        ),
    );
    // MTC Full is eight bytes: two packets. Leave room for one.
    while keys.vacant() > 1 {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(1),
        )
        .unwrap();
    }
    assert_eq!(
        keys.send_universal(Stamp::NextBlock, message),
        Err(PushError::Full)
    );
    assert_eq!(keys.vacant(), 1);
    assert_eq!(keys.losses().lost, 0);
    d.block();
    assert_eq!(
        sent(&d).len(),
        LIVE_INLET_CAPACITY.slots() - 1,
        "only the notes"
    );
}

/// The room is counted in packets: a message of exactly the vacant room is
/// sent, whole.
#[test]
fn a_message_that_fills_the_ring_exactly_is_sent() {
    let (mut d, mut keys) = rig();
    while keys.vacant() > 2 {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(1),
        )
        .unwrap();
    }
    keys.send_universal(
        Stamp::NextBlock,
        addressed(
            Recipient::AllCall,
            Universal::MtcFullMessage(
                tutti_midi_protocol::MtcFullMessage::new(
                    MtcRate::Fps30,
                    TimecodeHours::of(0),
                    TimecodeMinutes::of(0),
                    TimecodeSeconds::of(1),
                    2,
                )
                .unwrap(),
            ),
        ),
    )
    .unwrap();
    assert_eq!(keys.vacant(), 0);
    d.block();
    let all = sent(&d);
    // The two packets and the notes fill the ring.
    assert_eq!(all.len(), LIVE_INLET_CAPACITY.slots());
}

/// A message of more packets than the room is refused whole: a Single Note
/// Tuning Change of 127 changes is 515 bytes, 86 packets, against 80 vacant.
#[test]
fn a_message_longer_than_the_room_is_refused_whole() {
    let (_d, mut keys) = rig();
    let mut changes = tutti_midi_protocol::KeyChanges::new();
    for key in 0..127 {
        assert!(changes.push(tutti_midi_protocol::KeyChange {
            key: NoteNumber::of(key),
            frequency: tutti_midi_protocol::TuningFrequency::from_parts(U7::of(key), U14::of(0)),
        }));
    }
    let message = addressed(
        Recipient::AllCall,
        Universal::Tuning(tutti_midi_protocol::TuningMessage::RealTimeNoteChange {
            bank: Some(U7::of(1)),
            program: U7::of(2),
            changes,
        }),
    );
    while keys.vacant() > 80 {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(1),
        )
        .unwrap();
    }
    let before = keys.vacant();
    assert_eq!(
        keys.send_universal(Stamp::NextBlock, message),
        Err(PushError::Full)
    );
    assert_eq!(keys.vacant(), before);
}

/// Every packet of a message lands on the frame it was stamped for, in order.
#[test]
fn a_frame_stamp_lands_every_packet_on_that_frame() {
    let (mut d, mut keys) = rig();
    let full = tutti_midi_protocol::MtcFullMessage::new(
        MtcRate::Fps30,
        TimecodeHours::of(0),
        TimecodeMinutes::of(0),
        TimecodeSeconds::of(1),
        2,
    )
    .unwrap();
    keys.send_universal(
        Frame::new(10),
        addressed(Recipient::AllCall, Universal::MtcFullMessage(full)),
    )
    .unwrap();
    d.block();
    let at = sent_at(&d);
    assert_eq!(at.len(), 2);
    assert!(at.iter().all(|(offset, _)| *offset == 10), "{at:?}");
    let umps: Vec<Ump> = at.iter().map(|(_, ump)| *ump).collect();
    assert_eq!(payload_of(&umps), full.payload());
}

/// Dropped keys release what they hold: each note `note_on` started and
/// `note_off` did not end gets its Note Off on the next block, and an ended note
/// gets none.
#[test]
fn dropping_the_keys_ends_each_note_they_hold() {
    let (mut d, mut keys) = rig();
    for note in [60, 64, 67] {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::of(2),
            NoteNumber::of(note),
            NoteVelocity::new(0x8000),
        )
        .unwrap();
    }
    keys.note_off(Stamp::NextBlock, MidiChannel::of(2), NoteNumber::of(64))
        .unwrap();
    d.block();
    drop(keys);
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_off(MidiChannel::of(2), 60, 0),
            midi::note_off(MidiChannel::of(2), 67, 0),
        ]
    );
}

/// A Note Off the ring refused leaves the note held, so dropping the keys ends it.
#[test]
fn a_refused_note_off_is_sent_when_the_keys_drop() {
    let (node, writer) = EventInletNode::pair(EventCapacity::of(1));
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(0x8000),
    )
    .unwrap();
    assert_eq!(
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(PushError::Full)
    );
    d.block();
    drop(keys);
    d.block();
    assert_eq!(sent(&d), [midi::note_off(MidiChannel::FIRST, 60, 0)]);
}
