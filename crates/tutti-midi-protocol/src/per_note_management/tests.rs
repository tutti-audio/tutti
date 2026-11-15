use tutti_ump::{
    Address, Attribute, BendValue, ControlValue, MidiChannel, MidiGroup, NoteNumber, NoteVelocity,
    PerNoteManagement, RegisteredPerNoteController, Voice2,
};

use super::*;

const A: Address = Address::new(MidiGroup::of(0), MidiChannel::of(0));

fn n(note: u8) -> NoteNumber {
    NoteNumber::of(note)
}

fn on(note: u8) -> Voice2 {
    Voice2::NoteOn {
        note: n(note),
        velocity: NoteVelocity::MAX,
        attribute: Attribute::None,
    }
}

fn off(note: u8) -> Voice2 {
    Voice2::NoteOff {
        note: n(note),
        velocity: NoteVelocity::MIN,
        attribute: Attribute::None,
    }
}

fn pan(note: u8) -> Voice2 {
    Voice2::RegisteredPerNote {
        note: n(note),
        controller: RegisteredPerNoteController::Pan,
        value: ControlValue::MAX,
    }
}

fn manager(choice: PerNoteManagement) -> PerNoteManager {
    PerNoteManager::new(choice)
}

/// What a sender sends for `script`, the management owed put in.
fn send(m: &mut PerNoteManager, script: &[Voice2]) -> Vec<Voice2> {
    script
        .iter()
        .flat_map(|&v| m.before(A, &v).into_iter().chain([v]))
        .collect()
}

fn management(note: u8, management: PerNoteManagement) -> Voice2 {
    management.voice(n(note))
}

#[test]
fn a_first_note_with_its_setup_controllers_owes_nothing() {
    // Appendix C.2, Figure 91: the controllers before the first Note On set it up.
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let script = [pan(60), on(60), pan(60), off(60)];
    assert_eq!(send(&mut m, &script), script);
}

#[test]
fn the_next_notes_setup_is_preceded_by_management() {
    // Appendix C.2, Figure 91: management, the controller, the Note On.
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let sent = send(&mut m, &[on(60), pan(60), off(60), pan(60), on(60)]);
    assert_eq!(
        sent,
        [
            on(60),
            pan(60),
            off(60),
            management(60, PerNoteManagement::DetachAndReset),
            pan(60),
            on(60)
        ]
    );
}

#[test]
fn a_restruck_key_that_still_sounds_is_managed_before_its_note_on() {
    let mut m = manager(PerNoteManagement::Detach);
    let sent = send(&mut m, &[on(60), pan(60), on(60)]);
    assert_eq!(
        sent,
        [
            on(60),
            pan(60),
            management(60, PerNoteManagement::Detach),
            on(60)
        ]
    );
}

#[test]
fn the_choice_is_what_is_sent() {
    for (choice, d, r) in [
        (PerNoteManagement::Detach, true, false),
        (PerNoteManagement::Reset, false, true),
        (PerNoteManagement::DetachAndReset, true, true),
    ] {
        let mut m = manager(choice);
        let sent = send(&mut m, &[on(1), pan(1), on(1)]);
        assert_eq!(sent[2], management(1, choice));
        assert_eq!((choice.detaches(), choice.resets()), (d, r));
    }
}

#[test]
fn a_key_struck_with_no_controller_sent_owes_nothing() {
    // Nothing is attached: there is nothing to detach or reset.
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let script = [on(60), off(60), on(60), on(60)];
    assert_eq!(send(&mut m, &script), script);
}

#[test]
fn management_is_owed_once_per_strike() {
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let sent = send(
        &mut m,
        &[on(60), pan(60), off(60), pan(60), pan(60), on(60)],
    );
    let managements = sent
        .iter()
        .filter(|v| matches!(v, Voice2::PerNoteManagement { .. }))
        .count();
    assert_eq!(managements, 1);
}

#[test]
fn keys_do_not_share_state() {
    // Per-Note Controllers are shared by the notes of one Note Number only.
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let script = [on(60), pan(60), off(60), on(61), pan(61), pan(62)];
    assert_eq!(send(&mut m, &script), script);
}

#[test]
fn channels_and_groups_do_not_share_state() {
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let other = [
        Address::new(MidiGroup::of(0), MidiChannel::of(1)),
        Address::new(MidiGroup::of(1), MidiChannel::of(0)),
    ];
    assert!(m.before(A, &on(60)).is_none());
    assert!(m.before(A, &pan(60)).is_none());
    for address in other {
        assert!(m.before(address, &on(60)).is_none());
    }
    assert_eq!(
        m.before(A, &on(60)),
        Some(management(60, PerNoteManagement::DetachAndReset)),
        "the first channel's key is still attached"
    );
}

#[test]
fn a_management_the_sender_sends_detaches_the_key() {
    let mut m = manager(PerNoteManagement::DetachAndReset);
    let script = [
        on(60),
        pan(60),
        off(60),
        management(60, PerNoteManagement::Detach),
        pan(60),
        on(60),
    ];
    assert_eq!(send(&mut m, &script), script);
}

#[test]
fn every_per_note_controller_message_touches_the_key() {
    // Appendix C names Poly Aftertouch and the Registered, Assignable and Pitch Bend ones.
    let touches = [
        Voice2::PolyPressure {
            note: n(9),
            value: ControlValue::MAX,
        },
        Voice2::AssignablePerNote {
            note: n(9),
            controller: 3,
            value: ControlValue::MAX,
        },
        Voice2::PerNotePitchBend {
            note: n(9),
            value: BendValue::CENTER,
        },
        pan(9),
    ];
    for touch in touches {
        let mut m = manager(PerNoteManagement::DetachAndReset);
        assert_eq!(
            send(&mut m, &[on(9), touch, on(9)])[2],
            management(9, PerNoteManagement::DetachAndReset),
            "{touch:?}"
        );
    }
}

#[test]
fn a_message_that_is_not_per_note_owes_nothing_and_changes_nothing() {
    let mut m = manager(PerNoteManagement::DetachAndReset);
    m.before(A, &on(60));
    m.before(A, &pan(60));
    assert_eq!(
        m.before(A, &Voice2::ChannelPressure(ControlValue::MAX)),
        None
    );
    assert_eq!(
        m.before(A, &on(60)),
        Some(management(60, PerNoteManagement::DetachAndReset))
    );
}

#[test]
fn clear_forgets_what_is_attached() {
    let mut m = manager(PerNoteManagement::DetachAndReset);
    m.before(A, &on(60));
    m.before(A, &pan(60));
    m.clear();
    assert_eq!(m.before(A, &on(60)), None);
}
