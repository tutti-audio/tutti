use tutti_ump::{
    Articulation, Classification, CoreSound, CustomClassification, MidiChannel, NoteArticulation,
    NoteNumber, NoteVelocity, ShortSound, U4, Voice2,
};

use super::super::testing::{self, channel, header_on};
use super::super::*;

fn u4(raw: u8) -> U4 {
    U4::of(raw)
}

fn counts(raw: [u8; 16]) -> SubclassCounts {
    SubclassCounts::new(raw.map(VariationCount::of))
}

fn table(address: CiDeviceId, enabled: bool) -> ProfileTable {
    testing::table_with(address, ArticulationLevel::One.profile(), enabled)
}

#[test]
fn the_profile_id_is_7e_21_01_01_01() {
    // M2-123 §3.2, Table 4.
    assert_eq!(
        ArticulationLevel::One.profile().to_bytes(),
        [0x7E, 0x21, 0x01, 0x01, 0x01]
    );
    assert_eq!(ProfileId::ARTICULATION, ArticulationLevel::One.profile());
}

#[test]
fn the_profile_is_per_channel_and_only_an_enabled_one_counts() {
    // M2-123 §3.1: a Single Channel Profile.
    let ch = MidiChannel::of(5);
    assert_eq!(
        ArticulationLevel::enabled_on(&table(channel(5), true), ch),
        Some(ArticulationLevel::One)
    );
    assert_eq!(
        ArticulationLevel::enabled_on(&table(channel(5), false), ch),
        None
    );
    assert_eq!(
        ArticulationLevel::enabled_on(&table(channel(5), true), MidiChannel::of(6)),
        None
    );
    assert_eq!(
        ArticulationLevel::enabled_on(&table(CiDeviceId::Group, true), ch),
        None
    );
    // Another Profile enabled on the channel is not this one.
    let other = testing::table_with(channel(5), ProfileId::MPE, true);
    assert_eq!(ArticulationLevel::enabled_on(&other, ch), None);
}

#[test]
fn an_attribute_is_read_as_an_articulation_only_while_the_profile_is_enabled() {
    // M2-123 §5.1: otherwise the receiver decides which articulation to use.
    let ch = MidiChannel::of(5);
    let on = NoteArticulation::new(Articulation::Short(ShortSound::Pizzicato))
        .note_on(NoteNumber::of(60), NoteVelocity::new(0x8000));
    assert_eq!(
        ArticulationLevel::read(&table(channel(5), false), ch, &on),
        None
    );
    assert_eq!(
        ArticulationLevel::read(&table(channel(5), true), ch, &on),
        Some(Ok(NoteArticulation::new(Articulation::Short(
            ShortSound::Pizzicato
        ))))
    );
    let plain = Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000));
    assert_eq!(
        ArticulationLevel::read(&table(channel(5), true), ch, &plain),
        Some(Err(tutti_ump::ArticulationError::NotAnArticulation(0x00)))
    );
}

#[test]
fn the_inquiry_targets_are_01_and_40() {
    // M2-123 §9.1.1, §9.2.1.
    assert_eq!(ArticulationDetails::TARGET.get().get(), 0x01);
    assert_eq!(CustomSounds::TARGET.get().get(), 0x40);
}

