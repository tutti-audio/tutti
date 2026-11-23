use super::*;
use crate::test_support::{RATE, prepared, render_f64};
use tutti_dsp::{Algorithm, Operator, OperatorCount, OperatorFrequency, OperatorSpec, Route};
use tutti_graph::Node;
use tutti_graph::contract::{BlockRig, assert_param_fork};
use tutti_units::Tail;
use tutti_units::{FrequencyRatio, UnitAddr, UnitParam};

/// Whole periods of every multiple of 10 Hz at 48 kHz.
const WINDOW: usize = 4800;
/// The half-bands' ripple at 2x: `2 * 10^(-100/20)` for the one stage.
const RIPPLE: f64 = 2e-5;

fn ratio(x: f64) -> OperatorFrequency {
    OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap())
}

/// Operator 0 at `fm`, modulating operator 1 at `fc`, a carrier.
fn two_operators() -> FmPatch {
    FmPatch::new(Algorithm::chain(OperatorCount::of(2))).operators(&[
        OperatorSpec::new(ratio(1.0)),
        OperatorSpec::new(ratio(10.0)),
    ])
}

/// One operator that modulates itself.
fn feedback_operator() -> FmPatch {
    let route = Route {
        modulator: Operator::of(0),
        target: Operator::of(0),
    };
    let algorithm =
        Algorithm::try_from((OperatorCount::of(1), &[route][..], &[Operator::of(0)][..])).unwrap();
    FmPatch::new(algorithm).operators(&[OperatorSpec::new(ratio(1.0))])
}

/// `J_n(x)` from its series, for `n >= 0`.
fn bessel_j(n: u32, x: f64) -> f64 {
    let mut term = (x / 2.0).powi(n as i32) / (1..=n).map(f64::from).product::<f64>();
    let mut sum = term;
    for m in 1..60 {
        term *= -(x / 2.0).powi(2) / (f64::from(m) * f64::from(m + n));
        sum += term;
    }
    sum
}

/// The amplitude of the line at `hz` in `window` (whole periods of it), `x[n] = J sin(w (n -
/// delay))` reading `(-J sin(w delay), -J cos(w delay))`.
fn line(window: &[f64], hz: f64, delay: f64) -> f64 {
    let w = core::f64::consts::TAU * hz / RATE.get();
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in window.iter().enumerate() {
        re += v * libm::cos(w * n as f64);
        im -= v * libm::sin(w * n as f64);
    }
    let (re, im) = (
        re * 2.0 / window.len() as f64,
        im * 2.0 / window.len() as f64,
    );
    -im * libm::cos(w * delay) - re * libm::sin(w * delay)
}

fn delay() -> f64 {
    Oversampling::new(OversamplingFactor::X2)
        .plan(RATE)
        .unwrap()
        .down_delay()
        .get()
}

#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(FmNode::<f32>::from(FmConfig::new(two_operators())));
}

/// A mono source declaring the frequency, the level, then the index and the feedback as the
/// two `Index` params, starting as configured, with no tail.
#[test]
fn the_shape_is_a_mono_source_declaring_frequency_level_index_and_feedback() {
    let n = prepared(FmNode::<f32>::from(
        FmConfig::new(feedback_operator())
            .frequency(Hz::new(220.0))
            .level(Amplitude::new(0.5))
            .index(ModulationIndex::of(2.0))
            .feedback(ModulationIndex::of(0.75)),
    ));
    let shape = n.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (0, 1)
    );
    assert_eq!(shape.tail, Tail::None);
    assert_eq!(
        shape.params.as_slice(),
        &[
            UnitAddr::from(UnitParam::Frequency),
            UnitAddr::from(UnitParam::Volume),
            UnitAddr::new(UnitParam::Index, 0),
            UnitAddr::new(UnitParam::Index, 1),
        ][..]
    );
    let bases: Vec<_> = (0..4).map(|k| n.param_base(k)).collect();
    assert_eq!(bases, [Some(220.0), Some(0.5), Some(2.0), Some(0.75)]);
}

/// The node plays the voice: at index `I` the carrier line is J_0(I) and its first
/// sidebands +-J_1(I) (J_{-1} = -J_1), at the output level.
#[test]
fn the_node_plays_the_bessel_lines_of_its_index() {
    let (index, level) = (1.8, 0.5);
    let node = FmNode::<f64>::from(
        FmConfig::new(two_operators())
            .frequency(Hz::new(1_000.0))
            .level(Amplitude::new(level))
            .index(ModulationIndex::of(index)),
    );
    let out = render_f64(node, RATE, &[], 2 * WINDOW);
    let window = &out[0][WINDOW..];
    let d = delay();
    for (hz, want) in [
        (10_000.0, bessel_j(0, index)),
        (11_000.0, bessel_j(1, index)),
        (9_000.0, -bessel_j(1, index)),
    ] {
        let got = line(window, hz, d);
        assert!(
            (got - level * want).abs() <= level * RIPPLE,
            "{hz} Hz: {got}"
        );
    }
}

/// The first `Index` param is the index of modulation and the second the feedback: set
/// through the cells, a driven single operator has the second harmonic J_1(beta) that
/// `sin(theta + beta sin(theta))` has, to the first order in the two-sample average's lag of
/// `1.5` samples (`1.5 w = 0.01 rad` at 100 Hz and 96 kHz, a few parts in a thousand of
/// `beta`).
#[test]
fn the_second_index_is_the_feedback() {
    let beta = 0.5;
    let (mut rig, controls) = BlockRig::<f64>::with_format(
        FmNode::<f64>::from(FmConfig::new(feedback_operator()).frequency(Hz::new(100.0))),
        RATE,
        2 * WINDOW,
    );
    assert!(controls.set(UnitAddr::new(UnitParam::Index, 1), beta));
    rig.block();
    let out = rig.output();
    let window = &out.plane(0).samples()[WINDOW..];
    let second = line(window, 200.0, delay()).abs();
    assert!(
        (second - bessel_j(1, beta)).abs() <= 0.02,
        "second harmonic {second} against {}",
        bessel_j(1, beta)
    );
    // The index of modulation moves nothing on a patch whose operator has no modulator.
    let before = window.to_vec();
    assert!(controls.set(UnitParam::Index, 3.0));
    rig.block();
    let after = rig.output().plane(0).samples()[WINDOW..].to_vec();
    assert!(
        before
            .iter()
            .zip(&after)
            .all(|(a, b)| (a - b).abs() < 1e-12)
    );
}
