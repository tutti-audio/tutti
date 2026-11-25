//! The `MpeSender` against M2-120 sections 3 to 5, with figures written here from
//! the spec. Channels are indices: Ch1 is 0.

use tutti_midi_protocol::{Midi1Translator, PitchBendSensitivity, ProfileZone, ProfileZones};
use tutti_ump::{
    Address, Attribute, Bank, BendValue, CCNumber, ControlValue, Message, MidiChannel, MidiGroup,
    MpeChannels, MpeZoneSpan, NoteNumber, NoteVelocity, PerNoteManagement, Pitch,
    RegisteredPerNoteController, Rpn, U7, U14, Ump, Voice1, Voice2,
};
use tutti_voicing::MpeSender;

const CENTER: u32 = 0x8000_0000;

/// A zone whose Manager is `manager` and that has `members` Members, at the
/// Profile's 48 Hundred Cent Units (section 3.5).
fn zone(manager: u8, members: u8) -> ProfileZone {
    let channels = MpeChannels::of(members + 1);
    let span = MpeZoneSpan::new(MidiChannel::of(manager), channels).unwrap();
    ProfileZone::new(span, PitchBendSensitivity::MPE_PROFILE_DEFAULT)
}

/// A sender with one zone: the Manager Ch1 and `members` Members from Ch2.
fn sender(members: u8) -> MpeSender {
    let mut sender = MpeSender::new();
    sender.set_profile(
        MidiGroup::FIRST,
        Some(ProfileZones::single(zone(0, members))),
    );
    sender
}

/// One with its first note played and ended, so the range is sent.
fn started(members: u8) -> MpeSender {
    let mut sender = sender(members);
    feed(&mut sender, 0, note_on(0));
    feed(&mut sender, 0, note_off(0));
    sender
}

fn feed_message(sender: &mut MpeSender, message: Message) -> Vec<(u8, Voice2)> {
    let mut out = Vec::new();
    sender.feed(message.into(), |ump| match ump.decode() {
        Ok(Message::Midi2(address, voice)) => out.push((address.channel.get(), voice)),
        other => panic!("not MIDI 2.0 voice: {other:?}"),
    });
    out
}

fn feed(sender: &mut MpeSender, channel: u8, voice: Voice2) -> Vec<(u8, Voice2)> {
    feed_message(
        sender,
        Message::Midi2(Address::first(MidiChannel::of(channel)), voice),
    )
}

/// The Note Ons and Note Offs of `sent`.
fn notes(sent: &[(u8, Voice2)]) -> Vec<(u8, Voice2)> {
    let is_note = |v: &Voice2| matches!(v, Voice2::NoteOn { .. } | Voice2::NoteOff { .. });
    sent.iter().copied().filter(|(_, v)| is_note(v)).collect()
}

fn note_on(key: u8) -> Voice2 {
    Voice2::NoteOn {
        note: NoteNumber::of(key),
        velocity: NoteVelocity::new(0x8000),
        attribute: Attribute::None,
    }
}

fn note_off(key: u8) -> Voice2 {
    Voice2::note_off(NoteNumber::of(key), NoteVelocity::new(0))
}

fn bend(value: u32) -> Voice2 {
    Voice2::PitchBend(BendValue::new(value))
}

fn note_bend(key: u8, value: u32) -> Voice2 {
    Voice2::PerNotePitchBend {
        note: NoteNumber::of(key),
        value: BendValue::new(value),
    }
}

fn pressure(value: u32) -> Voice2 {
    Voice2::ChannelPressure(ControlValue::new(value))
}

fn note_pressure(key: u8, value: u32) -> Voice2 {
    Voice2::PolyPressure {
        note: NoteNumber::of(key),
        value: ControlValue::new(value),
    }
}

fn cc(controller: u8, value: u32) -> Voice2 {
    Voice2::ControlChange {
        controller: CCNumber::of(controller),
        value: ControlValue::new(value),
    }
}

fn slide(value: u32) -> Voice2 {
    cc(74, value)
}

fn note_slide(key: u8, value: u32) -> Voice2 {
    Voice2::AssignablePerNote {
        note: NoteNumber::of(key),
        controller: 74,
        value: ControlValue::new(value),
    }
}

