use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_ump::{CCNumber, MidiChannel, RegisteredPerNoteController, U7};
use tutti_units::Pan;

use super::super::testing::{channel, profile_table};
use super::super::*;

/// M2-125 Table 5, one row per Note Number: the note, the name, the Mutually
/// Exclusive Set (0 for none), the recommended pan in percent (negative is
/// left) and the Details bitmap byte and bit.
const TABLE_5: [(u8, &str, u8, i8, u8, u8); 62] = [
    (27, "High Q", 0, -23, 2, 0),
    (28, "Slap", 0, -23, 2, 1),
    (29, "Scratch Push", 7, -16, 2, 2),
    (30, "Scratch Pull", 7, -16, 2, 3),
    (31, "Sticks", 0, 0, 2, 4),
    (32, "Square Click", 0, -16, 2, 5),
    (33, "Metronome Click", 0, 0, 2, 6),
    (34, "Metronome Bell", 0, 0, 3, 0),
    (35, "Acoustic Bass Drum", 0, 0, 3, 1),
    (36, "Bass Drum 1", 0, 0, 3, 2),
    (37, "Side Stick", 0, 0, 3, 3),
    (38, "Acoustic Snare", 0, 0, 3, 4),
    (39, "Hand Clap", 0, -16, 3, 5),
    (40, "Electric Snare", 0, 0, 3, 6),
    (41, "Low Floor Tom", 0, -47, 4, 0),
    (42, "Closed Hi-hat", 1, 32, 4, 1),
    (43, "High Floor Tom", 0, -28, 4, 2),
    (44, "Pedal Hi-hat", 1, 32, 4, 3),
    (45, "Low Tom", 0, -9, 4, 4),
    (46, "Open Hi-hat", 1, 32, 4, 5),
    (47, "Low-Mid Tom", 0, 10, 4, 6),
    (48, "High Mid Tom", 0, 29, 5, 0),
    (49, "Crash Cymbal 1", 0, 32, 5, 1),
    (50, "High Tom", 0, 48, 5, 2),
    (51, "Ride Cymbal 1", 0, -31, 5, 3),
    (52, "Chinese Cymbal", 0, -31, 5, 4),
    (53, "Ride Bell", 0, -31, 5, 5),
    (54, "Tambourine", 0, 16, 5, 6),
    (55, "Splash Cymbal", 0, -16, 6, 0),
    (56, "Cowbell", 0, 32, 6, 1),
    (57, "Crash Cymbal 2", 0, -31, 6, 2),
    (58, "Vibra-slap", 0, -55, 6, 3),
    (59, "Ride Cymbal 2", 0, -31, 6, 4),
    (60, "High Bongo", 0, 56, 6, 5),
    (61, "Low Bongo", 0, 56, 6, 6),
    (62, "Mute High Conga", 0, -39, 7, 0),
    (63, "Open High Conga", 0, -39, 7, 1),
    (64, "Low Conga", 0, -39, 7, 2),
    (65, "High Timbale", 0, 32, 7, 3),
    (66, "Low Timbale", 0, 32, 7, 4),
    (67, "High Agogo", 0, -55, 7, 5),
    (68, "Low Agogo", 0, -55, 7, 6),
    (69, "Cabasa", 0, -55, 8, 0),
    (70, "Maracas", 0, -63, 8, 1),
    (71, "Short Whistle", 2, 56, 8, 2),
    (72, "Long Whistle", 2, 56, 8, 3),
    (73, "Short Guiro", 3, 48, 8, 4),
    (74, "Long Guiro", 3, 48, 8, 5),
    (75, "Claves", 0, 32, 8, 6),
    (76, "High Wood Block", 0, 56, 9, 0),
    (77, "Low Wood Block", 0, 56, 9, 1),
    (78, "Mute Cuica", 4, -31, 9, 2),
    (79, "Open Cuica", 4, -31, 9, 3),
    (80, "Mute Triangle", 5, -63, 9, 4),
    (81, "Open Triangle", 5, -63, 9, 5),
    (82, "Shaker", 0, 48, 9, 6),
    (83, "Jingle Bell", 0, 56, 10, 0),
    (84, "Bell Tree", 0, 60, 10, 1),
    (85, "Castanets", 0, -47, 10, 2),
    (86, "Mute Surdo", 6, -31, 10, 3),
    (87, "Open Surdo", 6, -31, 10, 4),
    (88, "Applause", 0, 0, 10, 5),
];

#[test]
fn the_profile_id_is_7e_20_03_01_and_the_level() {
    // M2-125 Table 4.
    assert_eq!(
        DrumNoteMapLevel::MeetsMinimum.profile().to_bytes(),
        [0x7E, 0x20, 0x03, 0x01, 0x01]
    );
    assert_eq!(
        DrumNoteMapLevel::Partial.profile().to_bytes(),
        [0x7E, 0x20, 0x03, 0x01, 0x00]
    );
}

