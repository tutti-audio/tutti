use super::*;
use crate::{Form, text_packets};

fn collect<const W: usize>(bytes: &[u8]) -> Vec<(Form, [u8; W])> {
    text_packets::<W>(bytes).collect()
}

// The longest text of each kind, from the packet counts of M2-104 §7.1.4,
// §7.1.5 and §7.1.9.
/// Spec: m2-104/7.1.4#3, m2-104/7.1.5#6, m2-104/7.1.9#5, m2-124/4.4#6
#[test]
fn a_name_holds_exactly_what_its_packets_carry() {
    assert!(EndpointName::new(&"a".repeat(98)).is_some());
    assert!(EndpointName::new(&"a".repeat(99)).is_none());
    assert!(BlockName::new(&"a".repeat(91)).is_some());
    assert!(BlockName::new(&"a".repeat(92)).is_none());
    assert!(ProductInstanceId::new(&"a".repeat(42)).is_some());
    assert!(ProductInstanceId::new(&"a".repeat(43)).is_none());
}

#[test]
fn a_nul_cannot_be_in_a_name_because_it_ends_one() {
    assert!(EndpointName::new("a\0b").is_none());
    assert_eq!(EndpointName::new("").unwrap().as_str(), "");
    assert!(EndpointName::new("").unwrap().is_empty());
    assert_eq!(EndpointName::new("Pièce 5").unwrap().as_str(), "Pièce 5");
}

// M2-104 §1.8: MIDI messages carry only NFC-normalized UTF-8 text with no Byte
// Order Mark. A name is composed
// to NFC (at the constructor and at the wire) and a leading BOM is refused.
#[test]
fn a_name_is_held_in_nfc() {
    // "e" and a combining acute accent compose to the one character "é".
    let decomposed = "Cafe\u{301}";
    let composed = "Caf\u{e9}";
    assert_eq!(EndpointName::new(decomposed).unwrap().as_str(), composed);
    assert_eq!(
        BlockName::new(decomposed).unwrap().as_bytes(),
        composed.as_bytes()
    );
    // A name that is NFC already is kept as it is.
    assert_eq!(EndpointName::new(composed).unwrap().as_str(), composed);
    // The bound is on the composed bytes: 49 "e"+accent are 147 bytes as sent
    // and 98 composed, which fits an Endpoint Name; a fiftieth does not.
    assert!(EndpointName::new(&"e\u{301}".repeat(49)).is_some());
    assert!(EndpointName::new(&"e\u{301}".repeat(50)).is_none());
}

/// Spec: m2-104/1.8#2
#[test]
fn a_leading_byte_order_mark_is_refused() {
    let bom = "\u{FEFF}Piano";
    assert!(EndpointName::new(bom).is_none());
    assert!(BlockName::new(bom).is_none());
    assert!(EndpointName::new("Piano").is_some());
}

// §7.1.5: ASCII 32 to 126.
/// Spec: m2-104/7.1.5#4
#[test]
fn an_instance_id_is_printable_ascii() {
    assert!(ProductInstanceId::new(" ~").is_some());
    assert!(ProductInstanceId::new("\u{1f}").is_none());
    assert!(ProductInstanceId::new("\u{7f}").is_none());
    assert!(ProductInstanceId::new("é").is_none());
}

// §7.1.4: a message under 15 bytes is Complete, else a Start, up to five
// Continue and an End.
/// Spec: m2-104/7.1.4#3, m2-104/7.1.4#4, m2-104/7.1.4#5
#[test]
fn packets_are_complete_start_continue_end() {
    // Each filter bit set is owed a reply, so an empty name is a Complete of zeros.
    assert_eq!(collect::<14>(b""), [(Form::Complete, [0; 14])]);
    let one = collect::<14>(&[b'a'; 14]);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].0, Form::Complete);
    let two = collect::<14>(&[b'a'; 15]);
    assert_eq!(
        two.iter().map(|p| p.0).collect::<Vec<_>>(),
        [Form::Start, Form::End]
    );
    assert_eq!(two[1].1[..2], [b'a', 0]);
    let seven = collect::<14>(&[b'a'; 98]);
    assert_eq!(
        seven.iter().map(|p| p.0).collect::<Vec<_>>(),
        [
            Form::Start,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::End
        ]
    );
    assert!(seven[6].1.iter().all(|&b| b == b'a'));
}

