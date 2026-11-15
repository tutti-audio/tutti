use tutti_ump::{MidiChannel, Rpn, U7};

use super::super::testing::{channel, profile_table};
use super::super::*;

#[test]
fn the_profile_id_is_7e_22_00_01_01() {
    // M2-122 Table 4.
    assert_eq!(
        ProfileId::ROTARY_SPEAKER.to_bytes(),
        [0x7E, 0x22, 0x00, 0x01, 0x01]
    );
}

/// M2-122 Table 5 and Table 6, one row per Registered Controller: the index
/// under bank 0x60, the default value.
const CONTROLS: [(RotaryControl, u8, u32); 14] = [
    (RotaryControl::Speed, 0x20, 0x0000_0000),
    (RotaryControl::Effect, 0x21, 0xFFFF_FFFF),
    (RotaryControl::Brake, 0x22, 0x0000_0000),
    (RotaryControl::HornSlowSpeed, 0x23, 0x8000_0000),
    (RotaryControl::HornFastSpeed, 0x24, 0x8000_0000),
    (RotaryControl::WooferSlowSpeed, 0x25, 0x8000_0000),
    (RotaryControl::WooferFastSpeed, 0x26, 0x8000_0000),
    (RotaryControl::HornAccelerateTime, 0x27, 0x8000_0000),
    (RotaryControl::HornDecelerateTime, 0x28, 0x8000_0000),
    (RotaryControl::WooferAccelerateTime, 0x29, 0x8000_0000),
    (RotaryControl::WooferDecelerateTime, 0x2A, 0x8000_0000),
    (RotaryControl::HornLevel, 0x2B, 0xC800_0000),
    (RotaryControl::WooferLevel, 0x2C, 0xC800_0000),
    (RotaryControl::Overdrive, 0x2D, 0x0000_0000),
];

#[test]
fn each_control_matches_its_row_of_the_tables() {
    assert_eq!(CONTROLS.map(|r| r.0), RotaryControl::ALL);
    for (control, index, default) in CONTROLS {
        let rc = Rpn::new(U7::of(0x60), U7::of(index));
        assert_eq!(control.controller(), rc, "{control:?}");
        assert_eq!(RotaryControl::of_controller(rc), Some(control));
        assert_eq!(control.default_value().get(), default, "{control:?}");
    }
    assert_eq!(
        RotaryControl::of_controller(Rpn::new(U7::of(0x60), U7::of(0x2E))),
        None
    );
    assert_eq!(
        RotaryControl::of_controller(Rpn::new(U7::of(0x61), U7::of(0x20))),
        None
    );
}

#[test]
fn only_speed_is_required_and_the_value_kinds_follow_the_tables() {
    // §3.2.1: the speed; §3.2.2: the rest are optional.
    let required: Vec<_> = RotaryControl::ALL
        .into_iter()
        .filter(|c| c.is_required())
        .collect();
    assert_eq!(required, [RotaryControl::Speed]);
    let kinds = [
        RotaryValue::Switch,
        RotaryValue::Switch,
        RotaryValue::Switch,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Speed,
        RotaryValue::Level,
        RotaryValue::Level,
        RotaryValue::Level,
    ];
    assert_eq!(RotaryControl::ALL.map(RotaryControl::value_kind), kinds);
}

#[test]
fn a_switch_value_is_slow_up_to_7fffffff_and_fast_from_80000000() {
    // M2-122 Table 5; a two-position sender sends 0 and 0xFFFFFFFF.
    use tutti_ump::ControlValue;
    assert_eq!(TwoState::of(ControlValue::new(0)), TwoState::Low);
    assert_eq!(TwoState::of(ControlValue::new(0x7FFF_FFFF)), TwoState::Low);
    assert_eq!(TwoState::of(ControlValue::new(0x8000_0000)), TwoState::High);
    assert_eq!(TwoState::Low.value().get(), 0);
    assert_eq!(TwoState::High.value().get(), 0xFFFF_FFFF);
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    let on = profile_table(channel(2), ProfileId::ROTARY_SPEAKER, true);
    assert!(RotarySpeaker::enabled_on(&on, MidiChannel::of(2)));
    assert!(!RotarySpeaker::enabled_on(&on, MidiChannel::of(3)));
    let off = profile_table(channel(2), ProfileId::ROTARY_SPEAKER, false);
    assert!(!RotarySpeaker::enabled_on(&off, MidiChannel::of(2)));
    let group = profile_table(CiDeviceId::Group, ProfileId::ROTARY_SPEAKER, true);
    assert!(!RotarySpeaker::enabled_on(&group, MidiChannel::of(2)));
}
