use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{ControlValue, MidiChannel, Rpn, U7};

use super::super::testing::{channel, profile_table};
use super::super::*;

fn rc(index: u8) -> Rpn {
    Rpn::new(U7::of(0x40), U7::of(index))
}

#[test]
fn the_profile_id_is_7e_20_01_01_and_the_level() {
    // M2-121 Table 4.
    for (level, byte) in [
        (OrganLevel::Partial, 0x00),
        (OrganLevel::MeetsMinimum, 0x01),
        (OrganLevel::Extended, 0x02),
    ] {
        assert_eq!(level.profile().to_bytes(), [0x7E, 0x20, 0x01, 0x01, byte]);
    }
}

#[test]
fn the_nine_drawbars_are_the_registered_controllers_of_table_7() {
    // M2-121 Table 7, in order of footage.
    let rows = [
        (Drawbar::Feet16, 0x30),
        (Drawbar::Feet5And1Third, 0x31),
        (Drawbar::Feet8, 0x32),
        (Drawbar::Feet4, 0x33),
        (Drawbar::Feet2And2Thirds, 0x34),
        (Drawbar::Feet2, 0x35),
        (Drawbar::Feet1And3Fifths, 0x36),
        (Drawbar::Feet1And1Third, 0x37),
        (Drawbar::Feet1, 0x38),
    ];
    assert_eq!(rows.map(|r| r.0), Drawbar::ALL);
    for (drawbar, index) in rows {
        assert_eq!(drawbar.controller(), rc(index), "{drawbar:?}");
        assert_eq!(Drawbar::of_controller(rc(index)), Some(drawbar));
    }
    assert_eq!(Drawbar::of_controller(rc(0x2F)), None);
    assert_eq!(Drawbar::of_controller(rc(0x39)), None);
    assert_eq!(
        Drawbar::of_controller(Rpn::new(U7::of(0x41), U7::of(0x30))),
        None
    );
}

/// M2-121 Table 6: the setting, the first and last value of its range, and
/// its discrete value.
const TABLE_6: [(u8, u32, u32, u32); 9] = [
    (0, 0x0000_0000, 0x1C71_C71B, 0x0E38_E38E),
    (1, 0x1C71_C71C, 0x38E3_8E37, 0x2AAA_AAAB),
    (2, 0x38E3_8E38, 0x5555_5554, 0x471C_71C7),
    (3, 0x5555_5555, 0x71C7_1C70, 0x638E_38E4),
    (4, 0x71C7_1C71, 0x8E38_E38C, 0x8000_0000),
    (5, 0x8E38_E38D, 0xAAAA_AAA9, 0x9C71_C71D),
    (6, 0xAAAA_AAAA, 0xC71C_71C5, 0xB8E3_8E39),
    (7, 0xC71C_71C6, 0xE38E_38E1, 0xD555_5556),
    (8, 0xE38E_38E2, 0xFFFF_FFFF, 0xF1C7_1C72),
];

#[test]
fn each_setting_matches_its_row_of_table_6() {
    for (setting, first, last, discrete) in TABLE_6 {
        let s = DrawbarSetting::of(setting);
        assert_eq!(
            s.range(),
            ControlValue::new(first)..=ControlValue::new(last),
            "setting {setting}"
        );
        assert_eq!(s.discrete_value().get(), discrete, "setting {setting}");
    }
    assert_eq!(
        (DrawbarSetting::OFF.get(), DrawbarSetting::FULL.get()),
        (0, 8)
    );
}

/// Spec: m2-121/5.1.2#5
#[test]
fn a_value_falls_in_the_range_whose_ends_it_borders() {
    // Both ends of every range of Table 6, and each discrete value, read back.
    for (setting, first, last, discrete) in TABLE_6 {
        for value in [first, last, discrete] {
            assert_eq!(
                DrawbarSetting::of_value(ControlValue::new(value)).get(),
                setting,
                "{value:#x}"
            );
        }
    }
}

#[test]
fn a_drawbar_setting_is_0_to_8() {
    check_range_contract(&RangeContract {
        valid: &[0, 1, 8],
        invalid: &[9, 255],
        new: DrawbarSetting::new,
        get: DrawbarSetting::get,
        of: Some(DrawbarSetting::of),
    });
}

#[test]
fn the_vibrato_chorus_ranges_are_those_of_section_6_1_2_1() {
    // M2-121 §6.1.2.1: type, first and last value.
    let rows = [
        (VibratoChorus::V1, 0x0000_0000, 0x2AAA_AAA9),
        (VibratoChorus::C1, 0x2AAA_AAAA, 0x5555_5554),
        (VibratoChorus::V2, 0x5555_5555, 0x7FFF_FFFF),
        (VibratoChorus::C2, 0x8000_0000, 0xAAAA_AAA9),
        (VibratoChorus::V3, 0xAAAA_AAAA, 0xD555_5554),
        (VibratoChorus::C3, 0xD555_5555, 0xFFFF_FFFF),
    ];
    for (kind, first, last) in rows {
        assert_eq!(
            kind.range(),
            ControlValue::new(first)..=ControlValue::new(last),
            "{kind:?}"
        );
        for value in [first, last] {
            assert_eq!(VibratoChorus::of_value(ControlValue::new(value)), kind);
        }
    }
}