/// Pitch Bend Sensitivity (RPN 0/0), whole semitones in the top 7 bits.
fn bend_range(semitones: u32) -> Voice2 {
    Voice2::registered(
        Rpn::PITCH_BEND_SENSITIVITY,
        ControlValue::new(semitones << 25),
    )
}

/// Sensitivity of Per-Note Pitch Bend (RPN 0/7), 7.25 fixed point.
fn note_bend_range(semitones: u32) -> Voice2 {
    Voice2::registered(
        Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
        ControlValue::new(semitones << 25),
    )
}

/// The bend `semitones` up is over a range of `range`: the centre plus that part
/// of the half range above it (M2-104 section 7.4.11).
fn bend_of(semitones: f64, range: f64) -> f64 {
    f64::from(CENTER) + semitones / range * f64::from(0x7FFF_FFFF)
}

fn assert_bend(sent: &[(u8, Voice2)], channel: u8, want: f64) {
    let [(at, Voice2::PitchBend(value))] = sent else {
        panic!("one Pitch Bend: {sent:?}");
    };
    assert_eq!(*at, channel);
    // One step of the receiver's 14-bit bend is 2^18 of these.
    let off = (f64::from(value.get()) - want).abs();
    assert!(off < 4.0, "{:#x} is {off} from {want}", value.get());
}

/// **Section 5: the initial values go before the Note On**, bend, third
/// dimension and pressure, and the Manager is sent the range (section 3.5:
/// 48, a whole number of units) before the first. A later note is sent only what
/// its Member does not hold.
/// Spec: m2-120/5#1, m2-120/3.5#5, m2-120/3.5#7
#[test]
fn initial_values_are_sent_before_the_note_on_and_once() {
    let mut s = sender(1);
    assert_eq!(
        feed(&mut s, 0, note_on(60)),
        [
            (0, bend_range(48)),
            (1, bend(CENTER)),
            (1, slide(CENTER)),
            (1, pressure(0)),
            (1, note_on(60)),
        ]
    );
    assert_eq!(feed(&mut s, 0, note_off(60)), [(1, note_off(60))]);
    assert_eq!(feed(&mut s, 0, note_on(62)), [(1, note_on(62))]);
}

/// **Each note has its own Member until none is free**, and a freed Member is
/// taken again after the one idle longest.
/// Spec: m2-120/3.6.1#1, m2-104/d.2.8#1, m2-104/d.4.1#1
#[test]
fn each_note_takes_the_member_idle_longest() {
    let mut s = sender(3);
    for (key, member) in [(60, 1), (64, 2)] {
        let sent = feed(&mut s, 0, note_on(key));
        assert_eq!(notes(&sent), [(member, note_on(key))]);
    }
    assert_eq!(feed(&mut s, 0, note_off(60)), [(1, note_off(60))]);
    for (key, member) in [(67, 3), (71, 1)] {
        let sent = feed(&mut s, 0, note_on(key));
        assert_eq!(notes(&sent), [(member, note_on(key))]);
    }
}

/// **With no unoccupied Member a new note shares one with an existing note** and
/// ends none. The section leaves the choice open; the fewest notes, then the one
/// given a note longest ago, is this sender's.
/// Spec: m2-120/3.6.1#2
#[test]
fn a_note_shares_the_least_loaded_member_when_none_is_free() {
    let mut s = started(2);
    let mut play = |key| notes(&feed(&mut s, 0, note_on(key)));
    assert_eq!(play(60), [(2, note_on(60))]);
    assert_eq!(play(64), [(1, note_on(64))]);
    // One each: the one given a note longest ago is Ch3.
    assert_eq!(play(67), [(2, note_on(67))]);
    assert_eq!(play(70), [(1, note_on(70))]);
    // Releasing one of Ch3's two leaves it the least loaded.
    assert_eq!(feed(&mut s, 0, note_off(60)), [(2, note_off(60))]);
    assert_eq!(notes(&feed(&mut s, 0, note_on(72))), [(2, note_on(72))]);
}

