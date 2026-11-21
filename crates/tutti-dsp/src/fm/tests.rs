use core::f64::consts::TAU;

use tutti_buffers::{MaxBlock, MonoMut};
use tutti_units::{Amplitude, FrequencyRatio, Hz, ModulationIndex, SampleRate, Samples};

use super::*;
use crate::OversamplingFactor;
use crate::testing::lock_in;

const RATE: SampleRate = SampleRate::SR_48K;
/// A window of whole periods of every multiple of 10 Hz at 48 kHz.
const WINDOW: usize = 4800;
const BLOCK: usize = 1024;

fn ratio(x: f64) -> OperatorFrequency {
    OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap())
}

fn op(k: u8) -> Operator {
    Operator::of(k)
}

fn count(n: u8) -> OperatorCount {
    OperatorCount::of(n)
}

fn controls(frequency: f64, index: f64, feedback: f64) -> FmControls {
    FmControls {
        frequency: Hz::new(frequency),
        level: Amplitude::UNITY,
        index: ModulationIndex::new(index).unwrap(),
        feedback: ModulationIndex::new(feedback).unwrap(),
    }
}

/// `frames` frames of `voice` in blocks of at most [`BLOCK`].
fn render(voice: &mut FmVoice<f64>, frames: usize) -> Vec<f64> {
    let mut out = vec![0.0; frames];
    for chunk in out.chunks_mut(BLOCK) {
        voice.process(MonoMut::new(chunk));
    }
    out
}

fn voice(patch: FmPatch, factor: OversamplingFactor, c: &FmControls) -> FmVoice<f64> {
    let mut v = FmVoice::new(
        patch,
        Oversampling::new(factor),
        RATE,
        MaxBlock::of(Samples::new(BLOCK)),
    )
    .unwrap();
    v.retarget(c);
    v
}

/// `J_n(x)` from its series `sum (-1)^m / (m! (m + n)!) (x / 2)^(2m + n)`, for an integer `n`.
fn bessel_j(n: i32, x: f64) -> f64 {
    if n < 0 {
        return if n % 2 == 0 { 1.0 } else { -1.0 } * bessel_j(-n, x);
    }
    let n = n as u32;
    let mut term = (x / 2.0).powi(n as i32) / (1..=n).map(f64::from).product::<f64>();
    let mut sum = term;
    for m in 1..60 {
        term *= -(x / 2.0).powi(2) / (f64::from(m) * f64::from(m + n));
        sum += term;
    }
    sum
}

/// The half-bands' ripple for their default 100 dB, `10^(-100/20)` a stage and twice that by
/// Kaiser's estimate, over the stages of `factor`: the error of a line of at most unit
/// amplitude after the down-sampler.
fn ripple(factor: OversamplingFactor) -> f64 {
    2.0 * 1e-5 * f64::from(factor.get().trailing_zeros())
}

/// The amplitude `J`, signed, of the line at `hz` in a window of whole periods of `x[n] = J sin(w
/// (n - delay))`: the line reads `(re, im) = (-J sin(w delay), -J cos(w delay))`.
fn line(window: &[f64], hz: f64, delay: f64) -> f64 {
    let (re, im) = lock_in(window, hz, RATE.get());
    let wd = TAU * hz * delay / RATE.get();
    -im * wd.cos() - re * wd.sin()
}

/// `sin(2 pi fc t + I sin(2 pi fm t)) = sum_k J_k(I) sin(2 pi (fc + k fm) t)`: a two-operator
/// chain (modulator at `fm`, carrier at `fc`, both unit level) has the line at `fc + k fm` at
/// the amplitude `J_k(I)`, signed. The window is whole periods of every line, so a line is read
/// without leakage, and the output's delay is the down-sampler's linear-phase delay. The bound
/// is the half-bands' ripple (a line's amplitude is at most 1); the lines past `|k| = 8` are
/// under `1e-6`, and a line below 0 Hz folds onto one of them.
#[test]
fn the_sidebands_of_two_operator_pm_are_bessel_functions_of_the_index() {
    let (fm, fc) = (1_000.0, 10_000.0);
    let patch = FmPatch::new(Algorithm::chain(count(2))).operators(&[
        OperatorSpec::new(ratio(1.0)),
        OperatorSpec::new(ratio(10.0)),
    ]);
    for factor in [OversamplingFactor::X2, OversamplingFactor::X4] {
        for index in [0.5, 1.5, 2.4] {
            let mut v = voice(patch, factor, &controls(fm, index, 0.0));
            let delay = v.delay().get();
            let out = render(&mut v, 2 * WINDOW);
            for k in -8..=8 {
                let got = line(&out[WINDOW..], fc + f64::from(k) * fm, delay);
                let want = bessel_j(k, index);
                assert!(
                    (got - want).abs() <= ripple(factor),
                    "factor {factor:?} index {index} line {k}: {got} against {want}"
                );
            }
        }
    }
}

