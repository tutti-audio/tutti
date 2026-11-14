//! `I24` with the `serde` feature: a value is its `i32`, and the 24-bit range
//! is checked on the way in.

use tutti_buffers::I24;

#[test]
fn an_i24_serialises_as_its_i32_and_round_trips() {
    for raw in [-8_388_608, -1, 0, 1, 255, 8_388_607] {
        let value = I24::new(raw).unwrap();
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, raw.to_string());
        assert_eq!(serde_json::from_str::<I24>(&json).unwrap(), value);
    }
}

#[test]
fn a_value_outside_the_range_is_refused_on_the_way_in() {
    for raw in [-8_388_609i32, 8_388_608, i32::MAX, i32::MIN] {
        let err = serde_json::from_str::<I24>(&raw.to_string()).unwrap_err();
        assert!(err.to_string().contains(&raw.to_string()), "{err}");
    }
}

#[test]
fn a_slice_of_samples_round_trips_as_a_list() {
    let samples = [I24::MIN, I24::of(-5), I24::ZERO, I24::MAX];
    let json = serde_json::to_string(&samples).unwrap();
    assert_eq!(json, "[-8388608,-5,0,8388607]");
    assert_eq!(serde_json::from_str::<[I24; 4]>(&json).unwrap(), samples);
}
