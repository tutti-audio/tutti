use tutti_buffers::{BusTopology, ChannelIndex};
use tutti_units::{Feedback, PhaseIncrement, Seconds};

use super::*;
use crate::{MaxDelay, ModDelayLayout};

fn bus(width: u16) -> BusTopology {
    BusTopology::default_for(width.into()).expect("a test width is one bus")
}

fn step(x: f64) -> Stagger {
    Stagger::of_step(PhaseIncrement::new(x))
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
    let line = MaxDelay::of(Seconds::new(0.05));
    let ok = |base: f64, lo: f64, hi: f64, fb: f64| {
        Sweep::builder(Seconds::new(base), line)
            .depth(Seconds::new(lo), Seconds::new(hi))
            .feedback_max(Feedback::new(fb))
            .build()
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
            base: Seconds::new(0.011),
            depth_max: Seconds::new(0.04),
            max_delay: Seconds::new(0.05)
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
    for fb in [-0.01f64, 0.991, 5.0, f64::NAN, f64::INFINITY] {
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
    assert_eq!(Sweep::CHORUS.depth_max(), Seconds::new(0.04));
}

/// `of` panics where `new` refuses, the constant's spelling.
#[test]
#[should_panic(expected = "a sweep stays inside its line")]
fn a_sweep_of_a_transposed_base_panics_where_it_is_a_constant() {
    let _ = Sweep::of(
        Seconds::new(0.05),
        MaxDelay::of(Seconds::new(0.01)),
        Seconds::ZERO,
        Seconds::new(0.001),
        Feedback::NONE,
    );
}

/// A [`Stagger`] is finite, and wrapped into one cycle where it is made: a step of `1.25`
/// is a quarter cycle, an offset of `-0.25` is three quarters, and channel `c` of a step
/// is `c` times it, wrapped. One offset for each channel is checked against the bus by the
/// layout that takes it.
#[test]
fn a_stagger_is_finite_and_wrapped_into_one_cycle() {
    assert!(Stagger::step(PhaseIncrement::new(0.25)).is_some());
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(Stagger::step(PhaseIncrement::new(bad)), None, "{bad}");
        assert!(matches!(
            Stagger::try_from(&[PhaseIncrement::new(0.0), PhaseIncrement::new(bad)][..]),
            Err(StaggerError::NotFinite { channel: 1, .. })
        ));
    }
    let s = Stagger::step(PhaseIncrement::new(1.25)).unwrap();
    assert_eq!(s.offset(ChannelIndex::of(0)), PhaseIncrement::new(0.0));
    assert_eq!(s.offset(ChannelIndex::of(1)), PhaseIncrement::new(0.25));
    assert_eq!(s.offset(ChannelIndex::of(2)), PhaseIncrement::new(0.5));
    assert_eq!(
        s.offset(ChannelIndex::of(4)),
        PhaseIncrement::new(0.0),
        "a full cycle is none"
    );
    let back = Stagger::step(PhaseIncrement::new(-0.25)).unwrap();
    assert_eq!(
        back.offset(ChannelIndex::of(1)),
        PhaseIncrement::new(0.75),
        "behind is wrapped ahead"
    );
    let each =
        Stagger::try_from(&[PhaseIncrement::new(0.0), PhaseIncrement::new(-0.25)][..]).unwrap();
    assert_eq!(each.offset(ChannelIndex::of(1)), PhaseIncrement::new(0.75));
    assert_eq!(
        each.offset(ChannelIndex::of(7)),
        PhaseIncrement::new(0.0),
        "past the named: none"
    );
    assert_eq!((each.channels(), s.channels()), (Some(2), None));
    let too_many = [PhaseIncrement::new(0.0); MAX_CHANNELS + 1];
    assert_eq!(
        Stagger::try_from(&too_many[..]),
        Err(StaggerError::TooMany {
            count: MAX_CHANNELS + 1,
            max: MAX_CHANNELS
        })
    );
    assert!(Stagger::try_from(&too_many[..MAX_CHANNELS]).is_ok());
}

/// A layout refuses offsets that are not one for each channel of its bus, and takes a step
/// at any width.
#[test]
fn a_layout_refuses_offsets_that_do_not_fit_its_bus() {
    let two = Stagger::try_from(&[PhaseIncrement::new(0.0), PhaseIncrement::new(0.5)][..]).unwrap();
    assert!(
        ModDelayLayout::builder(BusTopology::STEREO, Sweep::CHORUS)
            .stagger(two)
            .build()
            .is_ok()
    );
    assert_eq!(
        ModDelayLayout::builder(bus(3), Sweep::CHORUS)
            .stagger(two)
            .build()
            .unwrap_err(),
        StaggerError::Length { width: 3, got: 2 }
    );
    assert_eq!(
        ModDelayLayout::builder(BusTopology::MONO, Sweep::CHORUS)
            .stagger(two)
            .build()
            .unwrap_err(),
        StaggerError::Length { width: 1, got: 2 }
    );
    for width in [1, 2, 6, 16] {
        assert!(
            ModDelayLayout::builder(bus(width), Sweep::CHORUS)
                .stagger(step(0.25))
                .build()
                .is_ok()
        );
    }
}