/// At index 0 the carrier is a sine of unit amplitude at its frequency and nothing else:
/// J_0(0) = 1 and J_k(0) = 0.
#[test]
fn at_index_zero_the_carrier_is_a_unit_sine() {
    let patch = FmPatch::new(Algorithm::chain(count(2))).operators(&[
        OperatorSpec::new(ratio(1.0)),
        OperatorSpec::new(ratio(10.0)),
    ]);
    let mut v = voice(patch, OversamplingFactor::X2, &controls(1_000.0, 0.0, 0.0));
    let delay = v.delay().get();
    let out = render(&mut v, 2 * WINDOW);
    let window = &out[WINDOW..];
    for (hz, want) in [
        (10_000.0, 1.0),
        (9_000.0, 0.0),
        (11_000.0, 0.0),
        (1_000.0, 0.0),
    ] {
        assert!(
            (line(window, hz, delay) - want).abs() <= ripple(OversamplingFactor::X2),
            "line {hz}"
        );
    }
}

/// An operator with no feedback is `sin(2 pi n f / fs)`, `n` counted from phase 0.
#[test]
fn without_feedback_an_operator_is_the_sine_of_its_phase() {
    let patch =
        FmPatch::new(Algorithm::parallel(count(1))).operators(&[OperatorSpec::new(ratio(1.0))]);
    let mut stack = Stack::<f64>::new(patch);
    stack.retarget(&controls(1_234.5, 0.0, 0.0), RATE);
    for n in 0..4_000 {
        let want = (TAU * 1_234.5 * f64::from(n) / RATE.get()).sin();
        assert!((stack.step() - want).abs() < 1e-10, "sample {n}");
    }
}

/// The feedback is the average of the last two outputs:
/// `y[n] = sin(2 pi n f / fs + beta (y[n-1] + y[n-2]) / 2)`, from `y[-1] = y[-2] = 0`.
#[test]
fn self_feedback_is_the_average_of_the_last_two_outputs() {
    let route = Route {
        modulator: op(0),
        target: op(0),
    };
    let algorithm = Algorithm::try_from((count(1), &[route][..], &[op(0)][..])).unwrap();
    let patch = FmPatch::new(algorithm).operators(&[OperatorSpec::new(ratio(1.0))]);
    let beta = 1.0;
    let mut stack = Stack::<f64>::new(patch);
    stack.retarget(&controls(1_000.0, 0.0, beta), RATE);
    let (mut y1, mut y2) = (0.0_f64, 0.0_f64);
    for n in 0..200 {
        let want = (TAU * 1_000.0 * f64::from(n) / RATE.get() + beta * (y1 + y2) / 2.0).sin();
        let got = stack.step();
        assert!((got - want).abs() < 1e-9, "sample {n}");
        (y2, y1) = (y1, want);
    }
}

/// An operator's output is a sine, so a voice of one carrier is within `[-1, 1]` for every
/// feedback, however large: the feedback moves the phase and never the amplitude.
#[test]
fn the_fed_back_operator_stays_within_unit_amplitude_for_every_feedback() {
    let route = Route {
        modulator: op(0),
        target: op(0),
    };
    let algorithm = Algorithm::try_from((count(1), &[route][..], &[op(0)][..])).unwrap();
    let patch = FmPatch::new(algorithm).operators(&[OperatorSpec::new(ratio(1.0))]);
    for beta in [0.0, 0.25, 1.0, 2.0, 4.0, 16.0, 1_000.0, 1e9] {
        let mut stack = Stack::<f64>::new(patch);
        stack.retarget(&controls(997.0, 0.0, beta), RATE);
        for n in 0..20_000 {
            let y = stack.step();
            assert!(y.abs() <= 1.0, "beta {beta} sample {n}: {y}");
        }
    }
}

/// With the feedback at 0 the self-routed operator is the same pure sine as without the route,
/// through the oversampling too: unit amplitude at its frequency, nothing at its harmonics.
#[test]
fn zero_feedback_is_a_pure_sine_through_the_oversampling() {
    let route = Route {
        modulator: op(0),
        target: op(0),
    };
    let algorithm = Algorithm::try_from((count(1), &[route][..], &[op(0)][..])).unwrap();
    let patch = FmPatch::new(algorithm).operators(&[OperatorSpec::new(ratio(1.0))]);
    let mut v = voice(patch, OversamplingFactor::X2, &controls(2_000.0, 0.0, 0.0));
    let delay = v.delay().get();
    let out = render(&mut v, 2 * WINDOW);
    let window = &out[WINDOW..];
    for (hz, want) in [(2_000.0, 1.0), (4_000.0, 0.0), (6_000.0, 0.0)] {
        assert!(
            (line(window, hz, delay) - want).abs() <= ripple(OversamplingFactor::X2),
            "line {hz}"
        );
    }
}

