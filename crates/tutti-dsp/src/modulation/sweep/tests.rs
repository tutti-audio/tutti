use tutti_types::BusTopology;
use tutti_units::{Feedback, PhaseIncrement, Seconds};

use super::*;
use crate::{MaxDelay, ModDelayLayout};

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

fn step(x: f32) -> Stagger {
    Stagger::of_step(PhaseIncrement(x))
}

// ---- the checked constants --------------------------------------------------
//
// `check_range_contract` reads a type through one raw (`new`, `get`): a `Sweep` and a
// `Stagger` are checked on several raws at once, and nothing persists them, so their rules
// are stated here one by one, each with a value on each side of it.

/// A [`Sweep`] refuses what its line could not hold, and never clamps: a base or depth that
/// is not finite or is negative, a least depth above the greatest, a sweep that reaches
/// past the line (`base + depth_max > max_delay`), and feedback outside `0..=MAX_STABLE`.
#[test]
fn a_sweep_refuses_what_its_line_cannot_hold() {
    let line = MaxDelay::of(Seconds(0.05));
    let ok = |base: f64, lo: f64, hi: f64, fb: f32| {
        Sweep::new(Seconds(base), line, Seconds(lo), Seconds(hi), Feedback(fb))
    };
    // The edge of each rule is in; the next value out is refused.
    assert!(
        ok(0.01, 0.0, 0.04, 0.99).is_ok(),
        "base + depth_max == max_delay"
    );
    assert!(
        ok(0.0, 0.0, 0.05, 0.0).is_ok(),
        "no base, the whole line of depth"
    );
    assert!(
        ok(0.01, 0.04, 0.04, 0.5).is_ok(),
        "a depth range of one value"
    );
    assert_eq!(
        ok(0.011, 0.0, 0.04, 0.5),
        Err(SweepError::Reach {
            base: Seconds(0.011),
            depth_max: Seconds(0.04),
            max_delay: Seconds(0.05)
        }),
        "past the line"
    );
    for bad in [f64::NAN, f64::INFINITY, -0.001] {
        assert!(
            matches!(ok(bad, 0.0, 0.01, 0.5), Err(SweepError::Base(_))),
            "{bad}"
        );
    }
    for (lo, hi) in [
        (0.02, 0.01),
        (-0.001, 0.01),
        (0.0, -0.001),
        (f64::NAN, 0.01),
        (0.0, f64::NAN),
        (0.0, f64::INFINITY),
    ] {
        assert!(
            matches!(ok(0.01, lo, hi, 0.5), Err(SweepError::Depth { .. })),
            "{lo}..{hi}"
        );
    }
    for fb in [-0.01f32, 0.991, 5.0, f32::NAN, f32::INFINITY] {
        assert!(
            matches!(ok(0.01, 0.0, 0.01, fb), Err(SweepError::FeedbackMax(_))),
            "{fb}"
        );
    }
    // The presets are inside their lines, and the accessors say what they were built to.
    for s in [Sweep::CHORUS, Sweep::FLANGER, Sweep::GENERAL_MIDI_CHORUS] {
        assert!(s.base().get() + s.depth_max().get() <= s.max_delay().get().get());
    }
    assert_eq!(Sweep::GENERAL_MIDI_CHORUS.feedback_max(), Feedback::NONE);
    assert_eq!(Sweep::CHORUS.depth_max(), Seconds(0.04));
}

/// `of` panics where `new` refuses, the constant's spelling.
#[test]
#[should_panic(expected = "a sweep stays inside its line")]
fn a_sweep_of_a_transposed_base_panics_where_it_is_a_constant() {
    let _ = Sweep::of(
        Seconds(0.05),
        MaxDelay::of(Seconds(0.01)),
        Seconds(0.0),
        Seconds(0.001),
        Feedback::NONE,
    );
}

/// A [`Stagger`] is finite, and wrapped into one cycle where it is made: a step of `1.25`
/// is a quarter cycle, an offset of `-0.25` is three quarters, and channel `c` of a step
/// is `c` times it, wrapped. One offset for each channel is checked against the bus by the
/// layout that takes it.
#[test]
fn a_stagger_is_finite_and_wrapped_into_one_cycle() {
    assert!(Stagger::step(PhaseIncrement(0.25)).is_some());
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(Stagger::step(PhaseIncrement(bad)), None, "{bad}");
        assert!(matches!(
            Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(bad)]),
            Err(StaggerError::NotFinite { channel: 1, .. })
        ));
    }
    let s = Stagger::step(PhaseIncrement(1.25)).unwrap();
    assert_eq!(s.offset(0), PhaseIncrement(0.0));
    assert_eq!(s.offset(1), PhaseIncrement(0.25));
    assert_eq!(s.offset(2), PhaseIncrement(0.5));
    assert_eq!(s.offset(4), PhaseIncrement(0.0), "a full cycle is none");
    let back = Stagger::step(PhaseIncrement(-0.25)).unwrap();
    assert_eq!(
        back.offset(1),
        PhaseIncrement(0.75),
        "behind is wrapped ahead"
    );
    let each = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(-0.25)]).unwrap();
    assert_eq!(each.offset(1), PhaseIncrement(0.75));
    assert_eq!(each.offset(7), PhaseIncrement(0.0), "past the named: none");
    assert_eq!((each.channels(), s.channels()), (Some(2), None));
    let too_many = [PhaseIncrement(0.0); MAX_CHANNELS + 1];
    assert_eq!(
        Stagger::each(&too_many),
        Err(StaggerError::TooMany {
            count: MAX_CHANNELS + 1,
            max: MAX_CHANNELS
        })
    );
    assert!(Stagger::each(&too_many[..MAX_CHANNELS]).is_ok());
}

/// A layout refuses offsets that are not one for each channel of its bus, and takes a step
/// at any width.
#[test]
fn a_layout_refuses_offsets_that_do_not_fit_its_bus() {
    let two = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap();
    assert!(ModDelayLayout::new(BusTopology::STEREO, Sweep::CHORUS, two).is_ok());
    assert_eq!(
        ModDelayLayout::new(bus(3), Sweep::CHORUS, two).unwrap_err(),
        StaggerError::Length { width: 3, got: 2 }
    );
    assert_eq!(
        ModDelayLayout::new(BusTopology::MONO, Sweep::CHORUS, two).unwrap_err(),
        StaggerError::Length { width: 1, got: 2 }
    );
    for width in [1, 2, 6, 16] {
        assert!(ModDelayLayout::new(bus(width), Sweep::CHORUS, step(0.25)).is_ok());
    }
}
