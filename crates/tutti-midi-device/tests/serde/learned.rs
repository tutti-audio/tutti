use tutti_midi_device::{DeviceInfo, Learned, Match, MidiDeviceId, PortDirection, SavedDevice};
use tutti_ump::ProductInstanceId;
use tutti_ump::{DeviceIdentity, ManufacturerId, SoftwareRevision, U7};

fn said() -> Learned {
    Learned::new()
        .identified(DeviceIdentity {
            manufacturer: ManufacturerId::of(0x0042),
            family: [1, 0].map(U7::of),
            model: [7, 0].map(U7::of),
            revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
        })
        .instanced(ProductInstanceId::new("SN-1").unwrap())
}

#[test]
fn what_a_device_said_round_trips() {
    let json = serde_json::to_string(&said()).unwrap();
    assert_eq!(serde_json::from_str::<Learned>(&json).unwrap(), said());
    let empty = serde_json::to_string(&Learned::new()).unwrap();
    assert_eq!(
        serde_json::from_str::<Learned>(&empty).unwrap(),
        Learned::new()
    );
}

/// A value the type cannot hold is refused on the way in, not carried: a
/// Product Instance Id outside printable ASCII or past 42 bytes, a
/// manufacturer id out of range, a byte past 7 bits.
#[test]
fn a_document_with_a_bad_value_is_refused() {
    let good = serde_json::to_value(said()).unwrap();
    let bad = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut value = good.clone();
        edit(&mut value);
        serde_json::from_value::<Learned>(value).is_err()
    };
    assert!(bad(&|v| v["instance_id"] = "caf\u{e9}".into()));
    assert!(bad(&|v| v["instance_id"] = "x".repeat(43).into()));
    assert!(bad(&|v| v["identity"]["family"][0] = 200.into()));
    assert!(bad(&|v| v["identity"]["manufacturer"] = 70_000.into()));
    assert!(!bad(&|_| {}));
}

#[test]
fn a_saved_device_round_trips_and_a_bare_info_reads_as_knowing_nothing() {
    let table_info = serde_json::json!({
        "id": { "identity": { "Alsa": { "client": "K", "port": "P" } }, "ordinal": 0 },
        "name": "K: P",
        "direction": "Input",
    });
    let info: DeviceInfo = serde_json::from_value(table_info.clone()).unwrap();
    assert_eq!(
        info.id,
        serde_json::from_value::<MidiDeviceId>(table_info["id"].clone()).unwrap()
    );
    assert_eq!(info.direction, PortDirection::Input);

    let saved = SavedDevice::new(info, said());
    let json = serde_json::to_string(&saved).unwrap();
    let back: SavedDevice = serde_json::from_str(&json).unwrap();
    assert_eq!(back, saved);
    assert_eq!(back.confirm(&said()), Match::SameUnit);

    let old = serde_json::json!({ "info": table_info });
    let old: SavedDevice = serde_json::from_value(old).unwrap();
    assert!(old.learned().is_empty());
}
