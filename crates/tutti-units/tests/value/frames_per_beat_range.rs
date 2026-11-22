//! `FramesPerBeat` keeps the range-checked contract: `new` refuses, `TryFrom`
//! agrees with it.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_units::{FramesPerBeat, FramesPerBeatOutOfRange};

/// A beat spans a finite count of frames above zero, so a beat span times it
/// is always a finite count of frames.
#[test]
fn a_count_of_frames_per_beat_is_finite_and_above_zero() {
    let c = RangeContract {
        new: FramesPerBeat::new,
        get: FramesPerBeat::get,
        of: Some(FramesPerBeat::of),
        valid: &[f64::MIN_POSITIVE, 1.0, 24_000.0, f64::MAX],
        invalid: &[
            0.0,
            -0.0,
            -24_000.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NAN,
        ],
    };
    check_range_contract(&c);
    assert_eq!(
        FramesPerBeat::try_from(0.0),
        Err(FramesPerBeatOutOfRange(0.0))
    );
}