/// A fixed-frequency operator plays its own frequency whatever the voice's.
#[test]
fn a_fixed_operator_ignores_the_voice_frequency() {
    let patch = FmPatch::new(Algorithm::parallel(count(1))).operators(&[OperatorSpec::new(
        OperatorFrequency::Fixed(Hz::new(3_000.0)),
    )]);
    let mut v = voice(patch, OversamplingFactor::X2, &controls(440.0, 0.0, 0.0));
    let delay = v.delay().get();
    let out = render(&mut v, 2 * WINDOW);
    let got = line(&out[WINDOW..], 3_000.0, delay);
    assert!((got - 1.0).abs() <= ripple(OversamplingFactor::X2), "{got}");
}

/// Carriers sum at their levels, and a modulator is not heard: three operators in parallel at
/// levels 0.5, 0.25 and 0.125 have lines of exactly those amplitudes, and an algorithm with
/// operator 0 as a modulator only has no line at its frequency.
#[test]
fn carriers_are_heard_at_their_levels_and_modulators_are_not() {
    let specs = [
        OperatorSpec::new(ratio(1.0)).level(Amplitude::new(0.5)),
        OperatorSpec::new(ratio(2.0)).level(Amplitude::new(0.25)),
        OperatorSpec::new(ratio(3.0)).level(Amplitude::new(0.125)),
    ];
    let mut v = voice(
        FmPatch::new(Algorithm::parallel(count(3))).operators(&specs),
        OversamplingFactor::X2,
        &controls(1_000.0, 0.0, 0.0),
    );
    let delay = v.delay().get();
    let out = render(&mut v, 2 * WINDOW);
    for (hz, want) in [(1_000.0, 0.5), (2_000.0, 0.25), (3_000.0, 0.125)] {
        let got = line(&out[WINDOW..], hz, delay);
        assert!(
            (got - want).abs() <= ripple(OversamplingFactor::X2),
            "line {hz}"
        );
    }
    let muted = Algorithm::try_from((count(2), &[][..], &[op(1)][..])).unwrap();
    let mut v = voice(
        FmPatch::new(muted)
            .operators(&[OperatorSpec::new(ratio(1.0)), OperatorSpec::new(ratio(2.0))]),
        OversamplingFactor::X2,
        &controls(1_000.0, 0.0, 0.0),
    );
    let out = render(&mut v, 2 * WINDOW);
    let (re, im) = lock_in(&out[WINDOW..], 1_000.0, RATE.get());
    assert!(re.hypot(im) <= ripple(OversamplingFactor::X2));
}

#[test]
fn an_algorithm_refuses_what_cannot_be_evaluated() {
    let route = |modulator, target| Route {
        modulator: op(modulator),
        target: op(target),
    };
    assert_eq!(
        Algorithm::try_from((count(3), &[route(2, 1)][..], &[op(2)][..])),
        Err(AlgorithmError::Backwards { route: route(2, 1) })
    );
    assert_eq!(
        Algorithm::try_from((count(2), &[route(0, 2)][..], &[op(1)][..])),
        Err(AlgorithmError::NoSuchOperator {
            operator: op(2),
            count: count(2)
        })
    );
    assert_eq!(
        Algorithm::try_from((count(2), &[][..], &[op(3)][..])),
        Err(AlgorithmError::NoSuchOperator {
            operator: op(3),
            count: count(2)
        })
    );
    assert_eq!(
        Algorithm::try_from((count(2), &[route(0, 1)][..], &[][..])),
        Err(AlgorithmError::NoCarrier)
    );
}

/// A chain has operator `k - 1` modulating `k` with the last operator the carrier; a parallel
/// algorithm modulates nothing and hears everything.
#[test]
fn the_chain_and_the_parallel_algorithm_have_their_routes() {
    let chain = Algorithm::chain(count(4));
    for j in 0..4 {
        for k in 0..4 {
            assert_eq!(chain.modulates(op(j), op(k)), k == j + 1, "{j} -> {k}");
        }
        assert_eq!(chain.is_carrier(op(j)), j == 3);
    }
    let parallel = Algorithm::parallel(count(6));
    for j in 0..6 {
        assert!(parallel.is_carrier(op(j)));
        assert!(!parallel.modulates(op(j), op(5)));
    }
}

/// The specs go to operators in order up to the count: one past it is unused, and an operator
/// without one is at the voice's frequency at unity level.
#[test]
fn a_patch_gives_its_specs_to_the_operators_in_order_up_to_the_count() {
    let spec = |r| OperatorSpec::new(ratio(r));
    let unset = OperatorSpec::new(ratio(1.0));
    let two = Algorithm::chain(count(2));
    let long = FmPatch::new(two).operators(&[spec(3.0), spec(5.0), spec(7.0)]);
    assert_eq!(long.operators[..3], [spec(3.0), spec(5.0), unset]);
    let short = FmPatch::new(two).operators(&[spec(3.0)]);
    assert_eq!(short.operators[..2], [spec(3.0), unset]);
}
