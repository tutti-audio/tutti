use tutti_units::{Amplitude, Gain, SampleSpan};

use super::*;
use crate::reverb::network::{Band, Fdn, Matrix, Network, NetworkStage};
use crate::{Lti as _, System as _};

fn shimmering() -> FdnLayout {
    FdnLayout::shimmering(8, MaxDecay::DEFAULT).unwrap()
}

fn base() -> FdnControls {
    FdnControls::DEFAULT
}

fn with(f: impl FnOnce(&mut FdnControls)) -> FdnControls {
    let mut c = base();
    f(&mut c);
    c
}

fn design_at(d: &FdnLayout, c: &FdnControls, rate: SampleRate) -> Box<Network> {
    let mut network = Box::new(Network::new(rate));
    d.design_into(c, rate, &mut network);
    network
}

fn design_of(d: &FdnLayout, c: &FdnControls) -> Box<Network> {
    design_at(d, c, SampleRate::SR_48K)
}

fn fdn_of(n: &Network) -> &Fdn {
    let Some(NetworkStage::Fdn(f)) = n.stages.first() else {
        panic!("one FDN stage")
    };
    f
}

/// Every control, however hostile, designs a stable network: out of range clamps and not
/// finite is the default, so the loop never gains.
#[test]
fn every_control_designs_a_stable_network() {
    let nasty = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -3.0, 40.0, 0.0];
    let d = FdnLayout::default();
    for &x in &nasty {
        let c = FdnControls {
            decay: [Seconds::new(x); 3],
            size: Mix::new(x),
            diffusion: Mix::new(x),
            pre_delay: Seconds::new(x),
            modulation: Bipolar::clamped(x),
            rate: Hz::new(x),
            width: StereoWidth::new(x),
            wet: Db::new(x),
            freeze: x > 0.5,
            ..FdnControls::DEFAULT
        };
        let n = design_of(&d, &c);
        assert!(n.is_stable(), "{x}");
        assert!(n.pre_delay <= pre_delay_frames(MAX_PRE_DELAY, SampleRate::SR_48K));
    }
}

/// A shimmering layout is the plain one with a shifter in the loop: even lines from 2 to 16 as
/// the plain layout's, with no freeze, and every control, a send and a shift past their ranges
/// or not finite included, designs a stable network.
#[test]
fn a_shimmering_layout_designs_a_stable_network_that_never_freezes() {
    assert!(FdnLayout::shimmering(3, MaxDecay::DEFAULT).is_none());
    assert!(FdnLayout::shimmering(18, MaxDecay::DEFAULT).is_none());
    let d = FdnLayout::shimmering(8, MaxDecay::DEFAULT).unwrap();
    assert!(d.shimmers() && !FdnLayout::default().shimmers());
    for x in [f64::NAN, f64::INFINITY, -3.0, 40.0, 0.0] {
        let c = FdnControls {
            shimmer_pitch: Cents::new(x),
            shimmer_send: Feedback::new(x),
            freeze: true,
            ..FdnControls::DEFAULT
        };
        let n = design_of(&d, &c);
        assert!(n.is_stable() && !n.is_lossless(), "{x}");
    }
}

/// Diffusion is the reflection vector: none couples no line (the network says so), full is the
/// Householder matrix, and between is a unit vector that leans from one axis toward the flat
/// vector, so the loop is orthogonal at every setting.
#[test]
fn diffusion_is_the_reflection_vector() {
    let d = FdnLayout::default();
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix::new(0.0)))).matrix,
        Matrix::Diagonal
    );
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix::new(1.0)))).matrix,
        Matrix::Householder
    );
    let mut last_axis = f64::INFINITY;
    for diffusion in [0.1, 0.25, 0.5, 0.75, 0.9] {
        let n = design_of(&d, &with(|c| c.diffusion = Mix::new(diffusion)));
        let Matrix::Reflection(v) = &fdn_of(&n).matrix else {
            panic!("a reflection at {diffusion}")
        };
        assert_eq!(v.len(), 8);
        let unit: f64 = v.iter().map(|&x| x.get().powi(2)).sum();
        assert!((unit - 1.0).abs() < 1e-14, "{diffusion}: |v|^2 = {unit}");
        assert!(
            v[0].get() < last_axis,
            "the first line's share falls as the mix grows"
        );
        last_axis = v[0].get();
    }
}

/// A freeze holds every gain at exactly one, shuts the input, stops the modulation and makes
/// the network lossless with an unbounded tail; a shimmering layout, whose tail always decays,
/// ignores it.
#[test]
fn a_freeze_is_lossless_and_takes_no_input() {
    let (d, shimmer) = (FdnLayout::default(), shimmering());
    let frozen = with(|c| c.freeze = true);
    let n = design_of(&d, &frozen);
    let f = fdn_of(&n);
    assert!(n.is_lossless() && n.is_stable());
    assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Unbounded);
    assert!(
        f.input.iter().all(|&g| g == Gain::SILENT),
        "the input is shut"
    );
    assert!(
        f.lines
            .iter()
            .all(|l| l.modulation.depth == SampleSpan::ZERO)
    );
    for l in f.lines.iter() {
        for band in Band::ALL {
            assert_eq!(l.absorption.gain(band), Amplitude::UNITY);
        }
    }
    assert_eq!(design_of(&shimmer, &frozen), design_of(&shimmer, &base()));
    assert!(!design_of(&shimmer, &frozen).is_lossless());
}

