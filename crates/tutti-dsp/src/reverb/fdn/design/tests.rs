use super::*;
use crate::reverb::network::{Band, Fdn, Matrix, Network, Stage};
use crate::{Lti as _, System as _};

fn freezable() -> FdnLayout {
    FdnLayout::new(8, MaxDecay::DEFAULT.get(), true).unwrap()
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
    let Some(Stage::Fdn(f)) = n.stages.first() else {
        panic!("one FDN stage")
    };
    f
}

/// Every control, however hostile, designs a stable network: out of range clamps and not
/// finite is the default, so the loop never gains.
#[test]
fn every_control_designs_a_stable_network() {
    let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 40.0, 0.0];
    for d in [FdnLayout::default(), freezable()] {
        for &v in &nasty {
            let c = FdnControls {
                decay: [Seconds::from_f32(v); 3],
                size: Mix(v),
                diffusion: Mix(v),
                pre_delay: Seconds::from_f32(v),
                modulation: Depth::clamped(v),
                rate: Hz(v),
                width: StereoWidth(v),
                wet: Db(v),
                freeze: v > 0.5,
            };
            let n = design_of(&d, &c);
            assert!(n.is_stable(), "{v}");
            assert!(n.pre_delay <= pre_delay_frames(MAX_PRE_DELAY, SampleRate::SR_48K));
        }
    }
}

/// Diffusion is the reflection vector: none couples no line (the network says so), full is the
/// Householder matrix, and between is a unit vector that leans from one axis toward the flat
/// vector, so the loop is orthogonal at every setting.
#[test]
fn diffusion_is_the_reflection_vector() {
    let d = freezable();
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(0.0)))).matrix,
        Matrix::Diagonal
    );
    assert_eq!(
        fdn_of(&design_of(&d, &with(|c| c.diffusion = Mix(1.0)))).matrix,
        Matrix::Householder
    );
    let mut last_axis = f32::INFINITY;
    for diffusion in [0.1, 0.25, 0.5, 0.75, 0.9] {
        let n = design_of(&d, &with(|c| c.diffusion = Mix(diffusion)));
        let Matrix::Reflection(v) = &fdn_of(&n).matrix else {
            panic!("a reflection at {diffusion}")
        };
        assert_eq!(v.len(), 8);
        let unit: f64 = v.iter().map(|&x| f64::from(x).powi(2)).sum();
        assert!((unit - 1.0).abs() < 1e-6, "{diffusion}: |v|^2 = {unit}");
        assert!(
            v[0] < last_axis,
            "the first line's share falls as the mix grows"
        );
        last_axis = v[0];
    }
}

/// A freeze holds every gain at exactly one, shuts the input, stops the modulation and makes
/// the network lossless with an unbounded tail; a layout that cannot freeze ignores it.
#[test]
fn a_freeze_is_lossless_and_takes_no_input() {
    let (d, plain) = (freezable(), FdnLayout::default());
    let frozen = with(|c| c.freeze = true);
    let n = design_of(&d, &frozen);
    let f = fdn_of(&n);
    assert!(n.is_lossless() && n.is_stable());
    assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
    assert!(f.input.iter().all(|&g| g == 0.0), "the input is shut");
    assert!(f.lines.iter().all(|l| l.modulation.depth == 0.0));
    for l in f.lines.iter() {
        for band in Band::ALL {
            assert_eq!(l.absorption.gain(band).get().to_bits(), 1.0f32.to_bits());
        }
    }
    assert_eq!(design_of(&plain, &frozen), design_of(&plain, &base()));
    assert!(!design_of(&plain, &frozen).is_lossless());
}

/// Lengths are whole frames, grow with the size and scale with the rate, so a settled network
/// delays by whole frames and a frozen one reads no fraction.
#[test]
fn lengths_are_whole_frames_that_grow_with_size_and_rate() {
    let d = FdnLayout::default();
    let mut last = vec![0.0f32; 8];
    for size in [0.0, 0.3, 0.6, 1.0] {
        let n = design_of(&d, &with(|c| c.size = Mix(size)));
        let lengths: Vec<f32> = fdn_of(&n).lines.iter().map(|l| l.length).collect();
        assert!(lengths.iter().all(|l| l.fract() == 0.0), "{lengths:?}");
        assert!(lengths.iter().zip(&last).all(|(a, b)| a > b));
        last = lengths;
    }
    let at = |hz| design_at(&d, &base(), SampleRate::of(hz));
    let (a, b) = (at(44_100.0), at(88_200.0));
    for (x, y) in fdn_of(&a).lines.iter().zip(fdn_of(&b).lines.iter()) {
        assert!((y.length - 2.0 * x.length).abs() <= 1.0);
    }
}

/// Even and odd lines go to opposite ears: the left output row feeds the even lines and the
/// right the odd, each with `sqrt(2 / n)` so an ear's gains have unit norm, and one mono input
/// of `1 / sqrt(n)` feeds every line.
#[test]
fn the_input_feeds_every_line_and_the_ears_take_alternate_lines() {
    for n in [2usize, 8, 16] {
        let d = FdnLayout::new(n, Seconds(2.0), false).unwrap();
        let net = design_of(&d, &base());
        let f = fdn_of(&net);
        let input = 1.0 / (n as f32).sqrt();
        let ear = (2.0 / n as f32).sqrt();
        assert_eq!(f.lines.len(), n);
        assert!(f.input.iter().all(|&g| (g - input).abs() < 1e-6), "{n}");
        for j in 0..n {
            let (left, right) = (f.output.rows[0][j], f.output.rows[1][j]);
            let (hot, cold) = if j % 2 == 0 {
                (left, right)
            } else {
                (right, left)
            };
            assert!((hot - ear).abs() < 1e-6 && cold == 0.0, "line {j} of {n}");
        }
    }
}

/// The configuration refuses what the network cannot hold.
#[test]
fn a_configuration_refuses_what_it_cannot_hold() {
    let ok = Seconds(8.0);
    assert!(FdnLayout::new(8, ok, false).is_some());
    assert!(FdnLayout::new(16, ok, true).is_some());
    for lines in [0, 1, 7, 17, 18] {
        assert!(FdnLayout::new(lines, ok, false).is_none(), "{lines}");
    }
    for bad in [0.0, 0.01, 60.001, f32::NAN, f32::INFINITY] {
        assert!(
            FdnLayout::new(8, Seconds::from_f32(bad), false).is_none(),
            "{bad}"
        );
    }
}

/// The capacity holds a design's tail at 192 kHz too, where the rounding of the reflection
/// vector (applied every frame by `rt60_bound`) is a larger share of a frame's decay: the
/// margin is computed from the rate and the longest decay, not a flat figure.
#[test]
fn the_capacity_dominates_where_rounding_matters() {
    let d = FdnLayout::default();
    let rate = SampleRate::SR_192K;
    let bound = d.capacity(rate);
    for k in 1..100 {
        let c = FdnControls {
            decay: [d.max_decay; 3],
            size: Mix(1.0),
            diffusion: Mix(k as f32 / 100.0),
            pre_delay: MAX_PRE_DELAY,
            modulation: Depth::of(1.0),
            ..FdnControls::DEFAULT
        };
        let tail = design_at(&d, &c, rate).tail(TAIL_FLOOR);
        assert!(
            bound.tail.covers(tail),
            "diffusion {k}%: {tail:?} over {:?}",
            bound.tail
        );
    }
}