/// **A note that joins a Member leaves what the Member holds where it is**, which
/// the note there is playing with (section 3.6.1), and a later change of either
/// note is the Member's.
/// Spec: m2-120/3.6.1#2
#[test]
fn a_note_that_joins_a_member_resets_nothing_on_it() {
    let mut s = started(1);
    feed(&mut s, 0, note_on(60));
    assert_eq!(
        feed(&mut s, 0, note_bend(60, 0xFFFF_FFFF)),
        [(1, bend(0xFFFF_FFFF))]
    );
    assert_eq!(feed(&mut s, 0, note_on(67)), [(1, note_on(67))]);
    assert_eq!(feed(&mut s, 0, note_bend(67, 0)), [(1, bend(0))]);
    // Alone again on it, the next note starts from its own values.
    feed(&mut s, 0, note_off(60));
    feed(&mut s, 0, note_off(67));
    assert_eq!(
        feed(&mut s, 0, note_on(72)),
        [(1, bend(CENTER)), (1, note_on(72))]
    );
}

/// **The same key struck again ends its earlier note** on the same Member, so one
/// Note Off later ends the only one sounding.
#[test]
fn a_key_struck_again_ends_its_note_and_keeps_its_member() {
    let mut s = started(2);
    feed(&mut s, 0, note_on(60));
    assert_eq!(
        feed(&mut s, 0, note_on(60)),
        [(2, note_off(60)), (2, note_on(60))]
    );
    assert_eq!(feed(&mut s, 0, note_off(60)), [(2, note_off(60))]);
    assert!(feed(&mut s, 0, note_off(60)).is_empty(), "none sounds");
}

/// **The same key on two source channels is two notes**, each ended by its own
/// channel's Note Off; and every channel of the zone is a source, its Members
/// too.
#[test]
fn a_key_of_two_source_channels_is_two_notes() {
    let mut s = started(3);
    let first = notes(&feed(&mut s, 0, note_on(60)));
    let second = notes(&feed(&mut s, 2, note_on(60)));
    assert_eq!((first[0].0, second[0].0), (2, 3));
    assert_eq!(feed(&mut s, 2, note_off(60)), [(3, note_off(60))]);
    assert_eq!(feed(&mut s, 0, note_off(60)), [(2, note_off(60))]);
}

/// **A note's own bend, pressure and third dimension are its Member's** Pitch
/// Bend, Channel Pressure and Control Change 74, exactly where the ranges are
/// equal and the channel adds nothing.
/// Spec: m2-120/4.3#1, m2-120/4.4#1, m2-120/4.5#1, m2-120/4.5#2
#[test]
fn per_note_expression_is_the_members() {
    let mut s = started(2);
    feed(&mut s, 0, note_on(60));
    for value in [0, 0x1234_5678, 0xFFFF_FFFF] {
        assert_eq!(feed(&mut s, 0, note_bend(60, value)), [(2, bend(value))]);
    }
    assert_eq!(
        feed(&mut s, 0, note_pressure(60, 0x4000_0000)),
        [(2, pressure(0x4000_0000))]
    );
    assert_eq!(
        feed(&mut s, 0, note_slide(60, 0x1234_5678)),
        [(2, slide(0x1234_5678))]
    );
    // Registered Per-Note Controller 74 is Brightness too (M2-104 Table 22).
    let brightness = Voice2::RegisteredPerNote {
        note: NoteNumber::of(60),
        controller: RegisteredPerNoteController::Brightness,
        value: ControlValue::new(0x2000_0000),
    };
    assert_eq!(feed(&mut s, 0, brightness), [(2, slide(0x2000_0000))]);
    assert!(feed(&mut s, 0, brightness).is_empty(), "it holds the value");
}

/// **What a note number is told before its Note On is sent ahead of it** (M2-104
/// section 7.4.5: Per-Note Controllers apply to later notes of the number, and
/// section 5: initial values before the Note On).
/// Spec: m2-120/5#1
#[test]
fn a_value_told_before_the_note_on_is_sent_ahead_of_it() {
    let mut s = started(1);
    assert!(feed(&mut s, 0, note_bend(60, 0xC000_0000)).is_empty());
    assert!(feed(&mut s, 0, note_pressure(60, 5)).is_empty());
    assert!(feed(&mut s, 0, note_slide(60, 7)).is_empty());
    assert_eq!(
        feed(&mut s, 0, note_on(60)),
        [
            (1, bend(0xC000_0000)),
            (1, slide(7)),
            (1, pressure(5)),
            (1, note_on(60)),
        ]
    );
}

