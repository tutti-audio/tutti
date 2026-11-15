use tutti_ump::{CCNumber, MidiChannel, U7};

use super::super::testing::{channel, profile_table};
use super::super::*;
use CcFunction as F;

fn cc(n: u8) -> SuggestedCc {
    SuggestedCc::of(CCNumber::of(n))
}

#[test]
fn the_profile_id_is_7e_21_00_01_and_the_level() {
    // M2-113 §2.2.
    assert_eq!(
        DefaultCcLevel::MeetsMinimum.profile().to_bytes(),
        [0x7E, 0x21, 0x00, 0x01, 0x01]
    );
    assert_eq!(
        DefaultCcLevel::Partial.profile().to_bytes(),
        [0x7E, 0x21, 0x00, 0x01, 0x00]
    );
}

fn table_with(address: CiDeviceId, level: DefaultCcLevel, enabled: bool) -> ProfileTable {
    profile_table(address, level.profile(), enabled)
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    // M2-113 §2.2.1: the Destination is a channel; §2.3: enabled by Set Profile On.
    let ch = MidiChannel::of(3);
    for level in [DefaultCcLevel::Partial, DefaultCcLevel::MeetsMinimum] {
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, true), ch),
            Some(level)
        );
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, true), MidiChannel::of(4)),
            None
        );
        assert_eq!(
            DefaultCcLevel::enabled_on(&table_with(channel(3), level, false), ch),
            None
        );
    }
}

#[test]
fn a_group_wide_enable_is_not_a_channel_enable() {
    let table = table_with(CiDeviceId::Group, DefaultCcLevel::MeetsMinimum, true);
    assert_eq!(DefaultCcLevel::enabled_on(&table, MidiChannel::of(0)), None);
}

#[test]
fn a_number_32_above_a_fine_control_is_its_lsb_with_the_same_function_and_reset() {
    // Appendix A rows 32 to 63: the LSB for Control 0 to 31.
    for n in 0..32 {
        let (msb, lsb) = (cc(n), cc(n + 32));
        assert_eq!(msb.part(), CcPart::Msb, "CC {n}");
        assert_eq!(lsb.part(), CcPart::Lsb, "CC {}", n + 32);
        assert_eq!(
            (msb.function(), msb.reset(), msb.value()),
            (lsb.function(), lsb.reset(), lsb.value()),
            "CC {n} and CC {}",
            n + 32
        );
    }
}

#[test]
fn the_table_has_one_row_per_number_in_order() {
    let rows: Vec<u8> = SuggestedCc::all().map(|r| r.controller().get()).collect();
    assert_eq!(rows, (0..=127).collect::<Vec<u8>>());
}

