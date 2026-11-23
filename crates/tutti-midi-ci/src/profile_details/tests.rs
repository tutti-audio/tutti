use tutti_ump::U7;

use super::super::*;

fn data(bytes: &[u8]) -> DetailsData {
    DetailsData::new(bytes.to_vec()).unwrap()
}

/// M2-121 Table 4 / M2-125 Table 4 / M2-123 Table 4 / M2-120 Table 4, typed out.
fn id(bank: u8, number: u8, version: u8, level: u8) -> ProfileId {
    ProfileId::Standard(StandardProfile::new(
        U7::of(bank),
        U7::of(number),
        U7::of(version),
        U7::of(level),
    ))
}

const ORGAN: ProfileId = ProfileId::Standard(StandardProfile::new(
    U7::of(0x20),
    U7::of(0x01),
    U7::of(0x01),
    U7::of(0x01),
));

const FEATURES: InquiryTarget = InquiryTarget::new(U7::of(0x01));

#[test]
fn number_of_channels_is_read_for_any_profile_by_the_registered_target() {
    // M2-102 §2.5.1 Table 8: in use then maximum, each 14 bits LSB first.
    let reply = data(&[0x01, 0x00, 0x04, 0x00]);
    let want = Some(ProfileDetails::Channels(ChannelsDetail {
        in_use: ProfileChannels::of(1),
        max: ProfileChannels::of(4),
    }));
    assert_eq!(
        reply.decode(
            id(0x31, 0x00, 0x01, 0x01),
            InquiryTarget::NUMBER_OF_CHANNELS
        ),
        want
    );
    assert_eq!(reply.decode(ORGAN, InquiryTarget::NUMBER_OF_CHANNELS), want);
}

#[test]
fn an_organ_reply_is_the_one_byte_bitmap_of_m2_121_table_5() {
    // D0 soft pedal, D2 percussion, D4 crosstalk.
    let reply = data(&[0b1_0101]);
    let want = ProfileDetails::Organ(OrganFeatures {
        soft_pedal: true,
        percussion: true,
        crosstalk: true,
        ..Default::default()
    });
    for level in [0x00, 0x01, 0x02] {
        assert_eq!(
            reply.decode(id(0x20, 0x01, 0x01, level), FEATURES),
            Some(want.clone())
        );
    }
}

#[test]
fn a_drum_reply_is_the_ten_bytes_of_m2_125_table_11() {
    // Byte 1 D0 and D1 set; byte 2 D0 is Note 27 (High Q, Table 5).
    let mut bytes = [0u8; 10];
    bytes[0] = 0b11;
    bytes[1] = 0b1;
    let got = data(&bytes).decode(id(0x20, 0x03, 0x01, 0x01), FEATURES);
    let Some(ProfileDetails::Drum(features)) = got else {
        panic!("not a drum reply: {got:?}");
    };
    assert!(features.per_note_controllers && features.key_based_controllers);
    let notes: Vec<u8> = features.sounds.iter().map(|i| i.note().get()).collect();
    assert_eq!(notes, [27]);
}

#[test]
fn an_articulation_reply_is_130_bytes_and_its_custom_reply_96() {
    // M2-123 §9.1.2: bitmap D0 mute type and D2 playing position, then the
    // Core Classification's Subclass 0 has 3 Variations.
    let mut bytes = vec![0u8; 130];
    bytes[0] = 0b101;
    bytes[2] = 3;
    let got = data(&bytes).decode(ProfileId::ARTICULATION, FEATURES);
    let Some(ProfileDetails::Articulation(details)) = got else {
        panic!("not an articulation reply: {got:?}");
    };
    assert!(details.features.mute_type && details.features.playing_position);
    assert!(!details.features.mute_amount && !details.features.note_off_velocity);
    let core = details.counts(tutti_ump::Classification::Core).unwrap();
    assert_eq!(core.get(tutti_ump::U4::of(0)).get(), 3);
    assert_eq!(core.get(tutti_ump::U4::of(1)).get(), 0);

    // §9.2.2: the first custom Classification's Subclass 0 has 2.
    let mut bytes = vec![0u8; 96];
    bytes[0] = 2;
    let got = data(&bytes).decode(ProfileId::ARTICULATION, InquiryTarget::new(U7::of(0x40)));
    assert!(
        matches!(got, Some(ProfileDetails::ArticulationCustom(_))),
        "{got:?}"
    );
}

#[test]
fn the_profile_id_and_target_decide_never_the_length() {
    let organ = data(&[0b1]);
    // The same one byte under the drum Profile, the MPE Profile, an unknown
    // Version of the organ Profile, an unknown target or the articulation
    // Profile is not an organ reply.
    assert_eq!(organ.decode(id(0x20, 0x03, 0x01, 0x01), FEATURES), None);
    assert_eq!(organ.decode(id(0x31, 0x00, 0x01, 0x01), FEATURES), None);
    assert_eq!(organ.decode(id(0x20, 0x01, 0x02, 0x01), FEATURES), None);
    assert_eq!(organ.decode(ORGAN, InquiryTarget::new(U7::of(0x40))), None);
    assert_eq!(organ.decode(ProfileId::ARTICULATION, FEATURES), None);

    // Ten bytes that are a valid drum reply are not an organ's or an
    // articulation's.
    let mut drum = [0u8; 10];
    drum[0] = 1;
    let drum = data(&drum);
    assert_eq!(drum.decode(ORGAN, FEATURES), None);
    assert_eq!(drum.decode(ProfileId::ARTICULATION, FEATURES), None);
    assert!(drum.decode(id(0x20, 0x03, 0x01, 0x00), FEATURES).is_some());

    // A reserved bit is not read as a reply.
    assert_eq!(data(&[0b10_0000]).decode(ORGAN, FEATURES), None);
}

#[test]
fn a_reply_of_the_wrong_length_is_none_whatever_the_profile() {
    // Table 8 is four bytes; M2-123 §9.1 is 130 and §9.2 is 96.
    assert_eq!(
        data(&[0x01, 0x00, 0x04]).decode(ORGAN, InquiryTarget::NUMBER_OF_CHANNELS),
        None
    );
    assert_eq!(
        data(&[0u8; 129]).decode(ProfileId::ARTICULATION, FEATURES),
        None
    );
    assert_eq!(
        data(&[0u8; 95]).decode(ProfileId::ARTICULATION, InquiryTarget::new(U7::of(0x40))),
        None
    );
}

#[test]
fn a_drum_reply_under_an_unknown_target_is_none() {
    let mut drum = [0u8; 10];
    drum[0] = 1;
    assert_eq!(
        data(&drum).decode(id(0x20, 0x03, 0x01, 0x01), InquiryTarget::new(U7::of(0x40))),
        None
    );
}