#[test]
fn the_optional_controllers_are_table_10_and_3f_is_reserved() {
    // M2-121 Table 10.
    let rows = [
        (
            OrganOption::VibratoChorusType,
            0x39,
            OrganValue::VibratoChorus,
        ),
        (OrganOption::VibratoChorusOn, 0x3A, OrganValue::Switch),
        (OrganOption::PercussionOn, 0x3B, OrganValue::Switch),
        (OrganOption::PercussionSoft, 0x3C, OrganValue::Switch),
        (OrganOption::PercussionFast, 0x3D, OrganValue::Switch),
        (OrganOption::PercussionThird, 0x3E, OrganValue::Switch),
        (OrganOption::KeyClick, 0x40, OrganValue::Amount),
        (OrganOption::Crosstalk, 0x41, OrganValue::Amount),
    ];
    assert_eq!(rows.map(|r| r.0), OrganOption::ALL);
    for (option, index, kind) in rows {
        assert_eq!(option.controller(), rc(index), "{option:?}");
        assert_eq!(OrganOption::of_controller(rc(index)), Some(option));
        assert_eq!(option.value_kind(), kind, "{option:?}");
    }
    assert_eq!(OrganOption::of_controller(rc(0x3F)), None);
}

#[test]
fn the_defaults_are_those_of_section_6_1_2() {
    // §6.1.2.2: Off; §6.1.2.7 and §6.1.2.8: 0x80000000; the rest state none.
    let default = |o: OrganOption| o.default_value().map(ControlValue::get);
    assert_eq!(default(OrganOption::VibratoChorusOn), Some(0));
    assert_eq!(default(OrganOption::KeyClick), Some(0x8000_0000));
    assert_eq!(default(OrganOption::Crosstalk), Some(0x8000_0000));
    assert_eq!(default(OrganOption::VibratoChorusType), None);
    assert_eq!(default(OrganOption::PercussionOn), None);
}

#[test]
fn the_key_range_is_36_to_96_with_69_sounding_a440() {
    // M2-121 §5.1.1.
    assert_eq!(
        (
            ORGAN_LOWEST_NOTE.get(),
            ORGAN_HIGHEST_NOTE.get(),
            ORGAN_REFERENCE_NOTE.get()
        ),
        (36, 96, 69)
    );
}

#[test]
fn the_features_data_is_the_byte_of_table_5() {
    // M2-121 Table 5: D0 soft pedal, D1 vibrato/chorus, D2 percussion, D3 key
    // click, D4 crosstalk.
    assert_eq!(OrganFeatures::INQUIRY_TARGET.get(), U7::of(0x01));
    assert_eq!(OrganFeatures::default().to_data(), [0x00]);
    let each = [
        (
            OrganFeatures {
                soft_pedal: true,
                ..Default::default()
            },
            0x01,
        ),
        (
            OrganFeatures {
                vibrato_chorus: true,
                ..Default::default()
            },
            0x02,
        ),
        (
            OrganFeatures {
                percussion: true,
                ..Default::default()
            },
            0x04,
        ),
        (
            OrganFeatures {
                key_click: true,
                ..Default::default()
            },
            0x08,
        ),
        (
            OrganFeatures {
                crosstalk: true,
                ..Default::default()
            },
            0x10,
        ),
    ];
    for (features, byte) in each {
        assert_eq!(features.to_data(), [byte]);
        assert_eq!(OrganFeatures::from_data(&[byte]), Some(features));
    }
}

#[test]
fn features_data_refuses_the_reserved_bits_and_a_wrong_length() {
    // Table 5: D5 and D6 reserved, D7 a status bit that is 0.
    for byte in [0x20, 0x40, 0x80] {
        assert_eq!(OrganFeatures::from_data(&[byte]), None, "{byte:#x}");
    }
    assert_eq!(OrganFeatures::from_data(&[]), None);
    assert_eq!(OrganFeatures::from_data(&[0, 0]), None);
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    // M2-121 §4: the Profile works on a Channel.
    let ch = MidiChannel::of(0);
    for level in [
        OrganLevel::Partial,
        OrganLevel::MeetsMinimum,
        OrganLevel::Extended,
    ] {
        let on = profile_table(channel(0), level.profile(), true);
        assert_eq!(OrganLevel::enabled_on(&on, ch), Some(level));
        assert_eq!(OrganLevel::enabled_on(&on, MidiChannel::of(1)), None);
        let off = profile_table(channel(0), level.profile(), false);
        assert_eq!(OrganLevel::enabled_on(&off, ch), None);
    }
    let group = profile_table(CiDeviceId::Group, OrganLevel::Extended.profile(), true);
    assert_eq!(OrganLevel::enabled_on(&group, ch), None);
}

#[test]
fn the_details_reply_carries_the_one_byte_bitmap_of_table_5() {
    use super::super::testing::{DST, header_on, preamble, to_dst};
    // M2-121 §4.2.2: target 0x01, a one-byte Inquiry Target Data.
    let features = OrganFeatures {
        soft_pedal: true,
        percussion: true,
        crosstalk: true,
        ..Default::default()
    };
    let reply = CiMessage::ProfileDetailsReply {
        header: header_on(channel(0)),
        to: Muid::of(DST),
        profile: OrganLevel::Extended.profile(),
        target: OrganFeatures::INQUIRY_TARGET,
        data: DetailsData::new(features.to_data().to_vec()).unwrap(),
    };
    let mut want = preamble(0x00, 0x29, to_dst());
    want.extend([0x7E, 0x20, 0x01, 0x01, 0x02, 0x01, 0x01, 0x00, 0b1_0101]);
    assert_eq!(reply.encode(), want);
}
