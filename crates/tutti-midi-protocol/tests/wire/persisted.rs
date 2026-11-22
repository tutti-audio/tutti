//! The values a project saves read back through their checks: what is written
//! reads back equal, and a document that names no value is refused with its text.

use tutti_midi_protocol::{
    ChannelMask, KeyChange, KeyChanges, KeyTable, MtcRate, MtcTimeCode, OctaveOffsets,
    PerNotePolicy, PerNoteTranslation, SmpteFrameRate, SmpteTimecode, TimecodeHours,
    TimecodeMinutes, TimecodeSeconds, Timing, TranslationMode, TuningFrequency, TuningName,
};
use tutti_ump::{ExtremeValues, NoteNumber, PerNoteManagement, U7, U14};

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
}

/// **A timecode reads back through the label ranges of its rate**: 29.97 and 30
/// frames label 0..=29, 24 labels 0..=23, and hours stop at 23.
#[test]
fn a_timecode_reads_back_only_when_its_labels_are_in_range_for_its_rate() {
    for rate in SmpteFrameRate::ALL {
        let at = SmpteTimecode::new(
            TimecodeHours::of(23),
            TimecodeMinutes::of(59),
            TimecodeSeconds::of(58),
            23,
            rate,
        )
        .unwrap();
        assert_eq!(round_trip(&at), at);
    }
    let mut json = serde_json::to_value(SmpteTimecode::origin(SmpteFrameRate::Fps24)).unwrap();
    json["frames"] = 24.into();
    let refused = serde_json::from_value::<SmpteTimecode>(json).unwrap_err();
    assert!(refused.to_string().contains("out of range"), "{refused}");
    let mut json = serde_json::to_value(SmpteTimecode::origin(SmpteFrameRate::Fps30)).unwrap();
    json["hours"] = 24.into();
    assert!(serde_json::from_value::<SmpteTimecode>(json).is_err());

    for rate in MtcRate::ALL {
        let at = MtcTimeCode::new(
            rate,
            TimecodeHours::of(1),
            TimecodeMinutes::of(2),
            TimecodeSeconds::of(3),
            rate.labels_per_second() - 1,
        )
        .unwrap();
        assert_eq!(round_trip(&at), at);
        let mut json = serde_json::to_value(at).unwrap();
        json["frames"] = rate.labels_per_second().into();
        assert!(
            serde_json::from_value::<MtcTimeCode>(json).is_err(),
            "{rate:?}"
        );
    }
}

/// The translation choices read back as written.
#[test]
fn a_translation_mode_and_a_policy_read_back_as_written() {
    for extremes in [ExtremeValues::Scaled, ExtremeValues::Avoided] {
        let mode = TranslationMode {
            extremes,
            per_note: PerNoteTranslation::Dropped,
        };
        assert_eq!(round_trip(&mode), mode);
    }
    for policy in [
        PerNotePolicy::WhenAttached(PerNoteManagement::Detach),
        PerNotePolicy::EveryNote(PerNoteManagement::DetachAndReset),
    ] {
        assert_eq!(round_trip(&policy), policy);
    }
}

/// **A key table is exactly 128 words and a change list at most 127 (MTS)**: a
/// document with another count names no message.
#[test]
fn a_key_table_and_a_change_list_read_back_only_at_their_wire_counts() {
    let table = KeyTable::equal_temperament();
    assert_eq!(round_trip(&table), table);
    for len in [0usize, 127, 129] {
        let words = vec![TuningFrequency::NO_CHANGE; len];
        let json = serde_json::to_value(&words).unwrap();
        let refused = serde_json::from_value::<KeyTable>(json).unwrap_err();
        assert!(refused.to_string().contains("128"), "{len}: {refused}");
        assert!(
            refused.to_string().contains(&len.to_string()),
            "{len}: {refused}"
        );
    }

    let words = vec![TuningFrequency::NO_CHANGE; 5];
    assert_eq!(KeyTable::try_from(words).unwrap_err().count(), 5);

    let change = KeyChange {
        key: NoteNumber::of(60),
        frequency: TuningFrequency::from_parts(U7::of(61), U14::of(8192)),
    };
    let mut changes = KeyChanges::new();
    assert!(changes.push(change));
    assert_eq!(round_trip(&changes), changes);
    let too_many = serde_json::to_value(vec![change; 128]).unwrap();
    let refused = serde_json::from_value::<KeyChanges>(too_many).unwrap_err();
    assert!(refused.to_string().contains("127"), "{refused}");
    assert_eq!(
        KeyChanges::try_from(vec![change; 128]).unwrap_err().count(),
        128
    );
}

/// A frequency word is 21 bits, and the small tuning values keep their bits.
#[test]
fn the_tuning_values_read_back_and_a_frequency_above_21_bits_is_refused() {
    let word = TuningFrequency::from_parts(U7::of(69), U14::of(1));
    assert_eq!(round_trip(&word), word);
    assert!(serde_json::from_value::<TuningFrequency>(0x1F_FFFFu32.into()).is_ok());
    assert!(serde_json::from_value::<TuningFrequency>(0x20_0000u32.into()).is_err());

    assert_eq!(round_trip(&Timing::NonRealTime), Timing::NonRealTime);
    let mask = ChannelMask::from_bits(0x8001);
    assert_eq!(round_trip(&mask), mask);
    let name = TuningName::new([U7::of(0x41); 16]);
    assert_eq!(round_trip(&name), name);
    for offsets in [
        OctaveOffsets::ZERO,
        OctaveOffsets::Fine([U14::of(0x2000); 12]),
    ] {
        assert_eq!(round_trip(&offsets), offsets);
    }
}

/// **A zone has 1 to 15 member channels** (RP-053): the count type holds exactly
/// that range, and a persisted zone with another count is refused.
#[test]
fn a_member_count_keeps_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
    use tutti_midi_protocol::MemberCount;
    let counts = RangeContract {
        new: MemberCount::new,
        get: MemberCount::get,
        of: Some(MemberCount::of),
        valid: &[1, 7, 15],
        invalid: &[0, 16, 255],
    };
    check_range_contract(&counts);
    check_range_serde(&counts);
}