/// **A source channel's Pitch Bend bends its notes and no other channel's**: an
/// octave up over its own range of 12 is a quarter of the zone's 48, on the
/// Members of that channel's notes; it is not sent on the Manager, whose bend
/// moves every note (section 4.2).
#[test]
fn a_source_channels_bend_moves_its_own_notes() {
    let mut s = started(3);
    feed(&mut s, 2, bend_range(12));
    feed(&mut s, 2, note_on(60)); // Ch3
    feed(&mut s, 0, note_on(64)); // Ch4
    let sent = feed(&mut s, 2, bend(0xFFFF_FFFF));
    assert_bend(&sent, 2, bend_of(12.0, 48.0));
    // The note's own bend adds to it: 12 + 24 semitones.
    let sent = feed(&mut s, 2, note_bend(60, 0xC000_0000));
    assert_bend(&sent, 2, bend_of(36.0, 48.0));
    // Past the range it is the range's end.
    let sent = feed(&mut s, 2, note_bend(60, 0xFFFF_FFFF));
    assert_eq!(sent, [(2, bend(0xFFFF_FFFF))]);
}

/// **A source channel's Pressure and third dimension are its notes'**: the
/// greater pressure of the two, and the note's own third dimension where it was
/// told one.
#[test]
fn a_source_channels_pressure_and_third_dimension_are_its_notes() {
    let mut s = started(2);
    feed(&mut s, 0, note_on(60)); // Ch3
    feed(&mut s, 0, note_on(64)); // Ch2
    feed(&mut s, 0, note_pressure(60, 0x6000_0000));
    assert_eq!(
        feed(&mut s, 0, pressure(0x4000_0000)),
        [(1, pressure(0x4000_0000))],
        "the other note holds a greater one"
    );
    feed(&mut s, 0, note_slide(60, 1));
    assert_eq!(feed(&mut s, 0, slide(2)), [(1, slide(2))]);
}

/// **The zone has one range, sent on the Manager, the widest its sources need**:
/// a source channel with a Per-Note Pitch Bend Sensitivity of 96 widens the 48
/// the zone starts with, and the notes that sound are bent over the new range.
/// Spec: m2-120/3.5#2, m2-120/3.5#5, m2-120/3.5#7, m2-120/3.5#9
#[test]
fn a_wider_source_widens_the_zones_range_on_the_manager() {
    let mut s = started(2);
    feed(&mut s, 0, note_on(60)); // Ch3
    feed(&mut s, 0, note_bend(60, 0xC000_0000)); // 24 of 48
    let sent = feed(&mut s, 1, note_bend_range(96));
    assert_eq!(sent[0], (0, bend_range(96)));
    assert_bend(&sent[1..], 2, bend_of(24.0, 96.0));
    // And back, once that source needs no more.
    let sent = feed(&mut s, 1, note_bend_range(48));
    assert_eq!(sent, [(0, bend_range(48)), (2, bend(0xC000_0000))]);
}

/// A narrower source keeps the zone's range, and its notes' bends are scaled to
/// it: half of 24 semitones is a quarter of 48. The range it was told is sent
/// nowhere.
/// Spec: m2-120/3.5#2
#[test]
fn a_narrower_source_is_scaled_to_the_zones_range() {
    let mut s = started(1);
    assert!(feed(&mut s, 0, note_bend_range(24)).is_empty());
    assert!(feed(&mut s, 0, bend_range(7)).is_empty());
    feed(&mut s, 0, note_on(60));
    let sent = feed(&mut s, 0, note_bend(60, 0xC000_0000));
    assert_bend(&sent, 1, bend_of(12.0, 48.0));
}

