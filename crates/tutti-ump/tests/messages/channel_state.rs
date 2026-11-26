//! Which channel state a message sets: MIDI 1.0 Detailed Specification (Control
//! Change, Channel Mode messages), M2-104 section 7.4.

use tutti_ump::{
    CCNumber, ChannelEffect, ChannelState, ControlValue, NoteNumber, ParameterKind, U7, U14,
    Voice1, Voice2,
};

fn cc1(n: u8) -> Option<ChannelEffect> {
    ChannelEffect::of_midi1(Voice1::control_change(CCNumber::new(n).unwrap(), U7::of(1)))
}

fn cc2(n: u8) -> Option<ChannelEffect> {
    ChannelEffect::of_midi2(Voice2::control_change(
        CCNumber::new(n).unwrap(),
        ControlValue::new(1),
    ))
}

#[test]
fn a_value_controller_sets_its_own_state_in_both_protocols() {
    let state = ChannelState::Controller(CCNumber::VOLUME);
    assert_eq!(cc1(7), Some(ChannelEffect::Sets(state)));
    assert_eq!(cc2(7), Some(ChannelEffect::Sets(state)));
}

#[test]
fn the_controllers_that_are_events_set_nothing() {
    // Portamento Control, Velocity Prefix, Data Increment, Data Decrement.
    for n in [84, 88, 96, 97] {
        assert_eq!(cc1(n), None, "CC {n}");
        assert_eq!(cc2(n), None, "CC {n}");
    }
}

#[test]
fn the_commands_of_the_mode_range_are_not_values() {
    assert_eq!(cc1(120), Some(ChannelEffect::EndsNotes));
    assert_eq!(cc1(123), Some(ChannelEffect::EndsNotes));
    assert_eq!(cc1(121), Some(ChannelEffect::ResetsControllers));
    for n in [122, 124, 125, 126, 127] {
        let mode = ChannelEffect::Mode(ChannelState::Controller(CCNumber::new(n).unwrap()));
        assert_eq!(cc1(n), Some(mode));
    }
}

#[test]
fn all_notes_off_and_omni_mono_poly_end_the_notes_and_local_control_does_not() {
    for n in [120, 123, 124, 125, 126, 127] {
        assert!(cc1(n).unwrap().ends_notes(), "CC {n}");
    }
    for n in [121, 122, 7] {
        assert!(!cc1(n).unwrap().ends_notes(), "CC {n}");
    }
}

#[test]
fn midi2_ignores_the_controllers_it_carries_as_other_messages() {
    // Bank select, data entry and the parameter selects have their own MIDI 2.0
    // messages (M2-104 section 7.4.2).
    for n in [0, 6, 32, 38, 98, 99, 100, 101] {
        assert_eq!(cc2(n), None, "CC {n}");
        assert!(cc1(n).is_some(), "CC {n}");
    }
}

#[test]
fn program_pressure_and_bend_set_their_state_in_both_protocols() {
    use ChannelEffect::Sets;
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::ProgramChange(U7::of(3))),
        Some(Sets(ChannelState::Program))
    );
    assert_eq!(
        ChannelEffect::of_midi2(Voice2::ProgramChange {
            program: U7::of(3),
            bank: None
        }),
        Some(Sets(ChannelState::Program))
    );
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::ChannelPressure(U7::of(3))),
        Some(Sets(ChannelState::Pressure))
    );
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::PitchBend(U14::of(3))),
        Some(Sets(ChannelState::Bend))
    );
}

#[test]
fn a_registered_and_an_assignable_controller_are_distinct_parameters() {
    let registered = Voice2::Registered {
        bank: U7::of(0),
        index: U7::of(2),
        value: ControlValue::new(0),
    };
    let assignable = Voice2::Assignable {
        bank: U7::of(0),
        index: U7::of(2),
        value: ControlValue::new(0),
    };
    let kind = |voice| match ChannelEffect::of_midi2(voice) {
        Some(ChannelEffect::Sets(ChannelState::Parameter(kind, ..))) => kind,
        other => panic!("{other:?}"),
    };
    assert_eq!(kind(registered), ParameterKind::Registered);
    assert_eq!(kind(assignable), ParameterKind::Assignable);
}

#[test]
fn notes_and_polyphonic_pressure_are_not_channel_state() {
    let note = NoteNumber::of(60);
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::note_on(note, U7::of(9))),
        None
    );
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::note_off(note, U7::of(0))),
        None
    );
    assert_eq!(
        ChannelEffect::of_midi1(Voice1::PolyPressure {
            note,
            pressure: U7::of(1)
        }),
        None
    );
}

#[test]
fn the_order_of_the_state_is_the_order_a_receiver_is_told_it_in() {
    let parameter = |kind| ChannelState::Parameter(kind, U7::of(0), U7::of(0));
    let order = [
        ChannelState::Program,
        parameter(ParameterKind::Registered),
        parameter(ParameterKind::Assignable),
        ChannelState::Controller(CCNumber::MOD_WHEEL),
        ChannelState::Pressure,
        ChannelState::Bend,
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]));
}