/// Appendix A, one row per number: function, value, part, Reset All Controllers.
const APPENDIX_A: [(F, CcValue, CcPart, CcReset); 128] = [
    // 0
    (F::BankSelect, CcValue::Range, CcPart::Msb, CcReset::Keep),
    // 1
    (
        F::ModWheel,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(0)),
    ),
    // 2
    (
        F::Breath,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(0)),
    ),
    // 3
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 4
    (F::Foot, CcValue::Range, CcPart::Msb, CcReset::To(U7::of(0))),
    // 5
    (
        F::PortamentoTime,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(0)),
    ),
    // 6
    (F::DataEntry, CcValue::Range, CcPart::Msb, CcReset::Keep),
    // 7
    (F::Volume, CcValue::Range, CcPart::Msb, CcReset::Keep),
    // 8
    (
        F::Balance,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(64)),
    ),
    // 9
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 10
    (F::Pan, CcValue::Range, CcPart::Msb, CcReset::Keep),
    // 11
    (
        F::Expression,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(127)),
    ),
    // 12
    (
        F::EffectControl1,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(0)),
    ),
    // 13
    (
        F::EffectControl2,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(0)),
    ),
    // 14
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 15
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 16
    (
        F::GeneralPurpose1,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 17
    (
        F::GeneralPurpose2,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 18
    (
        F::GeneralPurpose3,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 19
    (
        F::GeneralPurpose4,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 20
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 21
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 22
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 23
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 24
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 25
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 26
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 27
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 28
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 29
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 30
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 31
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Msb,
        CcReset::DeviceSpecific,
    ),
    // 32
    (F::BankSelect, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 33
    (
        F::ModWheel,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(0)),
    ),
    // 34
    (
        F::Breath,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(0)),
    ),
    // 35
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 36
    (F::Foot, CcValue::Range, CcPart::Lsb, CcReset::To(U7::of(0))),
    // 37
    (
        F::PortamentoTime,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(0)),
    ),
    // 38
    (F::DataEntry, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 39
    (F::Volume, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 40
    (
        F::Balance,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(64)),
    ),
    // 41
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 42
    (F::Pan, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 43
    (
        F::Expression,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(127)),
    ),
    // 44
    (
        F::EffectControl1,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(0)),
    ),
    // 45
    (
        F::EffectControl2,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(0)),
    ),
    // 46
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 47
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 48
    (
        F::GeneralPurpose1,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 49
    (
        F::GeneralPurpose2,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 50
    (
        F::GeneralPurpose3,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 51
    (
        F::GeneralPurpose4,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 52
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 53
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 54
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 55
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 56
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 57
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 58
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 59
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 60
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 61
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 62
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 63
    (
        F::Undefined,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 64
    (
        F::Sustain,
        CcValue::Switch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 65
    (
        F::PortamentoSwitch,
        CcValue::Switch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 66
    (
        F::Sostenuto,
        CcValue::Switch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 67
    (
        F::SoftPedal,
        CcValue::Switch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 68
    (
        F::Legato,
        CcValue::LegatoSwitch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 69
    (
        F::Hold2,
        CcValue::Switch,
        CcPart::Whole,
        CcReset::To(U7::of(0)),
    ),
    // 70
    (
        F::SoundVariation,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::Keep,
    ),
    // 71
    (F::Resonance, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 72
    (F::ReleaseTime, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 73
    (F::AttackTime, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 74
    (F::Brightness, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 75
    (F::DecayTime, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 76
    (F::VibratoRate, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 77
    (F::VibratoDepth, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 78
    (F::VibratoDelay, CcValue::Range, CcPart::Lsb, CcReset::Keep),
    // 79
    (
        F::SoundController10,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::Keep,
    ),
    // 80
    (
        F::GeneralPurpose5,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 81
    (
        F::GeneralPurpose6,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 82
    (
        F::GeneralPurpose7,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 83
    (
        F::GeneralPurpose8,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::DeviceSpecific,
    ),
    // 84
    (
        F::PortamentoControl,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::Keep,
    ),
    // 85
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 86
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 87
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 88
    (
        F::VelocityPrefix,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::Keep,
    ),
    // 89
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 90
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 91
    (F::ReverbSend, CcValue::Range, CcPart::Whole, CcReset::Keep),
    // 92
    (
        F::Effects2Depth,
        CcValue::Range,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 93
    (F::ChorusSend, CcValue::Range, CcPart::Whole, CcReset::Keep),
    // 94
    (
        F::Effects4Depth,
        CcValue::Range,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 95
    (
        F::Effects5Depth,
        CcValue::Range,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 96
    (
        F::DataIncrement,
        CcValue::Unused,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 97
    (
        F::DataDecrement,
        CcValue::Unused,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 98
    (
        F::NrpnSelect,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(127)),
    ),
    // 99
    (
        F::NrpnSelect,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(127)),
    ),
    // 100
    (
        F::RpnSelect,
        CcValue::Range,
        CcPart::Lsb,
        CcReset::To(U7::of(127)),
    ),
    // 101
    (
        F::RpnSelect,
        CcValue::Range,
        CcPart::Msb,
        CcReset::To(U7::of(127)),
    ),
    // 102
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 103
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 104
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 105
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 106
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 107
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 108
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 109
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 110
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 111
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 112
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 113
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 114
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 115
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 116
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 117
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 118
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 119
    (
        F::Undefined,
        CcValue::Undefined,
        CcPart::Whole,
        CcReset::DeviceSpecific,
    ),
    // 120
    (F::AllSoundOff, CcValue::Zero, CcPart::Whole, CcReset::Keep),
    // 121
    (F::ResetAll, CcValue::Zero, CcPart::Whole, CcReset::Keep),
    // 122
    (
        F::LocalControl,
        CcValue::OnOff127,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 123
    (F::AllNotesOff, CcValue::Zero, CcPart::Whole, CcReset::Keep),
    // 124
    (F::OmniModeOff, CcValue::Zero, CcPart::Whole, CcReset::Keep),
    // 125
    (F::OmniModeOn, CcValue::Zero, CcPart::Whole, CcReset::Keep),
    // 126
    (
        F::MonoModeOn,
        CcValue::ChannelCount,
        CcPart::Whole,
        CcReset::Keep,
    ),
    // 127
    (F::PolyModeOn, CcValue::Zero, CcPart::Whole, CcReset::Keep),
];

#[test]
fn every_row_is_the_one_appendix_a_gives() {
    // Swapping any two rows of the table fails here.
    for (n, (function, value, part, reset)) in APPENDIX_A.into_iter().enumerate() {
        let row = cc(u8::try_from(n).unwrap());
        assert_eq!(row.controller().get() as usize, n);
        assert_eq!(
            (row.function(), row.value(), row.part(), row.reset()),
            (function, value, part, reset),
            "CC {n}"
        );
    }
}

#[test]
fn a_function_has_the_name_cc_number_gives_its_number() {
    // One name per concept: a number `CCNumber` names has the function of the same name.
    for (number, function) in [
        (CCNumber::BANK_SELECT, F::BankSelect),
        (CCNumber::MOD_WHEEL, F::ModWheel),
        (CCNumber::BREATH, F::Breath),
        (CCNumber::FOOT, F::Foot),
        (CCNumber::PORTAMENTO_TIME, F::PortamentoTime),
        (CCNumber::DATA_ENTRY, F::DataEntry),
        (CCNumber::VOLUME, F::Volume),
        (CCNumber::BALANCE, F::Balance),
        (CCNumber::PAN, F::Pan),
        (CCNumber::EXPRESSION, F::Expression),
        (CCNumber::BANK_SELECT_LSB, F::BankSelect),
        (CCNumber::DATA_ENTRY_LSB, F::DataEntry),
        (CCNumber::RESONANCE, F::Resonance),
        (CCNumber::RELEASE_TIME, F::ReleaseTime),
        (CCNumber::ATTACK_TIME, F::AttackTime),
        (CCNumber::BRIGHTNESS, F::Brightness),
        (CCNumber::SUSTAIN, F::Sustain),
        (CCNumber::PORTAMENTO_SWITCH, F::PortamentoSwitch),
        (CCNumber::SOSTENUTO, F::Sostenuto),
        (CCNumber::SOFT_PEDAL, F::SoftPedal),
        (CCNumber::LEGATO, F::Legato),
        (CCNumber::PORTAMENTO_CONTROL, F::PortamentoControl),
        (CCNumber::VELOCITY_PREFIX, F::VelocityPrefix),
        (CCNumber::ALL_SOUND_OFF, F::AllSoundOff),
        (CCNumber::RESET_ALL, F::ResetAll),
        (CCNumber::ALL_NOTES_OFF, F::AllNotesOff),
        (CCNumber::REVERB_SEND, F::ReverbSend),
        (CCNumber::CHORUS_SEND, F::ChorusSend),
        (CCNumber::NRPN_LSB, F::NrpnSelect),
        (CCNumber::NRPN_MSB, F::NrpnSelect),
        (CCNumber::RPN_LSB, F::RpnSelect),
        (CCNumber::RPN_MSB, F::RpnSelect),
    ] {
        assert_eq!(SuggestedCc::of(number).function(), function, "{number:?}");
    }
}

#[test]
fn the_part_of_a_fine_control_follows_cc_number_lsb() {
    for n in 0..32 {
        let msb = CCNumber::of(n);
        let lsb = msb.lsb().unwrap();
        assert_eq!(SuggestedCc::of(msb).part(), CcPart::Msb);
        assert_eq!(SuggestedCc::of(lsb).part(), CcPart::Lsb);
    }
}
