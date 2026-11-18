//! A velocity and a controller value as a position on `0..=1`: `get / MAX`, and
//! `round(n * MAX)` back.

use tutti_ump::{ControlValue, NoteVelocity};
use tutti_units::Normalized;

fn controller_samples() -> impl Iterator<Item = u32> {
    let edges = [
        0,
        1,
        2,
        0x7F,
        0x4000_0000,
        0x7FFF_FFFF,
        0x8000_0000,
        u32::MAX - 1,
        u32::MAX,
    ];
    let stride = (0..=u32::MAX).step_by(65_537);
    edges.into_iter().chain(stride)
}

#[test]
fn a_velocity_is_its_field_over_0xffff() {
    for raw in 0..=u16::MAX {
        assert_eq!(
            NoteVelocity::new(raw).normalized().get(),
            f64::from(raw) / 65535.0
        );
    }
}

#[test]
fn a_controller_value_is_its_word_over_u32_max() {
    for raw in controller_samples() {
        assert_eq!(
            ControlValue::new(raw).normalized().get(),
            f64::from(raw) / f64::from(u32::MAX)
        );
    }
}

#[test]
fn every_velocity_round_trips_exactly() {
    for raw in 0..=u16::MAX {
        let v = NoteVelocity::new(raw);
        assert_eq!(NoteVelocity::from_normalized(v.normalized()), v);
    }
}

#[test]
fn edge_and_sampled_controller_values_round_trip_exactly() {
    for raw in controller_samples() {
        let v = ControlValue::new(raw);
        assert_eq!(ControlValue::from_normalized(v.normalized()), v);
    }
}

#[test]
fn the_bounds_of_the_unit_interval_are_the_bounds_of_the_wire() {
    assert_eq!(
        NoteVelocity::from_normalized(Normalized::of(0.0)),
        NoteVelocity::MIN
    );
    assert_eq!(
        NoteVelocity::from_normalized(Normalized::of(1.0)),
        NoteVelocity::MAX
    );
    assert_eq!(
        ControlValue::from_normalized(Normalized::of(0.0)),
        ControlValue::MIN
    );
    assert_eq!(
        ControlValue::from_normalized(Normalized::of(1.0)),
        ControlValue::MAX
    );
}

/// 0.5 is `32767.5` of the 16-bit range and rounds half up; 0.75 is `49151.25`.
#[test]
fn a_position_rounds_to_the_nearest_wire_value() {
    assert_eq!(
        NoteVelocity::from_normalized(Normalized::of(0.5)).get(),
        32768
    );
    assert_eq!(
        NoteVelocity::from_normalized(Normalized::of(0.75)).get(),
        49151
    );
    assert_eq!(
        ControlValue::from_normalized(Normalized::of(0.5)).get(),
        0x8000_0000
    );
}