/// **A Note On's pitch is the Member's bend** (M2-104 section 7.4.1's Pitch 7.25
/// attribute), and the attribute is not sent too; a later Registered Per-Note
/// Controller 3 moves the note.
#[test]
fn a_notes_pitch_is_its_members_bend() {
    let mut s = started(1);
    let q7_25 = |semitones: f64| Pitch::from_q7_25((semitones * f64::from(1u32 << 25)) as u32);
    let on = Voice2::NoteOn {
        note: NoteNumber::of(60),
        velocity: NoteVelocity::new(0x8000),
        attribute: Attribute::Pitch(q7_25(60.5)),
    };
    let sent = feed(&mut s, 0, on);
    assert_eq!(sent[1], (1, note_on(60)));
    assert_bend(&sent[..1], 1, bend_of(0.5, 48.0));
    let moved = Voice2::RegisteredPerNote {
        note: NoteNumber::of(60),
        controller: RegisteredPerNoteController::Pitch,
        value: ControlValue::new(q7_25(64.0).q7_25()),
    };
    let sent = feed(&mut s, 0, moved);
    assert_bend(&sent, 1, bend_of(4.0, 48.0));
}

/// **A detached note takes no more changes, and a reset number's controllers are
/// at their defaults** (M2-104 section 7.4.5): what the number is told next is
/// for its next note.
#[test]
fn per_note_management_detaches_the_note_and_resets_the_number() {
    let manage = |management| Voice2::PerNoteManagement {
        note: NoteNumber::of(60),
        management,
    };
    let mut s = started(2);
    feed(&mut s, 0, note_on(60)); // Ch3
    feed(&mut s, 0, note_bend(60, 0xC000_0000));
    assert!(feed(&mut s, 0, manage(PerNoteManagement::DetachAndReset)).is_empty());
    assert!(
        feed(&mut s, 0, note_bend(60, 1)).is_empty(),
        "it is detached"
    );
    // A reset of an attached note is heard at once.
    feed(&mut s, 0, note_off(60));
    feed(&mut s, 0, note_on(60)); // Ch2, with the bend of 1
    assert_eq!(
        feed(&mut s, 0, manage(PerNoteManagement::Reset)),
        [(1, bend(CENTER))]
    );
}

/// **Every other message of the Manager is sent on the Manager, and none of a
/// Member's**: a Program Change, Volume and the sustain pedal.
/// Spec: m2-120/4.1#1, m2-120/4.6#1
#[test]
fn channel_messages_are_the_managers_only() {
    let mut s = started(2);
    let program = Voice2::ProgramChange {
        program: U7::of(8),
        bank: Some(Bank {
            msb: U7::of(1),
            lsb: U7::of(3),
        }),
    };
    for voice in [program, cc(7, 0xFFFF_FFFF), cc(64, 0xFFFF_FFFF)] {
        assert_eq!(feed(&mut s, 0, voice), [(0, voice)]);
        assert!(feed(&mut s, 1, voice).is_empty(), "{voice:?}");
    }
}

/// **Omni Off, Omni On, Mono Mode On and Poly Mode On are sent on no MPE
/// channel**, and the sender says it sent nothing; on a channel outside the zone
/// they pass.
/// Spec: m2-120/4.7#9, m2-120/4.7#11, m2-120/4.7#13, m2-120/4.7#15
#[test]
fn channel_mode_selects_are_never_sent() {
    let mut s = started(2);
    for controller in 124..=127 {
        for channel in [0, 1] {
            let message =
                Message::Midi2(Address::first(MidiChannel::of(channel)), cc(controller, 0));
            assert!(!s.feed(message.into(), |sent| panic!("{sent:?}")));
        }
        assert_eq!(feed(&mut s, 5, cc(controller, 0)), [(5, cc(controller, 0))]);
    }
}

/// **The Manager's Reset All Controllers goes on the Manager and leaves a Member's
/// own bend where its note has it**: the receiver centred the Member with the
/// zone, so it is told again. A Member's is not sent.
/// Spec: m2-120/4.7#3, m2-120/4.7#5
#[test]
fn reset_all_controllers_is_the_managers_and_keeps_the_notes_bend() {
    let mut s = started(2);
    feed(&mut s, 0, note_on(60)); // Ch3
    feed(&mut s, 0, note_bend(60, 0xC000_0000));
    feed(&mut s, 0, pressure(0x4000_0000));
    assert_eq!(
        feed(&mut s, 0, cc(121, 0)),
        [(0, cc(121, 0)), (2, bend(0xC000_0000))],
        "its pressure is 0 with the channel's"
    );
    assert!(feed(&mut s, 1, cc(121, 0)).is_empty());
}