// §7.1.4, §7.1.5, §7.1.9: a name at its limit is seven, seven and three packets.
/// Spec: m2-104/7.1.4#3, m2-104/7.1.5#6, m2-104/7.1.9#5
#[test]
fn the_longest_names_are_seven_seven_and_three_packets() {
    assert_eq!(
        collect::<14>(EndpointName::new(&"a".repeat(98)).unwrap().as_bytes()).len(),
        7
    );
    assert_eq!(
        collect::<13>(BlockName::new(&"a".repeat(91)).unwrap().as_bytes()).len(),
        7
    );
    assert_eq!(
        collect::<14>(
            ProductInstanceId::new(&"a".repeat(42))
                .unwrap()
                .as_str()
                .as_bytes()
        )
        .len(),
        3
    );
}

// A refusal says which rule it broke, and the text types read, show and parse
// as text.
#[test]
fn a_refused_text_says_why() {
    assert_eq!(
        EndpointName::parse(&"a".repeat(99)),
        Err(NameError::TooLong { max: 98 })
    );
    assert_eq!(
        BlockName::parse(&"a".repeat(92)),
        Err(NameError::TooLong { max: 91 })
    );
    assert_eq!(EndpointName::parse("a\0b"), Err(NameError::Nul));
    assert_eq!(EndpointName::parse("\u{FEFF}Keys"), Err(NameError::Bom));
    assert_eq!(
        ProductInstanceId::parse("a\0"),
        Err(InstanceIdError::NotPrintableAscii)
    );
    assert_eq!(
        ProductInstanceId::parse("caf\u{e9}"),
        Err(InstanceIdError::NotPrintableAscii)
    );
    assert_eq!(
        ProductInstanceId::parse(&"a".repeat(43)),
        Err(InstanceIdError::TooLong { max: 42 })
    );
}

#[test]
fn the_text_types_convert_display_and_parse() {
    let name: EndpointName = "Keystep".parse().unwrap();
    assert_eq!(name.to_string(), "Keystep");
    assert_eq!(name.as_ref() as &str, "Keystep");
    assert_eq!(BlockName::try_from("Keys").unwrap().as_str(), "Keys");
    let id: ProductInstanceId = "SN-0001".parse().unwrap();
    assert_eq!(
        (id.to_string(), id.as_ref() as &str),
        ("SN-0001".into(), "SN-0001")
    );
    assert!("a\0".parse::<EndpointName>().is_err());
}

// §7.1.5: the Id is ASCII 32-126 and at most 42 bytes; the 128 random bits are
// 32 lowercase hex digits, leading zeros kept.
#[test]
fn a_random_product_instance_id_is_32_lowercase_hex_digits() {
    let id = |bytes| ProductInstanceId::from_random(bytes);
    assert_eq!(id([0; 16]).as_str(), "0".repeat(32));
    assert_eq!(id([0xAB; 16]).as_str(), "ab".repeat(16));
    let mixed = [
        0x00, 0x01, 0x0F, 0x10, 0x7E, 0x7F, 0x80, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF, 0x09, 0xA0, 0x00,
        0x5C,
    ];
    assert_eq!(id(mixed).as_str(), "00010f107e7f80ffdeadbeef09a0005c");
}

#[test]
fn product_instance_ids_order_by_their_text() {
    let id = |text| ProductInstanceId::new(text).unwrap();
    assert!(id("a") < id("ab"));
    assert!(id("ab") < id("b"));
    assert!(id("") < id("a"));
    assert!(id("Z") < id("a"));
    assert_eq!(id("x").cmp(&id("x")), std::cmp::Ordering::Equal);
}