/// Spec: m2-125/4#1, m2-125/4.1#1
#[test]
fn each_instrument_matches_its_row_of_table_5() {
    let rows: Vec<_> = DrumInstrument::all().collect();
    assert_eq!(rows.len(), TABLE_5.len());
    for (instrument, (note, name, mes, pan, byte, bit)) in rows.into_iter().zip(TABLE_5) {
        assert_eq!(instrument.note().get(), note, "{name}");
        assert_eq!(instrument.name(), name, "note {note}");
        assert_eq!(
            instrument.mutually_exclusive_set().map_or(0, |s| s.get()),
            mes,
            "{name}"
        );
        assert_eq!(
            instrument.recommended_pan(),
            Pan::new(f64::from(pan) / 100.0),
            "{name}"
        );
        assert_eq!(
            (
                instrument.bitmap_position().byte(),
                instrument.bitmap_position().bit()
            ),
            (byte, bit),
            "{name}"
        );
    }
}

/// Spec: m2-125/4.1#3
#[test]
fn a_note_number_names_its_instrument_only_from_27_to_88() {
    // M2-125 §4.1: a device may add sounds on 0 to 26 and 89 to 127.
    for n in 0..=127u8 {
        let found = DrumInstrument::of_note(U7::of(n));
        assert_eq!(found.is_some(), (27..=88).contains(&n), "note {n}");
        if let Some(i) = found {
            assert_eq!(i.note().get(), n);
        }
    }
}

#[test]
fn the_mutually_exclusive_sets_are_the_seven_of_section_4_1() {
    // M2-125 §4.1: MES 1 to MES 7, by Note Number.
    let sets: [(u8, &[u8]); 7] = [
        (1, &[42, 44, 46]),
        (2, &[71, 72]),
        (3, &[73, 74]),
        (4, &[78, 79]),
        (5, &[80, 81]),
        (6, &[86, 87]),
        (7, &[29, 30]),
    ];
    for (number, notes) in sets {
        let set = MutuallyExclusiveSet::of(number);
        let members: Vec<u8> = set.members().map(|i| i.note().get()).collect();
        assert_eq!(members, notes, "MES {number}");
    }
}

#[test]
fn a_bitmap_position_is_a_bit_of_data_bytes_2_to_10() {
    // Table 11: nine data bytes of seven bits.
    check_range_contract(&RangeContract {
        valid: &[0, 61, 62],
        invalid: &[63, 255],
        new: BitmapPosition::new,
        get: BitmapPosition::get,
        of: Some(BitmapPosition::of),
    });
}

#[test]
fn a_mutually_exclusive_set_is_numbered_1_to_7() {
    // M2-125 Table 5: MES 1 to MES 7.
    check_range_contract(&RangeContract {
        valid: &[1, 2, 7],
        invalid: &[0, 8, 255],
        new: MutuallyExclusiveSet::new,
        get: MutuallyExclusiveSet::get,
        of: Some(MutuallyExclusiveSet::of),
    });
}

#[test]
fn the_controls_are_table_8_and_the_per_note_controllers_of_section_6_1() {
    // M2-125 Table 8 (controller numbers) and §5.1, §5.2 (the channel CCs).
    let rows = [
        (DrumControl::Volume, 7, Some(7)),
        (DrumControl::Pan, 10, None),
        (DrumControl::Timbre, 71, None),
        (DrumControl::ReleaseTime, 72, None),
        (DrumControl::AttackTime, 73, None),
        (DrumControl::Brightness, 74, None),
        (DrumControl::DecayTime, 75, None),
        (DrumControl::ReverbSend, 91, Some(91)),
    ];
    assert_eq!(rows.map(|r| r.0), DrumControl::ALL);
    for (control, number, cc) in rows {
        assert_eq!(control.number().get(), number, "{control:?}");
        assert_eq!(control.per_note().number(), number, "{control:?}");
        assert_eq!(control.channel_cc(), cc.map(CCNumber::of), "{control:?}");
    }
    assert_eq!(
        DrumControl::Volume.per_note(),
        RegisteredPerNoteController::Volume
    );
}

#[test]
fn the_defaults_are_the_figures_of_section_5_and_6() {
    // §5.1 and §6.1.1: Volume 0xC8000000; §6.1.3 to §6.1.7: 0x80000000.
    let default = |c: DrumControl| c.default_value().map(|v| v.get());
    assert_eq!(default(DrumControl::Volume), Some(0xC800_0000));
    for c in [
        DrumControl::Timbre,
        DrumControl::ReleaseTime,
        DrumControl::AttackTime,
        DrumControl::Brightness,
        DrumControl::DecayTime,
    ] {
        assert_eq!(default(c), Some(0x8000_0000), "{c:?}");
    }
    assert_eq!(default(DrumControl::Pan), None);
    assert_eq!(default(DrumControl::ReverbSend), None);
}