/// **The Manager's All Notes Off and All Sound Off end the zone's notes**: they
/// go on the Manager, and the zone's Members are idle after. A Member source's
/// end its own notes, each with a Note Off.
/// Spec: m2-120/4.7#1, m2-120/4.7#6, m2-120/4.7#8
#[test]
fn all_notes_off_is_the_managers_and_note_offs_for_a_members() {
    for controller in [120, 123] {
        let mut s = started(2);
        feed(&mut s, 0, note_on(60)); // Ch3
        feed(&mut s, 1, note_on(64)); // Ch2
        assert_eq!(feed(&mut s, 1, cc(controller, 0)), [(1, note_off(64))]);
        assert_eq!(feed(&mut s, 0, cc(controller, 0)), [(0, cc(controller, 0))]);
        assert!(feed(&mut s, 0, note_off(60)).is_empty(), "it is ended");
    }
}

/// **A per-note controller MPE has no message for is not sent**, and the sender
/// says so (section 4.11 defines none).
#[test]
fn a_per_note_controller_mpe_does_not_carry_is_refused() {
    let mut s = started(1);
    feed(&mut s, 0, note_on(60));
    for voice in [
        Voice2::RegisteredPerNote {
            note: NoteNumber::of(60),
            controller: RegisteredPerNoteController::Pan,
            value: ControlValue::new(1),
        },
        Voice2::AssignablePerNote {
            note: NoteNumber::of(60),
            controller: 10,
            value: ControlValue::new(1),
        },
    ] {
        let message = Message::Midi2(Address::first(MidiChannel::FIRST), voice);
        assert!(!s.feed(message.into(), |sent| panic!("{sent:?}")));
    }
}

/// **A MIDI 1.0 source follows the same rules**: its Note On takes a Member, one
/// at velocity 0 ends it (M2-104 D.3.1), its Pitch Bend is its notes', and the
/// four Control Changes of RPN 0/0 set that bend's range and are not sent.
/// Spec: m2-120/3.5#2, m2-104/d.3.1#1
#[test]
fn a_midi1_source_follows_the_same_rules() {
    let mut s = started(2);
    let mut feed1 = |voice| {
        feed_message(
            &mut s,
            Message::Midi1(Address::first(MidiChannel::FIRST), voice),
        )
    };
    let cc1 = |controller, value| Voice1::ControlChange {
        controller: CCNumber::of(controller),
        value: U7::of(value),
    };
    let on = |velocity| Voice1::NoteOn {
        note: NoteNumber::of(60),
        velocity: U7::of(velocity),
    };
    for select in [cc1(101, 0), cc1(100, 0), cc1(6, 24), cc1(38, 0)] {
        assert!(feed1(select).is_empty(), "{select:?}");
    }
    assert_eq!(notes(&feed1(on(0x40))).len(), 1);
    let sent = feed1(Voice1::PitchBend(U14::of(0)));
    assert_eq!(sent, [(2, bend(0x4000_0000))], "24 down of 48");
    let off = notes(&feed1(on(0)));
    assert!(matches!(off[..], [(2, Voice2::NoteOff { .. })]), "{off:?}");
}

/// **A Group or channel with no zone passes everything as it is**, a MIDI 1.0
/// message in its own form.
#[test]
fn outside_a_zone_everything_passes_unchanged() {
    let mut s = started(2);
    let outside = Address::first(MidiChannel::of(3));
    let other_group = Address::new(MidiGroup::of(1), MidiChannel::FIRST);
    for message in [
        Message::Midi2(outside, note_on(60)),
        Message::Midi2(other_group, note_bend(60, 1)),
        Message::Midi1(
            other_group,
            Voice1::ControlChange {
                controller: CCNumber::of(101),
                value: U7::of(0),
            },
        ),
    ] {
        let mut sent = Vec::new();
        assert!(s.feed(message.into(), |ump| sent.push(ump)));
        assert_eq!(sent, [Ump::from(message)]);
    }
}

