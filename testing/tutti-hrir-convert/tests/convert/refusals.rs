//! What a file holds that a sphere cannot represent is refused by name.

use tutti_dsp::HrtfBinauralError;
use tutti_hrir_convert::{SofaError, convert};

use crate::support::{Sofa, TAPS};

fn refused(name: &str, sofa: &Sofa) -> SofaError {
    convert(&sofa.write(name)).expect_err("the file is refused")
}

#[test]
fn a_file_without_a_convention_is_not_sofa() {
    let mut sofa = Sofa::octahedron();
    sofa.convention = None;
    assert!(matches!(
        refused("no_convention", &sofa),
        SofaError::NotSofa
    ));
}

#[test]
fn another_convention_is_refused_by_name() {
    let mut sofa = Sofa::octahedron();
    sofa.convention = Some("GeneralFIR");
    let err = refused("other_convention", &sofa);
    assert!(matches!(&err, SofaError::Convention { found } if found == "GeneralFIR"));
    assert!(err.to_string().contains("GeneralFIR"));
}

#[test]
fn a_receiver_count_other_than_two_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.ir_shape = vec![6, 1, 2 * TAPS as u64];
    assert!(matches!(
        refused("one_receiver", &sofa),
        SofaError::Receivers { found: 1 }
    ));
}

#[test]
fn a_fractional_or_per_measurement_rate_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.rate = vec![44_100.5];
    assert!(matches!(
        refused("rate_fraction", &sofa),
        SofaError::SamplingRate { .. }
    ));
    sofa.rate = vec![48_000.0; 6];
    assert!(matches!(
        refused("rate_each", &sofa),
        SofaError::SamplingRate { .. }
    ));
}

#[test]
fn a_fractional_or_negative_delay_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.delay = Some((vec![1, 2], vec![0.0, 2.5]));
    assert!(matches!(
        refused("delay_fraction", &sofa),
        SofaError::Delay { receiver: 1, .. }
    ));
    sofa.delay = Some((vec![1, 2], vec![-1.0, 0.0]));
    assert!(matches!(
        refused("delay_negative", &sofa),
        SofaError::Delay { receiver: 0, .. }
    ));
}

#[test]
fn a_missing_delay_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.delay = None;
    assert!(matches!(
        refused("delay_missing", &sofa),
        SofaError::Missing { name: "Data.Delay" }
    ));
}

#[test]
fn one_direction_is_no_sphere() {
    let mut sofa = Sofa::octahedron();
    sofa.position_shape = vec![1, 3];
    sofa.positions = vec![0.0, 0.0, 1.0];
    assert!(matches!(
        refused("one_direction", &sofa),
        SofaError::SingleDirection
    ));
}

#[test]
fn an_unknown_coordinate_type_or_unit_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.position_type = Some("polar");
    assert!(matches!(
        refused("type_other", &sofa),
        SofaError::PositionType { .. }
    ));
    sofa.position_type = None;
    assert!(matches!(
        refused("type_none", &sofa),
        SofaError::PositionType { found: None }
    ));
    sofa.position_type = Some("spherical");
    sofa.position_units = Some("radian, radian, metre");
    assert!(matches!(
        refused("units_radian", &sofa),
        SofaError::PositionUnits { .. }
    ));
}

#[test]
fn an_elevation_past_a_pole_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.positions[1] = 95.0;
    assert!(matches!(
        refused("past_the_pole", &sofa),
        SofaError::Position { measurement: 0, .. }
    ));
}

/// 2401 frames at 48 kHz is one past 50 ms.
#[test]
fn a_response_past_fifty_milliseconds_is_refused_not_truncated() {
    let mut sofa = Sofa::octahedron();
    let taps = 48_000 / 20 + 1;
    sofa.ir_shape = vec![6, 2, taps as u64];
    sofa.ir = vec![0.0; 6 * 2 * taps];
    assert!(matches!(
        refused("too_long", &sofa),
        SofaError::Sphere(HrtfBinauralError::ResponseTooLong {
            len: 2401,
            max: 2400
        })
    ));
}

#[test]
fn a_delay_that_lengthens_a_response_past_fifty_milliseconds_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.delay = Some((vec![1, 2], vec![0.0, 2400.0]));
    assert!(matches!(
        refused("delay_too_long", &sofa),
        SofaError::Sphere(HrtfBinauralError::ResponseTooLong { .. })
    ));
}

#[test]
fn coplanar_directions_are_refused_by_the_sphere() {
    let mut sofa = Sofa::octahedron();
    sofa.position_shape = vec![4, 3];
    sofa.positions.truncate(12);
    sofa.ir_shape = vec![4, 2, TAPS as u64];
    sofa.ir.truncate(4 * 2 * TAPS);
    sofa.delay = Some((vec![1, 2], vec![0.0, 0.0]));
    assert!(matches!(
        refused("coplanar", &sofa),
        SofaError::Sphere(HrtfBinauralError::NotEnclosing)
    ));
}

#[test]
fn a_rate_in_other_units_than_hertz_is_refused() {
    let mut sofa = Sofa::octahedron();
    sofa.rate = vec![48.0];
    sofa.rate_units = Some("kilohertz");
    assert!(matches!(
        refused("rate_kilohertz", &sofa),
        SofaError::RateUnits { found } if found == "kilohertz"
    ));
}