/// The 16 count bytes of the eight Classifications `0x10` to `0x17`, typed out.
const STANDARD: [[u8; 16]; 8] = [
    [16, 16, 4, 2, 1, 1, 3, 1, 2, 2, 1, 0, 0, 0, 0, 0],
    [3, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0],
    [2, 1, 0, 0, 4, 4, 4, 2, 2, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [5, 5, 5, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16],
];

fn standard_bytes(bitmap: [u8; 2]) -> Vec<u8> {
    let mut bytes = bitmap.to_vec();
    for row in STANDARD {
        bytes.extend(row);
    }
    bytes
}

fn details(features: ArticulationFeatures) -> ArticulationDetails {
    ArticulationDetails::new(features, STANDARD.map(counts))
}

#[test]
fn the_details_reply_is_130_bytes_a_bitmap_then_16_counts_per_classification() {
    // M2-123 §9.1.2, Tables 21 and 22; Figure 7: D0 is the least significant bit
    // of byte 1 and D7 of byte 2 is bit 0 of byte 2.
    assert_eq!(ArticulationDetails::LEN, 130);
    let features = ArticulationFeatures {
        mute_type: true,
        note_off_velocity: true,
        custom_sound_discovery: true,
        ..ArticulationFeatures::default()
    };
    // D0, D3 and D4: 0b001_1001.
    let data = details(features).to_data();
    assert_eq!(data.as_bytes(), standard_bytes([0x19, 0x00]));
    assert_eq!(ArticulationDetails::from_data(&data), Ok(details(features)));
}

#[test]
fn each_feature_bit_is_its_own_bit_d0_to_d4() {
    // M2-123 Table 21.
    let each: [(u8, fn(&mut ArticulationFeatures)); 5] = [
        (0x01, |f| f.mute_type = true),
        (0x02, |f| f.mute_amount = true),
        (0x04, |f| f.playing_position = true),
        (0x08, |f| f.note_off_velocity = true),
        (0x10, |f| f.custom_sound_discovery = true),
    ];
    for (bit, set) in each {
        let mut features = ArticulationFeatures::default();
        set(&mut features);
        assert_eq!(
            details(features).to_data().as_bytes()[..2],
            [bit, 0x00],
            "{features:?}"
        );
    }
}

#[test]
fn a_reserved_feature_bit_is_refused() {
    // M2-123 Table 21: D5 to D13 are reserved. D5 is bit 5 of byte 1; D7 and D13
    // are bits 0 and 6 of byte 2 (Figure 7).
    for (bytes, bits) in [
        ([0x20, 0], 0x20u16),
        ([0x00, 0x01], 0x80),
        ([0, 0x40], 0x2000),
    ] {
        let data = DetailsData::new(standard_bytes(bytes)).unwrap();
        assert_eq!(
            ArticulationDetails::from_data(&data),
            Err(ArticulationDetailsError::ReservedFeatures(bits))
        );
    }
}

#[test]
fn the_counts_are_read_by_classification_and_subclass() {
    // M2-123 Tables 22 and 23.
    let data = DetailsData::new(standard_bytes([0, 0])).unwrap();
    let read = ArticulationDetails::from_data(&data).unwrap();
    let at = |class, subclass| read.counts(class).unwrap().get(u4(subclass)).get();
    assert_eq!(at(Classification::Core, 0), 16);
    assert_eq!(at(Classification::Core, 2), 4);
    assert_eq!(at(Classification::Shorts, 1), 1);
    assert_eq!(at(Classification::Gestures, 3), 5);
    assert_eq!(at(Classification::Runs, 0), 7);
    assert_eq!(at(Classification::Effects, 15), 16);
    assert_eq!(
        read.counts(Classification::SustainedColors)
            .unwrap()
            .get(u4(0))
            .get(),
        0
    );
    // Custom Classifications are not in this reply.
    assert_eq!(
        read.counts(Classification::Custom(CustomClassification::C1A)),
        None
    );
}

#[test]
fn a_details_reply_of_the_wrong_length_or_with_a_count_above_16_is_refused() {
    let short = DetailsData::new(vec![0; 129]).unwrap();
    assert_eq!(
        ArticulationDetails::from_data(&short),
        Err(ArticulationDetailsError::Length {
            expected: 130,
            got: 129
        })
    );
    // Byte 2 + 16 + 3 is Subclass 3 of Classification 0x11.
    let mut bytes = standard_bytes([0, 0]);
    bytes[2 + 16 + 3] = 17;
    assert_eq!(
        ArticulationDetails::from_data(&DetailsData::new(bytes).unwrap()),
        Err(ArticulationDetailsError::Count {
            index: 21,
            value: 17
        })
    );
}

#[test]
fn the_custom_reply_is_96_bytes_one_declaration_per_custom_classification() {
    // M2-123 §9.2.2, Tables 24 and 25.
    assert_eq!(CustomSounds::LEN, 96);
    let mut sounds = [SubclassCounts::default(); 6];
    // Classification 0x1C (third) Subclass 5, and 0x1F (sixth) Subclass 0xF.
    sounds[2].set(u4(5), VariationCount::of(9));
    sounds[5].set(u4(0xF), VariationCount::of(16));
    let custom = CustomSounds::new(sounds);
    let data = custom.to_data();
    let mut expected = vec![0u8; 96];
    expected[2 * 16 + 5] = 9;
    expected[5 * 16 + 15] = 16;
    assert_eq!(data.as_bytes(), expected);
    assert_eq!(CustomSounds::from_data(&data), Ok(custom.clone()));
    assert_eq!(custom.counts(CustomClassification::C1C).get(u4(5)).get(), 9);
    assert_eq!(
        custom.counts(CustomClassification::C1F).get(u4(0xF)).get(),
        16
    );
    assert_eq!(custom.counts(CustomClassification::C1A).get(u4(5)).get(), 0);
}

#[test]
fn a_custom_reply_of_the_wrong_length_or_with_a_count_above_16_is_refused() {
    assert_eq!(
        CustomSounds::from_data(&DetailsData::new(vec![0; 130]).unwrap()),
        Err(ArticulationDetailsError::Length {
            expected: 96,
            got: 130
        })
    );
    let mut bytes = vec![0u8; 96];
    bytes[40] = 100;
    assert_eq!(
        CustomSounds::from_data(&DetailsData::new(bytes).unwrap()),
        Err(ArticulationDetailsError::Count {
            index: 40,
            value: 100
        })
    );
}

#[test]
fn a_variation_count_is_at_most_16() {
    assert_eq!(VariationCount::new(16).map(VariationCount::get), Some(16));
    assert_eq!(VariationCount::new(17), None);
}

#[test]
fn the_details_reply_travels_in_the_profile_details_message() {
    // M2-101 §7.7: the reply carries the target and its data.
    let reply = CiMessage::ProfileDetailsReply {
        header: header_on(channel(2)),
        to: Muid::of(testing::PEER),
        profile: ProfileId::ARTICULATION,
        target: ArticulationDetails::TARGET,
        data: details(ArticulationFeatures::default()).to_data(),
    };
    let bytes = reply.encode();
    assert_eq!(testing::decoded(&bytes), reply);
    // The Inquiry Target and the length follow the Profile Id: 130 is 0x02 0x01
    // as two 7-bit bytes, least significant first.
    let at = bytes
        .windows(5)
        .position(|w| w == [0x7E, 0x21, 0x01, 0x01, 0x01])
        .unwrap();
    assert_eq!(bytes[at + 5..at + 8], [0x01, 0x02, 0x01]);
}

fn set(core: &[(u8, u8)], shorts: &[(u8, u8)], custom: &[(u8, u8)]) -> SoundSet {
    let mut standard = [SubclassCounts::default(); 8];
    for &(subclass, n) in core {
        standard[0].set(u4(subclass), VariationCount::of(n));
    }
    for &(subclass, n) in shorts {
        standard[1].set(u4(subclass), VariationCount::of(n));
    }
    let mut sounds = [SubclassCounts::default(); 6];
    for &(subclass, n) in custom {
        sounds[1].set(u4(subclass), VariationCount::of(n));
    }
    SoundSet::new(
        ArticulationDetails::new(ArticulationFeatures::default(), standard),
        CustomSounds::new(sounds),
    )
}

fn sound(articulation: Articulation, variation: u8) -> Sound {
    Sound {
        articulation,
        variation: u4(variation),
    }
}

const PIZZICATO: Articulation = Articulation::Short(ShortSound::Pizzicato);
const STACCATISSIMO: Articulation = Articulation::Short(ShortSound::Staccatissimo);
const NORMAL_STACCATO: Articulation = Articulation::Short(ShortSound::NormalStaccatoOffString);

#[test]
fn a_sound_the_receiver_has_is_played_as_requested() {
    // M2-123 §6.1: the fallback applies only when there is no such sound.
    let have = set(&[(0, 1)], &[(0xA, 4)], &[]);
    assert_eq!(have.resolve(sound(PIZZICATO, 3)), Some(sound(PIZZICATO, 3)));
}

#[test]
fn a_missing_variation_falls_back_to_a_variation_of_the_same_subclass() {
    // M2-123 §6.1, first rule: 4 Variations of Pizzicato have indices 0 to 3.
    let have = set(&[(0, 1)], &[(0xA, 4), (0, 2)], &[]);
    assert_eq!(have.resolve(sound(PIZZICATO, 9)), Some(sound(PIZZICATO, 0)));
}

#[test]
fn a_missing_subclass_falls_back_to_subclass_0_of_the_same_classification() {
    // M2-123 §6.1, second rule.
    let have = set(&[(0, 1)], &[(0, 2)], &[]);
    assert_eq!(
        have.resolve(sound(STACCATISSIMO, 1)),
        Some(sound(NORMAL_STACCATO, 1))
    );
    // The requested Variation does not exist there either.
    assert_eq!(
        have.resolve(sound(STACCATISSIMO, 5)),
        Some(sound(NORMAL_STACCATO, 0))
    );
}

#[test]
fn a_missing_classification_falls_back_to_the_normal_sustain() {
    // M2-123 §6.1, third rule: Classification 1 (attribute type 0x10), Subclass 0.
    let have = set(&[(0, 3)], &[], &[]);
    assert_eq!(
        have.resolve(sound(PIZZICATO, 2)),
        Some(sound(Articulation::NORMAL, 2))
    );
    assert_eq!(
        have.resolve(sound(
            Articulation::Custom(CustomClassification::C1B, u4(7)),
            0
        )),
        Some(sound(Articulation::NORMAL, 0))
    );
}

#[test]
fn a_custom_sound_is_found_through_the_custom_declaration() {
    // M2-123 §5.2.10, §9.2.
    let have = set(&[(0, 1)], &[], &[(7, 2)]);
    let wanted = Articulation::Custom(CustomClassification::C1B, u4(7));
    assert_eq!(have.resolve(sound(wanted, 1)), Some(sound(wanted, 1)));
    assert_eq!(have.variations(wanted).get(), 2);
}

#[test]
fn a_receiver_without_the_normal_sustain_has_nothing_to_fall_back_to() {
    // M2-123 §5.2.1 requires the sound; a receiver without it breaks the Profile.
    let have = set(&[(2, 1)], &[], &[]);
    assert_eq!(have.resolve(sound(PIZZICATO, 0)), None);
    assert_eq!(
        have.resolve(sound(Articulation::Core(CoreSound::Legato), 0)),
        Some(sound(Articulation::Core(CoreSound::Legato), 0))
    );
}