/// **A zone that stays keeps its notes; one that changes forgets them**, since
/// the receiver stops what sounds there (section 3.3), and starts again with its
/// range and values.
#[test]
fn a_changed_zone_forgets_its_notes_and_another_keeps_its_own() {
    let zones = |second| {
        let both = ProfileZones::single(zone(0, 2)).with(zone(8, second));
        Some(both.unwrap())
    };
    let mut s = MpeSender::new();
    s.set_profile(MidiGroup::FIRST, zones(2));
    feed(&mut s, 0, note_on(60));
    feed(&mut s, 8, note_on(60));
    s.set_profile(MidiGroup::FIRST, zones(4));
    assert_eq!(feed(&mut s, 0, note_off(60)), [(1, note_off(60))]);
    assert!(feed(&mut s, 8, note_off(60)).is_empty());
    assert_eq!(feed(&mut s, 8, note_on(62))[0], (8, bend_range(48)));
    // With no zone left its channels pass.
    s.set_profile(MidiGroup::FIRST, None);
    assert_eq!(feed(&mut s, 8, note_on(62)), [(8, note_on(62))]);
}

/// **After a send that was not taken, what the receiver holds is not assumed**:
/// the range and the note's values are sent again at the next note.
#[test]
fn values_not_sent_are_sent_again() {
    let mut s = started(1);
    s.unsent();
    assert_eq!(
        feed(&mut s, 0, note_on(60)),
        [
            (0, bend_range(48)),
            (1, bend(CENTER)),
            (1, slide(CENTER)),
            (1, pressure(0)),
            (1, note_on(60)),
        ]
    );
}

/// A bend after that is over the range the zone has, not the one it starts with:
/// 24 semitones of 96, with the range sent ahead of it.
#[test]
fn a_bend_after_an_unsent_range_is_over_the_zones_range() {
    let mut s = started(1);
    feed(&mut s, 0, note_bend_range(96));
    feed(&mut s, 0, note_on(60));
    s.unsent();
    let sent = feed(&mut s, 0, note_bend(60, 0xA000_0000));
    assert_eq!(sent[0], (0, bend_range(96)));
    assert_bend(&sent[1..2], 1, bend_of(24.0, 96.0));
}

/// A zone that goes forgets a note its source, a Member, plays on a lower Member.
#[test]
fn a_zone_that_changes_forgets_a_note_of_a_higher_source() {
    let mut s = sender(3);
    feed(&mut s, 3, note_on(60)); // on Ch2
    s.set_profile(MidiGroup::FIRST, Some(ProfileZones::single(zone(0, 5))));
    assert!(feed(&mut s, 3, note_off(60)).is_empty());
    assert_eq!(notes(&feed(&mut s, 3, note_on(60))), [(1, note_on(60))]);
}

/// **To a MIDI 1.0 device the same messages go as MIDI 1.0 bytes** (M2-104
/// D.4.1): the range as the Manager's four Control Changes, the Member's values
/// centred at 0x2000, 0x40 and 0, and the note on the Member.
/// Spec: m2-104/d.4.1#1
#[test]
fn the_midi1_translator_makes_it_midi1() {
    let mut s = sender(1);
    let translator = Midi1Translator::new();
    let mut bytes = Vec::new();
    let on = Message::Midi2(Address::first(MidiChannel::FIRST), note_on(60));
    s.feed(on.into(), |ump| {
        translator.feed(ump, |b| bytes.push(b.as_slice().to_vec()));
    });
    assert_eq!(
        bytes,
        [
            vec![0xB0, 101, 0],
            vec![0xB0, 100, 0],
            vec![0xB0, 6, 48],
            vec![0xB0, 38, 0],
            vec![0xE1, 0x00, 0x40],
            vec![0xB1, 74, 0x40],
            vec![0xD1, 0],
            vec![0x91, 60, 0x40],
        ]
    );
}

/// Nothing is allocated for a message.
#[test]
fn feeding_allocates_nothing() {
    let mut s = sender(2);
    let messages = [note_on(60), note_bend(60, 1), cc(123, 0)]
        .map(|voice| Ump::from(Message::Midi2(Address::first(MidiChannel::FIRST), voice)));
    let mut sent = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for message in messages {
            s.feed(message, |_| sent += 1);
        }
    });
    // The range, three values and the note; the bend; the Manager's All Notes Off.
    assert_eq!(sent, 7);
}