#[test]
fn the_optional_features_target_is_01() {
    // M2-125 §7.1.1, Table 9.
    assert_eq!(DrumFeatures::INQUIRY_TARGET.get(), U7::of(0x01));
}

#[test]
fn the_features_data_is_the_bytes_of_table_11() {
    // Byte 1: D0 per-note, D1 key-based; byte 2 D0 is note 27, byte 3 D1 is
    // note 35, byte 10 D5 is note 88 (Table 11).
    let all = DrumFeatures {
        per_note_controllers: true,
        key_based_controllers: true,
        sounds: DrumInstrument::all().collect(),
    };
    assert_eq!(
        all.to_data(),
        [0x03, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x3F]
    );
    let some = DrumFeatures {
        per_note_controllers: false,
        key_based_controllers: true,
        sounds: [27, 35, 88]
            .map(|n| DrumInstrument::of_note(U7::of(n)).unwrap())
            .into_iter()
            .collect(),
    };
    assert_eq!(
        some.to_data(),
        [0x02, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20]
    );
    assert_eq!(DrumFeatures::default().to_data(), [0; 10]);
}

#[test]
fn features_data_reads_back_and_refuses_what_table_11_reserves() {
    let some = DrumFeatures {
        per_note_controllers: true,
        key_based_controllers: false,
        sounds: [36, 38, 42]
            .map(|n| DrumInstrument::of_note(U7::of(n)).unwrap())
            .into_iter()
            .collect(),
    };
    assert_eq!(DrumFeatures::from_data(&some.to_data()), Some(some));
    let base = some.to_data();
    // Byte 1 D2 to D6, byte 10 D6: reserved.
    for (at, bit) in [(0, 2), (0, 6), (9, 6)] {
        let mut data = base;
        data[at] |= 1 << bit;
        assert_eq!(
            DrumFeatures::from_data(&data),
            None,
            "byte {} D{bit}",
            at + 1
        );
    }
    let mut high = base;
    high[3] |= 0x80;
    assert_eq!(DrumFeatures::from_data(&high), None);
    assert_eq!(DrumFeatures::from_data(&base[..9]), None);
    assert_eq!(
        DrumFeatures::from_data(&[base.as_slice(), &[0]].concat()),
        None
    );
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    // M2-125 §3: the Profile works on a Channel.
    let ch = MidiChannel::of(9);
    for level in [DrumNoteMapLevel::Partial, DrumNoteMapLevel::MeetsMinimum] {
        let on = profile_table(channel(9), level.profile(), true);
        assert_eq!(DrumNoteMapLevel::enabled_on(&on, ch), Some(level));
        assert_eq!(DrumNoteMapLevel::enabled_on(&on, MidiChannel::of(8)), None);
        let off = profile_table(channel(9), level.profile(), false);
        assert_eq!(DrumNoteMapLevel::enabled_on(&off, ch), None);
    }
    let group = profile_table(
        CiDeviceId::Group,
        DrumNoteMapLevel::MeetsMinimum.profile(),
        true,
    );
    assert_eq!(DrumNoteMapLevel::enabled_on(&group, ch), None);
}

#[test]
fn the_details_messages_are_tables_9_and_10_with_the_drum_profile_id() {
    use super::super::testing::{DST, header_on, preamble, to_dst};
    // M2-125 Table 9: the inquiry names the Profile and target 0x01.
    let inquiry = CiMessage::ProfileDetailsInquiry {
        header: header_on(channel(1)),
        to: Muid::of(DST),
        profile: DrumNoteMapLevel::MeetsMinimum.profile(),
        target: DrumFeatures::INQUIRY_TARGET,
    };
    let mut want = preamble(0x01, 0x28, to_dst());
    want.extend([0x7E, 0x20, 0x03, 0x01, 0x01, 0x01]);
    assert_eq!(inquiry.encode(), want);

    // Table 10, with the data length of ten bytes that Table 11 defines: all
    // 62 sounds, both controller kinds.
    let features = DrumFeatures {
        per_note_controllers: true,
        key_based_controllers: true,
        sounds: DrumInstrument::all().collect(),
    };
    let data = DetailsData::new(features.to_data().to_vec()).unwrap();
    let reply = CiMessage::ProfileDetailsReply {
        header: header_on(channel(1)),
        to: Muid::of(DST),
        profile: DrumNoteMapLevel::MeetsMinimum.profile(),
        target: DrumFeatures::INQUIRY_TARGET,
        data,
    };
    let mut want = preamble(0x01, 0x29, to_dst());
    want.extend([0x7E, 0x20, 0x03, 0x01, 0x01, 0x01, 0x0A, 0x00]);
    want.extend([0x03, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x3F]);
    assert_eq!(reply.encode(), want);
}
