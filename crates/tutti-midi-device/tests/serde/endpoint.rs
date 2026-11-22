//! An Endpoint is the value an app stores of what a device declared: it reads
//! back as the Endpoint it was, and a document whose parts disagree is refused.

use serde_json::{Value, json};
use tutti_midi_device::{Capabilities, Endpoint, FunctionBlock, Supports};
use tutti_ump::{
    BlockDirection, CiVersion, DeviceIdentity, FirstGroup, FunctionBlockId, GroupSpan,
    ManufacturerId, Midi1Port, SoftwareRevision, Sysex8StreamCount, U7, UiHint,
};
use tutti_ump::{BlockName, EndpointName, ProductInstanceId};

fn block(id: u8, direction: BlockDirection) -> FunctionBlock {
    FunctionBlock::builder(
        FunctionBlockId::of(id),
        direction,
        FirstGroup::of(id),
        GroupSpan::of(1),
    )
    .ui_hint(UiHint::Both)
    .midi1(Midi1Port::Unrestricted)
    .midi_ci(CiVersion::of(2), Sysex8StreamCount::new(4))
    .name("Keys".parse::<BlockName>().unwrap())
    .build()
    .unwrap()
}

fn endpoint() -> Endpoint {
    Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: false,
    }))
    .identified(DeviceIdentity {
        manufacturer: ManufacturerId::of(0x0042),
        family: [1, 0].map(U7::of),
        model: [7, 0].map(U7::of),
        revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
    })
    .named("Stage piano".parse::<EndpointName>().unwrap())
    .instanced(ProductInstanceId::new("serial-0001").unwrap())
    .block(block(0, BlockDirection::Bidirectional))
    .block(block(2, BlockDirection::Input))
}

fn read<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(value.clone())
}

#[test]
fn an_endpoint_reads_back_as_the_endpoint_it_was() {
    let endpoint = endpoint();
    let back: Endpoint = serde_json::from_str(&serde_json::to_string(&endpoint).unwrap()).unwrap();
    assert_eq!(back, endpoint);
    assert_eq!(back.declared_function_blocks(), 3);
    assert_eq!(
        back.function_block(FunctionBlockId::of(2)).unwrap().name(),
        Some(&"Keys".parse::<BlockName>().unwrap())
    );
}

#[test]
fn the_document_of_a_block_names_its_fields() {
    let value = serde_json::to_value(block(1, BlockDirection::Output)).unwrap();
    assert_eq!(value["id"], json!(1));
    assert_eq!(value["first_group"], json!(1));
    assert_eq!(value["groups"], json!(1));
    assert_eq!(value["name"], json!("Keys"));
}

#[test]
fn a_block_document_that_breaks_a_rule_of_its_parts_is_refused() {
    let good = serde_json::to_value(block(1, BlockDirection::Bidirectional)).unwrap();
    assert!(read::<FunctionBlock>(&good).is_ok());
    let patched = |field: &str, to: Value| {
        let mut value = good.clone();
        value[field] = to;
        value
    };
    // Groups 15 and 16 are the last two: three from Group 15 run past it.
    let mut past = patched("first_group", json!(15));
    past["groups"] = json!(2);
    past["midi1"] = json!("No");
    assert!(read::<FunctionBlock>(&past).is_err(), "{past}");
    // An output-only block is not primarily a receiver.
    let mut contradiction = patched("direction", json!("Output"));
    contradiction["ui_hint"] = json!("Receiver");
    assert!(
        read::<FunctionBlock>(&contradiction).is_err(),
        "{contradiction}"
    );
    // A MIDI 1.0 port spans one Group.
    let mut wide = patched("groups", json!(2));
    wide["midi1"] = json!("Restricted");
    assert!(read::<FunctionBlock>(&wide).is_err(), "{wide}");
    // Number 32 is not a Function Block number.
    assert!(read::<FunctionBlock>(&patched("id", json!(32))).is_err());
}

#[test]
fn an_endpoint_document_whose_blocks_disagree_with_its_count_is_refused() {
    let good = serde_json::to_value(endpoint()).unwrap();
    assert!(read::<Endpoint>(&good).is_ok());
    let mut past = good.clone();
    past["declared_blocks"] = json!(2);
    assert!(
        read::<Endpoint>(&past)
            .unwrap_err()
            .to_string()
            .contains("past the 2"),
        "block 2 is at the count of 2"
    );
    let mut twice = good.clone();
    let first = twice["blocks"][0].clone();
    twice["blocks"].as_array_mut().unwrap().push(first);
    assert!(
        read::<Endpoint>(&twice)
            .unwrap_err()
            .to_string()
            .contains("numbered 0")
    );
}

#[test]
fn a_name_is_its_text_and_is_read_back_through_its_checks() {
    let name = "Stage piano".parse::<EndpointName>().unwrap();
    assert_eq!(serde_json::to_string(&name).unwrap(), r#""Stage piano""#);
    assert_eq!(
        serde_json::from_str::<EndpointName>(r#""Stage piano""#).unwrap(),
        name
    );
    for bad in [
        serde_json::to_string(&"a".repeat(99)).unwrap(),
        r#""a\u0000b""#.to_owned(),
        r#""﻿name""#.to_owned(),
    ] {
        assert!(serde_json::from_str::<EndpointName>(&bad).is_err(), "{bad}");
    }
    // A Block Name holds 91 bytes, not the 98 of an Endpoint Name.
    let ninety_two = serde_json::to_string(&"a".repeat(92)).unwrap();
    assert!(serde_json::from_str::<BlockName>(&ninety_two).is_err());
    assert!(serde_json::from_str::<EndpointName>(&ninety_two).is_ok());
}

#[test]
fn what_an_endpoint_supports_has_at_least_one_protocol() {
    let supports = Supports::of(Capabilities {
        midi2: true,
        ..Capabilities::default()
    });
    let json = serde_json::to_value(supports).unwrap();
    assert_eq!(serde_json::from_value::<Supports>(json).unwrap(), supports);
    let neither = serde_json::to_value(Capabilities::default()).unwrap();
    assert!(serde_json::from_value::<Supports>(neither).is_err());
}
