//! A saved state is the value an app stores: the device it came from, the state's
//! id, and what the device gave (M2-111 §3.1.1).
use tutti_midi_device::{MediaType, PeJson, PeState, SavedState, StateId, StateRev, UnixSeconds};
use tutti_ump::{DeviceIdentity, ManufacturerId, SoftwareRevision, U7};

fn device() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::of(0x0042),
        family: [1, 0].map(U7::of),
        model: [7, 0].map(U7::of),
        revision: SoftwareRevision::from([1, 2, 3, 4].map(U7::of)),
    }
}

fn saved(state: PeState) -> SavedState {
    SavedState::new(device(), StateId::new("fullState").unwrap(), state)
}

fn round_trip(saved: &SavedState) -> SavedState {
    serde_json::from_str(&serde_json::to_string(saved).unwrap()).unwrap()
}

#[test]
fn a_binary_state_reads_back_with_its_device_revision_and_time() {
    let state = PeState::octets(vec![0, 127, 128, 255])
        .revised(StateRev::new("buui890adj"))
        .stamped(UnixSeconds::of(1_586_786_400));
    let saved = saved(state);
    assert_eq!(round_trip(&saved), saved);
}

#[test]
fn a_json_state_reads_back_as_json() {
    let json = PeJson::try_from(br#"{"a":[1,2]}"#.as_slice()).unwrap();
    let saved = saved(PeState::json(json));
    let back = round_trip(&saved);
    assert_eq!(back, saved);
    assert_eq!(back.state().to_wire().0, None);
}

#[test]
fn the_document_names_each_part() {
    let state = PeState::binary(MediaType::new("audio/wav").unwrap(), vec![9])
        .unwrap()
        .stamped(UnixSeconds::of(5));
    assert_eq!(
        serde_json::to_string(&saved(state)).unwrap(),
        r#"{"device":{"manufacturer":66,"family":[1,0],"model":[7,0],"revision":[1,2,3,4]},"state_id":"fullState","rev":null,"timestamp":5,"media_type":"audio/wav","bytes":[9]}"#
    );
}

#[test]
fn a_document_that_breaks_a_rule_of_its_parts_is_refused() {
    let good = serde_json::to_string(&saved(PeState::octets(vec![1]))).unwrap();
    for (from, to, says) in [
        (
            r#""state_id":"fullState""#,
            r#""state_id":"no-way""#,
            "not a state id",
        ),
        (
            r#""media_type":"application/octet-stream""#,
            r#""media_type":"""#,
            "not a media type",
        ),
        (r#""timestamp":null"#, r#""timestamp":-1"#, "invalid value"),
    ] {
        assert!(good.contains(from), "{good}");
        let bad = good.replace(from, to);
        let error = serde_json::from_str::<SavedState>(&bad).unwrap_err();
        assert!(error.to_string().contains(says), "{bad}: {error}");
    }
}

/// The state's bytes that do not read back say why: the cause of the refusal is
/// in the document error's text, since a serde error keeps only its text.
#[test]
fn bytes_that_are_not_the_state_name_their_cause() {
    let good = serde_json::to_string(&saved(PeState::octets(vec![1]))).unwrap();
    let bad = good
        .replace(
            r#""media_type":"application/octet-stream""#,
            r#""media_type":null"#,
        )
        .replace(r#""bytes":[1]"#, r#""bytes":[123]"#);
    assert_ne!(bad, good);
    let error = serde_json::from_str::<SavedState>(&bad).unwrap_err();
    assert!(error.to_string().contains("do not read back"), "{error}");
    assert!(
        error
            .to_string()
            .contains("not JSON of the Property Exchange form"),
        "{error}"
    );
}