/// Lengths are whole frames, grow with the size and scale with the rate, so a settled network
/// delays by whole frames and a frozen one reads no fraction.
#[test]
fn lengths_are_whole_frames_that_grow_with_size_and_rate() {
    let d = FdnLayout::default();
    let mut last = vec![0.0f64; 8];
    for size in [0.0, 0.3, 0.6, 1.0] {
        let n = design_of(&d, &with(|c| c.size = Mix::new(size)));
        let lengths: Vec<f64> = fdn_of(&n).lines.iter().map(|l| l.length.get()).collect();
        assert!(lengths.iter().all(|l| l.fract() == 0.0), "{lengths:?}");
        assert!(lengths.iter().zip(&last).all(|(a, b)| a > b));
        last = lengths;
    }
    let at = |hz| design_at(&d, &base(), SampleRate::of(hz));
    let (a, b) = (at(44_100.0), at(88_200.0));
    for (x, y) in fdn_of(&a).lines.iter().zip(fdn_of(&b).lines.iter()) {
        assert!((y.length.get() - 2.0 * x.length.get()).abs() <= 1.0);
    }
}

/// Even and odd lines go to opposite ears: the left output row feeds the even lines and the
/// right the odd, each with `sqrt(2 / n)` so an ear's gains have unit norm, and one mono input
/// of `1 / sqrt(n)` feeds every line.
#[test]
fn the_input_feeds_every_line_and_the_ears_take_alternate_lines() {
    for n in [2usize, 8, 16] {
        let d = FdnLayout::new(n, MaxDecay::of(Seconds::new(2.0))).unwrap();
        let net = design_of(&d, &base());
        let f = fdn_of(&net);
        let input = 1.0 / (n as f64).sqrt();
        let ear = (2.0 / n as f64).sqrt();
        assert_eq!(f.lines.len(), n);
        assert!(
            f.input.iter().all(|&g| (g.get() - input).abs() < 1e-15),
            "{n}"
        );
        for j in 0..n {
            let (left, right) = (f.output.rows[0][j], f.output.rows[1][j]);
            let (hot, cold) = if j % 2 == 0 {
                (left, right)
            } else {
                (right, left)
            };
            assert!(
                (hot.get() - ear).abs() < 1e-15 && cold == Gain::SILENT,
                "line {j} of {n}"
            );
        }
    }
}

/// The configuration refuses a line count the network cannot hold: it is 2 to 16 and even.
#[test]
fn a_configuration_refuses_a_line_count_it_cannot_hold() {
    let ok = MaxDecay::DEFAULT;
    assert!(FdnLayout::new(8, ok).is_some());
    assert!(FdnLayout::new(16, ok).is_some());
    for lines in [0, 1, 7, 17, 18] {
        assert!(FdnLayout::new(lines, ok).is_none(), "{lines}");
    }
}

/// The capacity holds a design's tail at 192 kHz too, where the rounding of the reflection
/// vector (applied every frame by `rt60_bound`) is a larger share of a frame's decay: the
/// margin is computed from the rate and the longest decay, not a flat figure.
#[test]
fn the_capacity_dominates_where_rounding_matters() {
    let d = FdnLayout::default();
    let rate = SampleRate::SR_192K;
    let bound = d.capacity(rate, Samples::ZERO);
    for k in 1..100 {
        let c = FdnControls {
            decay: [d.max_decay; 3],
            size: Mix::new(1.0),
            diffusion: Mix::new(f64::from(k) / 100.0),
            pre_delay: MAX_PRE_DELAY,
            modulation: Bipolar::of(1.0),
            ..FdnControls::DEFAULT
        };
        let tail = design_at(&d, &c, rate).tail(TAIL_FLOOR);
        let covered = match (bound.tail, tail) {
            (DecayTime::Unbounded, _) => true,
            (DecayTime::Finite(_), DecayTime::Unbounded) => false,
            (DecayTime::Finite(a), DecayTime::Finite(b)) => a.get() >= b.get(),
        };
        assert!(covered, "diffusion {k}%: {tail:?} over {:?}", bound.tail);
    }
}

/// The wet gain is held at or above silence and at most `MAX_WET`: -120 dB or less is minus
/// infinity, a gain between passes through, and anything above 24 dB is 24 dB.
#[test]
fn the_wet_gain_is_silent_at_minus_120_and_held_at_24() {
    let d = FdnLayout::default();
    let wet_of = |db: f64| design_of(&d, &with(|c| c.wet = Db::new(db))).wet;
    assert_eq!(wet_of(-120.0), Db::new(f64::NEG_INFINITY));
    assert_eq!(wet_of(-200.0), Db::new(f64::NEG_INFINITY));
    assert_eq!(wet_of(-119.0), Db::new(-119.0));
    assert_eq!(wet_of(6.0), Db::new(6.0));
    assert_eq!(wet_of(30.0), Db::new(24.0));
}
