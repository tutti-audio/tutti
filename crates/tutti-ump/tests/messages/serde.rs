//! The address types read back only what they would construct, and write the
//! bare number. Run with `--features serde`.
#![cfg(feature = "serde")]

use proptest::prelude::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use tutti_ump::{
    ArticulationAttribute, BendValue, CCNumber, ChordType, ControlValue, Field, FirstGroup,
    FunctionBlockCount, FunctionBlockId, GroupSpan, Message, MidiChannel, MidiGroup, MonoChannels,
    MpeChannels, NoteVelocity, Pitch, Reserved, ReservedAccents, ReservedAttribute, SharpsFlats,
    Sysex7Packet, Sysex8Packet, TicksPerQuarterNote, Ump,
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
    // max + 1, 200 and 255: refused, with the raw and the width in the message.
    for raw in [max + 1, 200, 255] {
        let err = serde_json::from_str::<T>(&raw.to_string()).unwrap_err();
        // The message names the number and the width.
        let message = err.to_string();
        for part in [what.to_string(), raw.to_string()] {
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
    check("within 4 bits", 15, MidiChannel::new, |v| v.get());
    check("within 4 bits", 15, MidiGroup::new, |v| v.get());
    check("within 7 bits", 127, CCNumber::new, |v| v.get());
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
    for raw in [0u16, 1, 0x8000, 0xABCD, u16::MAX] {
        let velocity = serde_json::to_string(&NoteVelocity::new(raw)).unwrap();
        assert_eq!(velocity, raw.to_string());
        assert_eq!(
            serde_json::from_str::<NoteVelocity>(&velocity).unwrap(),
            NoteVelocity::new(raw)
        );
    }
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

/// A reserved value is read back only when it is one: the decoder's own reserved
/// raws round-trip, and a raw in range, an attribute type a table defines, or
/// accents of a defined shape are refused.
#[test]
fn a_reserved_value_that_is_not_reserved_is_refused() {
    let reserved: Field<FunctionBlockId> = serde_json::from_str(r#"{"Reserved":40}"#).unwrap();
    assert_eq!(
        serde_json::to_string(&reserved).unwrap(),
        r#"{"Reserved":40}"#
    );
    let value: Field<FunctionBlockId> = serde_json::from_str(r#"{"Value":5}"#).unwrap();
    assert_eq!(value.value(), Some(FunctionBlockId::of(5)));
    assert!(serde_json::from_str::<Field<FunctionBlockId>>(r#"{"Reserved":5}"#).is_err());
    assert!(serde_json::from_str::<Field<FunctionBlockId>>(r#"{"Value":32}"#).is_err());

    let attribute: ReservedAttribute = serde_json::from_str("[4,9]").unwrap();
    assert_eq!((attribute.kind(), attribute.data()), (4, 9));
    for kind in [0u8, 3, 0x10, 0x1F] {
        assert!(serde_json::from_str::<ReservedAttribute>(&format!("[{kind},0]")).is_err());
    }

    let accents: ReservedAccents = serde_json::from_str("[0,3,0]").unwrap();
    assert_eq!(accents.parts(), [0, 3, 0]);
    for parts in ["[3,0,0]", "[3,2,0]", "[3,2,1]"] {
        assert!(
            serde_json::from_str::<ReservedAccents>(parts).is_err(),
            "{parts}"
        );
    }
}

/// A packet's bytes past its length are zero, as `take` and `new` leave them.
#[test]
fn a_packet_with_bytes_past_its_length_is_refused() {
    assert!(serde_json::from_str::<Sysex7Packet>(r#"["Complete",1,[5,0,0,0,0,0]]"#).is_ok());
    assert!(serde_json::from_str::<Sysex7Packet>(r#"["Complete",1,[5,9,0,0,0,0]]"#).is_err());
    let eight =
        |len: u8, tail: u8| format!(r#"["Complete",0,{len},[5,{tail},0,0,0,0,0,0,0,0,0,0,0]]"#);
    assert!(serde_json::from_str::<Sysex8Packet>(&eight(1, 0)).is_ok());
    assert!(serde_json::from_str::<Sysex8Packet>(&eight(1, 9)).is_err());
}

/// Every `u8` is a reserved raw of some field, so a `Reserved` is the bare
/// number for all 256, and a number that is no `u8` is refused.
#[test]
fn a_reserved_raw_is_the_bare_byte() {
    for raw in 0..=u8::MAX {
        let text = raw.to_string();
        let back: Reserved = serde_json::from_str(&text).unwrap();
        assert_eq!(back.get(), raw);
        assert_eq!(serde_json::to_string(&back).unwrap(), text);
    }
    assert!(serde_json::from_str::<Reserved>("256").is_err());
    assert!(serde_json::from_str::<Reserved>("-1").is_err());
}
