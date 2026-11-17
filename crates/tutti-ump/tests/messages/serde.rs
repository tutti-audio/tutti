//! The address types read back only what they would construct, and write the
//! bare number. Run with `--features serde`.
#![cfg(feature = "serde")]

use proptest::prelude::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use tutti_ump::{
    ArticulationAttribute, BendValue, CCNumber, ChordType, ControlValue, FirstGroup,
    FunctionBlockCount, FunctionBlockId, GroupSpan, Message, MidiChannel, MidiGroup, MonoChannels,
    MpeChannels, Pitch, SharpsFlats, Sysex7Packet, Sysex8Packet, TicksPerQuarterNote, Ump,
};

use crate::common;

fn check<T>(what: &str, max: u8, make: fn(u8) -> Option<T>, get: fn(&T) -> u8)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    // 0 and max: accepted, and written as the bare number.
    for raw in [0, max] {
        let value = make(raw).unwrap();
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(text, raw.to_string());
        let back: T = serde_json::from_str(&text).unwrap();
        assert_eq!(get(&back), raw);
        assert_eq!(back, value);
    }
    // max + 1, 200 and 255: refused, with the range in the message.
    for raw in [max + 1, 200, 255] {
        let err = serde_json::from_str::<T>(&raw.to_string()).unwrap_err();
        // The message names what was refused, the number and the limit.
        let message = err.to_string();
        for part in [what.to_string(), raw.to_string(), max.to_string()] {
            assert!(message.contains(&part), "{message} lacks {part}");
        }
    }
    // Not a `u8` at all.
    assert!(serde_json::from_str::<T>("-1").is_err());
    assert!(serde_json::from_str::<T>("256").is_err());
    assert!(serde_json::from_str::<T>("\"3\"").is_err());
}

#[test]
fn a_number_above_the_range_is_refused() {
    check("channel", 15, MidiChannel::new, |v| v.get());
    check("group", 15, MidiGroup::new, |v| v.get());
    check("controller number", 127, CCNumber::new, |v| v.get());
}

#[test]
fn the_bytes_are_the_bare_number() {
    assert_eq!(serde_json::to_string(&MidiChannel::of(3)).unwrap(), "3");
    assert_eq!(serde_json::to_string(&MidiGroup::of(15)).unwrap(), "15");
    assert_eq!(serde_json::to_string(&CCNumber::of(74)).unwrap(), "74");
    assert_eq!(
        serde_json::to_string(&[MidiChannel::of(1), MidiChannel::of(2)]).unwrap(),
        "[1,2]"
    );
}

/// A whole-range word is written as the bare number and read back unchanged.
#[test]
fn a_whole_range_word_is_the_bare_number() {
    for raw in [0u32, 1, 0x8000_0000, u32::MAX] {
        let control = serde_json::to_string(&ControlValue::new(raw)).unwrap();
        let bend = serde_json::to_string(&BendValue::new(raw)).unwrap();
        let pitch = serde_json::to_string(&Pitch::from_q7_25(raw)).unwrap();
        assert_eq!(
            (control.as_str(), bend.as_str(), pitch.as_str()),
            (&*raw.to_string(), &*raw.to_string(), &*raw.to_string())
        );
        assert_eq!(
            serde_json::from_str::<ControlValue>(&control).unwrap(),
            ControlValue::new(raw)
        );
        assert_eq!(
            serde_json::from_str::<BendValue>(&bend).unwrap(),
            BendValue::new(raw)
        );
        assert_eq!(
            serde_json::from_str::<Pitch>(&pitch).unwrap(),
            Pitch::from_q7_25(raw)
        );
    }
    assert!(serde_json::from_str::<ControlValue>("4294967296").is_err());
}

fn bounded_edges<T>(lo: i32, hi: i32)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    for raw in [lo, hi] {
        let value: T = serde_json::from_str(&raw.to_string()).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), raw.to_string());
    }
    for raw in [lo - 1, hi + 1] {
        assert!(
            serde_json::from_str::<T>(&raw.to_string()).is_err(),
            "{raw}"
        );
    }
}

/// Each range-checked field reads back its own inclusive range and refuses the
/// numbers one past either end.
#[test]
fn a_bounded_field_is_refused_one_past_either_end() {
    bounded_edges::<FunctionBlockId>(0, 31);
    bounded_edges::<FirstGroup>(0, 15);
    bounded_edges::<GroupSpan>(1, 16);
    bounded_edges::<MpeChannels>(2, 16);
    bounded_edges::<ChordType>(0, 0x1B);
    bounded_edges::<FunctionBlockCount>(0, 32);
    bounded_edges::<TicksPerQuarterNote>(1, 65535);
    bounded_edges::<MonoChannels>(0, 16);
    bounded_edges::<SharpsFlats>(-7, 7);
}

/// A packet read back keeps its length within its capacity.
#[test]
fn a_packet_longer_than_its_capacity_is_refused() {
    let seven = Sysex7Packet::new(tutti_ump::SysexStatus::Complete, &[1, 2, 3]).unwrap();
    let text = serde_json::to_string(&seven).unwrap();
    assert_eq!(serde_json::from_str::<Sysex7Packet>(&text).unwrap(), seven);
    assert!(serde_json::from_str::<Sysex7Packet>(r#"["Complete",7,[0,0,0,0,0,0]]"#).is_err());
    assert!(serde_json::from_str::<Sysex7Packet>(r#"["Complete",1,[128,0,0,0,0,0]]"#).is_err());
    assert!(
        serde_json::from_str::<Sysex8Packet>(r#"["Complete",0,14,[0,0,0,0,0,0,0,0,0,0,0,0,0]]"#)
            .is_err()
    );
}

/// An articulation attribute is of type 0x10 to 0x1F.
#[test]
fn an_articulation_attribute_outside_its_type_range_is_refused() {
    let ok: ArticulationAttribute = serde_json::from_str("[16,5]").unwrap();
    assert_eq!((ok.kind(), ok.data()), (0x10, 5));
    assert_eq!(serde_json::to_string(&ok).unwrap(), "[16,5]");
    for kind in [0x0Fu32, 0x20] {
        assert!(serde_json::from_str::<ArticulationAttribute>(&format!("[{kind},0]")).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(common::cases(20_000)))]

    /// Every message a packet decodes to is read back from its serde form
    /// equal to itself.
    #[test]
    fn a_decoded_message_survives_serde(w in any::<[u32; 4]>(), sel in any::<u8>()) {
        let ump = Ump(common::shape(w, sel));
        if let Ok(message) = ump.decode() {
            let text = serde_json::to_string(&message).unwrap();
            let back: Message = serde_json::from_str(&text).unwrap();
            prop_assert_eq!(back, message);
        }
    }
}
