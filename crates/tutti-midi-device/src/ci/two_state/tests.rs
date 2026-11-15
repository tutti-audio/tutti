use tutti_ump::ControlValue;

use super::super::*;

#[test]
fn a_value_is_low_up_to_7fffffff_and_high_from_80000000() {
    // M2-121 §6.1.1, §6.1.2.2: 0x00000000-0x7FFFFFFF and 0x80000000-0xFFFFFFFF.
    let state = |raw| TwoState::of(ControlValue::new(raw));
    assert_eq!(state(0), TwoState::Low);
    assert_eq!(state(0x7FFF_FFFF), TwoState::Low);
    assert_eq!(state(0x8000_0000), TwoState::High);
    assert_eq!(state(0xFFFF_FFFF), TwoState::High);
}

#[test]
fn a_two_position_sender_uses_the_ends_of_the_halves() {
    // M2-122 Table 5: 0x00000000 and 0xFFFFFFFF.
    assert_eq!(TwoState::Low.value().get(), 0);
    assert_eq!(TwoState::High.value().get(), 0xFFFF_FFFF);
    for state in [TwoState::Low, TwoState::High] {
        assert_eq!(TwoState::of(state.value()), state);
    }
}

#[test]
fn each_meaning_names_its_two_states() {
    use SwitchMeaning::*;
    let rows = [
        (OffOn, "off", "on"),
        (NormalSoft, "normal", "soft"),
        (SlowFast, "slow", "fast"),
        (RotateStop, "rotate", "stop"),
        (SecondThirdHarmonic, "2nd harmonic", "3rd harmonic"),
    ];
    for (meaning, low, high) in rows {
        assert_eq!(meaning.name(TwoState::Low), low);
        assert_eq!(meaning.name(TwoState::High), high);
    }
}

#[test]
fn the_organ_switches_mean_what_section_6_1_2_says() {
    // M2-121 §6.1.2.2 to §6.1.2.6; the type and the amounts are not switches.
    use SwitchMeaning::*;
    let meanings = OrganOption::ALL.map(OrganOption::switch_meaning);
    assert_eq!(
        meanings,
        [
            None,
            Some(OffOn),
            Some(OffOn),
            Some(NormalSoft),
            Some(SlowFast),
            Some(SecondThirdHarmonic),
            None,
            None,
        ]
    );
    for option in OrganOption::ALL {
        assert_eq!(
            option.switch_meaning().is_some(),
            option.value_kind() == OrganValue::Switch,
            "{option:?}"
        );
    }
}

#[test]
fn the_rotary_switches_mean_what_tables_5_and_6_say() {
    // M2-122: Speed slow/fast, Effect off/on, Brake rotate/stop.
    use SwitchMeaning::*;
    for control in RotaryControl::ALL {
        let want = match control {
            RotaryControl::Speed => Some(SlowFast),
            RotaryControl::Effect => Some(OffOn),
            RotaryControl::Brake => Some(RotateStop),
            _ => None,
        };
        assert_eq!(control.switch_meaning(), want, "{control:?}");
        assert_eq!(
            control.switch_meaning().is_some(),
            control.value_kind() == RotaryValue::Switch,
            "{control:?}"
        );
    }
}