/// `truncated` keeps a name that fits whole and cuts a longer one at the last
/// character boundary within the bound, so the cut text is valid UTF-8 and never
/// longer than the type holds: a two-byte character that would straddle byte 91
/// is dropped whole.
#[test]
fn a_truncated_name_is_cut_at_a_character_boundary() {
    assert_eq!(BlockName::truncated("Keys").unwrap().as_str(), "Keys");
    let fits = "a".repeat(91);
    assert_eq!(BlockName::truncated(&fits).unwrap().as_str(), fits);
    assert_eq!(
        BlockName::truncated(&"a".repeat(200)).unwrap().as_str(),
        fits
    );
    // 90 bytes, then an e-acute of two bytes across the bound.
    let straddling = format!("{}é", "a".repeat(90));
    assert_eq!(straddling.len(), 92);
    assert_eq!(
        BlockName::truncated(&straddling).unwrap().as_str(),
        "a".repeat(90)
    );
    // 89 bytes, then the same character fits exactly.
    let fitting = format!("{}é", "a".repeat(89));
    assert_eq!(BlockName::truncated(&fitting).unwrap().as_str(), fitting);
}

/// What no name can hold is refused by `truncated` as by `new`: a NUL and a
/// Byte Order Mark. Only the length is cut.
#[test]
fn a_truncated_name_still_refuses_a_nul_and_a_bom() {
    assert_eq!(BlockName::truncated("a\0b"), None);
    assert_eq!(BlockName::truncated("\u{FEFF}a"), None);
    let long_with_nul = format!("{}\0", "a".repeat(200));
    assert_eq!(BlockName::truncated(&long_with_nul), None);
}

/// A multibyte character that would straddle the bound is refused whole: 90
/// bytes and an e-acute is 92, 89 and one is exactly 91.
#[test]
fn a_character_straddling_the_bound_is_refused_not_split() {
    let straddling = format!("{}é", "a".repeat(90));
    assert_eq!(
        BlockName::new(&straddling),
        None,
        "an e-acute across byte 91 is refused"
    );
    assert_eq!(
        BlockName::parse(&straddling),
        Err(NameError::TooLong { max: 91 })
    );
    let exact = format!("{}é", "a".repeat(89));
    assert_eq!(BlockName::new(&exact).unwrap().as_bytes().len(), 91);
}

#[cfg(feature = "serde")]
mod serde_text {
    use super::*;

    /// A name and an Id serialise as their bare text and read back; a decode
    /// that is not a name is refused.
    #[test]
    fn the_text_types_round_trip_as_text_and_refuse_a_bad_decode() {
        let name = EndpointName::new("Cafe\u{301}").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"Caf\u{e9}\"");
        assert_eq!(
            serde_json::from_str::<EndpointName>("\"Caf\u{e9}\"").unwrap(),
            name
        );
        let id = ProductInstanceId::new("SN-0001").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"SN-0001\"");
        assert_eq!(
            serde_json::from_str::<ProductInstanceId>("\"SN-0001\"").unwrap(),
            id
        );
        assert!(serde_json::from_str::<EndpointName>(&format!("\"{}\"", "a".repeat(99))).is_err());
        assert!(serde_json::from_str::<EndpointName>("\"\\u0000\"").is_err());
        assert!(serde_json::from_str::<ProductInstanceId>("\"caf\u{e9}\"").is_err());
        assert!(
            serde_json::from_str::<ProductInstanceId>(&format!("\"{}\"", "a".repeat(43))).is_err()
        );
    }
}

/// M2-104 §7.1.5 sets no minimum, so an empty Id is one.
#[test]
fn an_empty_instance_id_is_accepted() {
    assert_eq!(ProductInstanceId::new("").unwrap().as_str(), "");
}

/// The character check runs before the length: text that is too long and also
/// holds a non-printable byte is refused as `NotPrintableAscii`.
#[test]
fn an_instance_id_names_its_character_before_its_length() {
    let both = format!("{}\u{7f}", "a".repeat(50));
    assert_eq!(
        ProductInstanceId::parse(&both),
        Err(InstanceIdError::NotPrintableAscii)
    );
}
