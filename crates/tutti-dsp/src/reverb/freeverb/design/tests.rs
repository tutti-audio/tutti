use tutti_units::{Feedback, Mix};

use super::*;

fn controls(room: f32, damp: f32) -> FreeverbControls {
    FreeverbControls {
        room: Mix(room),
        damp: Mix(damp),
    }
}

/// The scalings are Jezar's: the comb loop gain is `room * 0.28 + 0.7` and the damping
/// `damp * 0.4`, so the General MIDI room (0.5, 0.5) is a loop gain of 0.84 and a damping of
/// 0.2, and the ends of the range are 0.7 to 0.98 and 0 to 0.4.
#[test]
fn the_scalings_are_freeverbs() {
    let gm = FreeverbControls::GENERAL_MIDI;
    assert_eq!(gm, FreeverbControls::default());
    assert!((gm.feedback().get() - 0.84).abs() < 1e-6);
    assert!((gm.damping() - 0.2).abs() < 1e-6);
    assert!((controls(0.0, 0.0).feedback().get() - 0.7).abs() < 1e-6);
    assert!((controls(1.0, 1.0).feedback().get() - 0.98).abs() < 1e-6);
    assert_eq!(controls(0.0, 0.0).damping(), 0.0);
    assert!((controls(1.0, 1.0).damping() - 0.4).abs() < 1e-6);
}

/// A control outside `0..1` is clamped and one that is not finite is the General MIDI
/// value, so every control gives a loop under the stable bound.
#[test]
fn a_hostile_control_is_made_safe() {
    let gm = FreeverbControls::GENERAL_MIDI;
    assert_eq!(controls(f32::NAN, f32::INFINITY).feedback(), gm.feedback());
    assert_eq!(controls(f32::NAN, f32::NAN).damping(), gm.damping());
    assert_eq!(
        controls(9.0, -2.0).feedback(),
        controls(1.0, 0.0).feedback()
    );
    assert_eq!(controls(9.0, -2.0).damping(), 0.0);
    assert_eq!(
        controls(f32::INFINITY, 0.0).feedback(),
        gm.feedback(),
        "not a number: General MIDI"
    );
    for room in [
        -1e30f32,
        -1.0,
        0.0,
        0.3,
        1.0,
        5.0,
        1e30,
        f32::NAN,
        f32::INFINITY,
    ] {
        let g = controls(room, 0.5).feedback();
        assert!(
            g >= Feedback(0.7) && g <= Feedback(0.98),
            "room {room}: {g:?}"
        );
    }
}
