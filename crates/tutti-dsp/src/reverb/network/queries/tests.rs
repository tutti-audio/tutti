use tutti_units::{Amplitude, Gain, Hz, SampleRate, SampleSpan};

use super::super::{
    Absorption, AllPass, AllPassForm, Band, Fdn, FirSummary, Gains, Inline, Line, LineModulation,
    Matrix, Network, NetworkCapacity, NetworkOutputMap, NetworkStage, Tap, TapSet, loop_gain,
};
use super::*;
use crate::Lti as _;

const RATE: SampleRate = SampleRate::SR_48K;

/// `v` as a row of signed gains.
fn gain_row<const N: usize>(v: &[f64]) -> Inline<Gain, N> {
    let gains: Vec<Gain> = v.iter().map(|&x| Gain::new(x)).collect();
    Inline::from_slice(&gains).unwrap()
}

fn line(length: f64, absorption: Absorption) -> Line {
    Line {
        length: SampleSpan::of(length),
        modulation: LineModulation::NONE,
        absorption,
        diffuser: None,
    }
}

fn fdn(lines: &[Line], matrix: Matrix) -> Fdn {
    Fdn {
        lines: Inline::from_slice(lines).unwrap(),
        matrix,
        input: Gains::filled(Gain::UNITY, lines.len()).unwrap(),
        output: NetworkOutputMap::default(),
        loop_delay: SampleSpan::ZERO,
    }
}

fn network(stages: &[NetworkStage]) -> Network {
    let mut n = Network::new(RATE);
    for s in stages {
        n.stages.push(s.clone()).unwrap();
    }
    n
}

/// One 1 s line (48 000 frames) set for an RT60 of 2 s, after a 0.1 s
/// pre-delay: the tail to -60 dB is `0.1 + 1 + 2` = 3.1 s, to -30 dB
/// `0.1 + 1 + 1` = 2.1 s, and to -90 dB 4.1 s.
#[test]
fn the_tail_is_pre_delay_transit_and_the_decay_to_the_floor() {
    let g = loop_gain(SampleSpan::of(48_000.0), Seconds::new(2.0), RATE);
    let mut n = network(&[NetworkStage::Fdn(fdn(
        &[line(48_000.0, Absorption::Gain(g))],
        Matrix::Diagonal,
    ))]);
    n.pre_delay = Samples::new(4_800);
    let seconds = |floor| match n.tail(Db::new(floor)) {
        DecayTime::Finite(s) => s.get(),
        DecayTime::Unbounded => panic!("a damped line rings out"),
    };
    assert!((seconds(-60.0) - 3.1).abs() < 1e-5, "{}", seconds(-60.0));
    assert!((seconds(-30.0) - 2.1).abs() < 1e-5);
    assert!((seconds(-90.0) - 4.1).abs() < 1e-5);
    assert!((n.rt60(Band::Mid).get() - 2.0).abs() < 1e-5);
}

/// A series all-pass rings for its delay times the trips its train needs to
/// fall to the floor: at `g = 0.5` to -60 dB that is `ceil(ln 1e-3 /
/// ln 0.5)` = 10 trips after the first, so 11 delays. It adds no RT60.
#[test]
fn a_series_all_pass_rings_for_its_geometric_train() {
    let ap = AllPass {
        delay: SampleSpan::of(100.0),
        gain: Gain::new(0.5),
        modulation: LineModulation::NONE,
        form: AllPassForm::Lattice,
    };
    let n = network(&[NetworkStage::AllPass(
        Inline::from_slice(&[ap, ap]).unwrap(),
    )]);
    let DecayTime::Finite(t) = n.tail(Db::new(-60.0)) else {
        panic!("an all-pass chain rings out")
    };
    let t = t.get();
    assert!((t - 1100.0 / 48_000.0).abs() < 1e-7, "{t}");
    assert_eq!(n.rt60(Band::Low).get(), 0.0);
    assert!(n.is_stable() && !n.is_lossless());
}

/// For a diagonal `A` the slowest line governs; for a mixing `A` the
/// loop is all the lines at once, `-3 Σm / (fs Σ log10 g)`, which for lines
/// each set from RT60 `T` is `T`, and for lines set for different RT60s is
/// between them.
#[test]
fn rt60_is_the_slowest_line_for_a_diagonal_and_the_length_weighted_loop_for_a_mixing() {
    let slow = line(
        40_000.0,
        Absorption::Gain(loop_gain(SampleSpan::of(40_000.0), Seconds::new(3.0), RATE)),
    );
    let fast = line(
        20_000.0,
        Absorption::Gain(loop_gain(SampleSpan::of(20_000.0), Seconds::new(1.0), RATE)),
    );
    let diagonal = network(&[NetworkStage::Fdn(fdn(&[slow, fast], Matrix::Diagonal))]);
    assert!((diagonal.rt60(Band::High).get() - 3.0).abs() < 1e-4);
    // Mixed: -3 (40000 + 20000) / (48000 (log10 g1 + log10 g2)) with
    // log10 g = -3 m / (T fs): = (m1 + m2) / (m1/T1 + m2/T2) = 60000 / (40000/3 + 20000).
    let mixed = network(&[NetworkStage::Fdn(fdn(&[slow, fast], Matrix::Hadamard))]);
    let want = 60_000.0 / (40_000.0 / 3.0 + 20_000.0);
    assert!(
        (mixed.rt60(Band::High).get() - want).abs() < 1e-4,
        "{:?}",
        mixed.rt60(Band::High)
    );
    let equal = [20_000.0, 30_000.0].map(|m| {
        line(
            m,
            Absorption::Gain(loop_gain(SampleSpan::of(m), Seconds::new(2.0), RATE)),
        )
    });
    let same = network(&[NetworkStage::Fdn(fdn(&equal, Matrix::Householder))]);
    assert!((same.rt60(Band::Low).get() - 2.0).abs() < 1e-4);
}

/// The bands of a one-pole at pole 0.5 and gain 0.8: DC `0.8`, a quarter of
/// the rate `0.8 * 0.5 / sqrt(1.25)`, Nyquist `0.8 * 0.5 / 1.5`; explicit
/// bands read straight.
#[test]
fn a_one_pole_has_its_gain_at_dc_a_quarter_rate_and_nyquist() {
    let a = Absorption::OnePole {
        gain: Amplitude::new(0.8),
        damp: 0.5,
    };
    assert!((a.gain(Band::Low).get() - 0.8).abs() < 1e-15);
    assert!((a.gain(Band::Mid).get() - 0.8 * 0.5 / 1.25f64.sqrt()).abs() < 1e-15);
    assert!((a.gain(Band::High).get() - 0.8 * 0.5 / 1.5).abs() < 1e-15);
    let b = Absorption::Bands {
        low: Amplitude::new(0.9),
        mid: Amplitude::new(0.8),
        high: Amplitude::new(0.7),
        crossover: [tutti_units::Hz::new(200.0), tutti_units::Hz::new(2000.0)],
    };
    assert_eq!(
        Band::ALL.map(|x| b.gain(x)),
        [0.9, 0.8, 0.7].map(Amplitude::new)
    );
    let from = Absorption::from_rt60(
        [Seconds::new(2.0), Seconds::new(1.0), Seconds::new(0.5)],
        [tutti_units::Hz::new(200.0), tutti_units::Hz::new(2000.0)],
        SampleSpan::of(4800.0),
        RATE,
    );
    let want = [2.0, 1.0, 0.5].map(|t| loop_gain(SampleSpan::of(4800.0), Seconds::new(t), RATE));
    assert_eq!(Band::ALL.map(|x| from.gain(x)), want);
}

/// A nested all-pass adds its group delay at DC, `D (1 + g) / (1 - g)`, to
/// the line's length: 100 frames at 0.5 add 300, and the RT60 follows the
/// longer loop.
#[test]
fn a_nested_all_pass_lengthens_the_loop_by_its_low_frequency_delay() {
    let ap = AllPass {
        delay: SampleSpan::of(100.0),
        gain: Gain::new(0.5),
        modulation: LineModulation::NONE,
        form: AllPassForm::Lattice,
    };
    assert_eq!(ap.low_frequency_delay(), Some(SampleSpan::of(300.0)));
    assert_eq!(
        AllPass {
            gain: Gain::UNITY,
            ..ap
        }
        .low_frequency_delay(),
        None
    );
    let g = 0.9;
    let plain = line(1000.0, Absorption::Gain(Amplitude::new(g)));
    let nested = Line {
        diffuser: Some(ap),
        ..plain
    };
    assert_eq!(nested.effective_length(), Some(SampleSpan::of(1300.0)));
    let r = |l| {
        network(&[NetworkStage::Fdn(fdn(&[l], Matrix::Diagonal))])
            .rt60(Band::Mid)
            .get()
    };
    assert!((r(nested) / r(plain) - 1.3).abs() < 1e-5);
}

/// A one-pole of pole `p` delays a loop by `|p| / (1 - |p|)` frames at its slowest, which the
/// effective length, the RT60 and the tail carry: a loop of a one-pole absorption rings longer
/// than one of the same gain without it, by that many frames a trip.
#[test]
fn a_one_pole_adds_its_group_delay_at_dc_to_the_loop() {
    let g = Amplitude::new(0.9);
    let pole = |damp| Absorption::OnePole { gain: g, damp };
    assert_eq!(pole(0.5).peak_delay(), SampleSpan::of(1.0));
    assert_eq!(pole(-0.5).peak_delay(), SampleSpan::of(1.0));
    assert!((pole(0.9).peak_delay().get() - 9.0).abs() < 1e-5);
    for none in [pole(1.5), pole(f64::NAN), Absorption::Gain(g)] {
        assert_eq!(none.peak_delay(), SampleSpan::ZERO);
    }
    assert_eq!(
        line(1000.0, pole(0.5)).effective_length(),
        Some(SampleSpan::of(1001.0))
    );
    let net = |a| network(&[NetworkStage::Fdn(fdn(&[line(1000.0, a)], Matrix::Diagonal))]);
    let rt = |a| net(a).rt60(Band::Low).get();
    // The same DC gain: the RT60 is that of a loop 1000 + 9 frames long.
    assert!((rt(pole(0.9)) / rt(Absorption::Gain(g)) - 1.009).abs() < 1e-5);
    let tail = |a| match net(a).tail(Db::new(-60.0)) {
        DecayTime::Finite(t) => t.get(),
        DecayTime::Unbounded => panic!("a finite tail"),
    };
    assert!(tail(pole(0.9)) > tail(Absorption::Gain(g)));
}

/// Stability: a gain below 1 under an orthogonal matrix is stable; a gain
/// of 1 in one band only is not (it never settles) and its tail is
/// unbounded; a Hadamard of 3 lines, a dense matrix of the wrong size, a
/// non-orthogonal circulant with `g ||A||_F >= 1`, and a negative or NaN
/// gain are not; all-pass gains of 1 or more are not.
#[test]
fn stability_follows_the_gains_and_the_matrix() {
    let stable = |lines: &[Line], m| network(&[NetworkStage::Fdn(fdn(lines, m))]).is_stable();
    let two = |g| {
        [
            line(100.0, Absorption::Gain(Amplitude::new(g))),
            line(130.0, Absorption::Gain(Amplitude::new(g))),
        ]
    };
    for m in [Matrix::Diagonal, Matrix::Householder, Matrix::Hadamard] {
        assert!(stable(&two(0.99), m.clone()));
        assert!(!stable(&two(1.01), m));
    }
    let low_only = Absorption::Bands {
        low: Amplitude::new(1.0),
        mid: Amplitude::new(0.9),
        high: Amplitude::new(0.9),
        crossover: [tutti_units::Hz::new(200.0), tutti_units::Hz::new(2000.0)],
    };
    let n = network(&[NetworkStage::Fdn(fdn(
        &[line(100.0, low_only), line(130.0, low_only)],
        Matrix::Hadamard,
    ))]);
    assert!(!n.is_stable() && !n.is_lossless());
    assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Unbounded);
    let three = [100.0, 130.0, 170.0].map(|m| line(m, Absorption::Gain(Amplitude::new(0.5))));
    assert!(!stable(&three, Matrix::Hadamard));
    assert!(stable(&three, Matrix::Householder));
    assert!(!stable(
        &two(0.5),
        Matrix::Dense(gain_row(&[1.0, 0.0, 0.0]))
    ));
    // A shift is an orthogonal circulant; [1, 1] is not, and is stable only
    // when g times its Frobenius norm 2 is below 1.
    let shift = Matrix::Circulant(gain_row(&[0.0, 1.0]));
    assert!(stable(&two(0.99), shift));
    let ones = Matrix::Circulant(gain_row(&[1.0, 1.0]));
    assert!(!stable(&two(0.6), ones.clone()));
    assert!(stable(&two(0.4), ones));
    let dense_orthogonal = Matrix::Dense(gain_row(&[0.0, 1.0, -1.0, 0.0]));
    assert!(stable(&two(0.99), dense_orthogonal));
    let dense_big = Matrix::Dense(gain_row(&[0.0, 2.0, 2.0, 0.0]));
    assert!(!stable(&two(0.6), dense_big));
    assert!(!stable(
        &[line(100.0, Absorption::Gain(Amplitude::new(-0.5)))],
        Matrix::Diagonal
    ));
    assert!(!stable(
        &[line(100.0, Absorption::Gain(Amplitude::new(f64::NAN)))],
        Matrix::Diagonal
    ));
    let ap = |gain: f64| {
        let gain = Gain::new(gain);
        network(&[NetworkStage::AllPass(
            Inline::from_slice(&[AllPass {
                delay: SampleSpan::of(10.0),
                gain,
                modulation: LineModulation::NONE,
                form: AllPassForm::Lattice,
            }])
            .unwrap(),
        )])
    };
    assert!(ap(0.9).is_stable() && !ap(1.0).is_stable() && !ap(-1.5).is_stable());
}

/// Every gain 1 under an orthogonal matrix is lossless and marginally
/// stable, with an unbounded tail; one band at 0.999 is not.
#[test]
fn a_lossless_loop_is_marginally_stable_with_an_unbounded_tail() {
    let one = Absorption::Gain(Amplitude::new(1.0));
    let frozen = network(&[NetworkStage::Fdn(fdn(
        &[line(100.0, one), line(130.0, one)],
        Matrix::Hadamard,
    ))]);
    assert!(frozen.is_lossless() && frozen.is_stable());
    assert_eq!(frozen.tail(Db::new(-60.0)), DecayTime::Unbounded);
    assert_eq!(frozen.rt60(Band::Mid).get(), f64::INFINITY);
    let almost = Absorption::Bands {
        low: Amplitude::new(1.0),
        mid: Amplitude::new(1.0),
        high: Amplitude::new(0.999),
        crossover: [tutti_units::Hz::new(200.0), tutti_units::Hz::new(2000.0)],
    };
    let n = network(&[NetworkStage::Fdn(fdn(
        &[line(100.0, almost), line(130.0, almost)],
        Matrix::Hadamard,
    ))]);
    assert!(!n.is_lossless());
    assert_eq!(n.tail(Db::new(f64::NAN)), DecayTime::Unbounded);
    assert!(!network(&[]).is_lossless());
}

/// Modal density is the total delay in seconds. Echo density at 1 kHz: a
/// diagonal of lines 10 and 20 frames long has nothing before 10 ms, 100 per
/// second from then, 150 from 20 ms (the times are a little past the line
/// lengths, so a delay never ties the probe); a Hadamard of the same lines multiplies
/// each trip (mean 15 ms): `2^1 / 0.015` at 10 ms, `2^3 / 0.015` at 45 ms,
/// capped at the rate.
#[test]
fn the_densities_are_the_modes_per_hertz_and_the_echoes_per_second() {
    let slow = SampleRate::of(1_000.0);
    let ls = [
        line(10.0, Absorption::Gain(Amplitude::new(0.5))),
        line(20.0, Absorption::Gain(Amplitude::new(0.5))),
    ];
    let mut diag = Network::new(slow);
    diag.stages
        .push(NetworkStage::Fdn(fdn(&ls, Matrix::Diagonal)))
        .unwrap();
    assert!((diag.modal_density().get() - 0.03).abs() < 1e-7);
    let e = |n: &Network, t: f64| n.echo_density(Seconds::new(t)).get();
    assert_eq!(e(&diag, 0.005), 0.0);
    assert!((e(&diag, 0.0105) - 100.0).abs() < 1e-3);
    assert!((e(&diag, 0.0205) - 150.0).abs() < 1e-3);
    assert!(
        (e(&diag, 5.0) - 150.0).abs() < 1e-3,
        "a diagonal never densifies"
    );
    let mut mixed = Network::new(slow);
    mixed
        .stages
        .push(NetworkStage::Fdn(fdn(&ls, Matrix::Hadamard)))
        .unwrap();
    assert_eq!(e(&mixed, 0.005), 0.0);
    assert!(
        (e(&mixed, 0.0105) - 2.0 / 0.015).abs() < 0.01,
        "{}",
        e(&mixed, 0.0105)
    );
    assert!(
        (e(&mixed, 0.0455) - 8.0 / 0.015).abs() < 0.1,
        "{}",
        e(&mixed, 0.0455)
    );
    assert_eq!(e(&mixed, 5.0), 1_000.0, "capped at one echo per frame");
    let fir = FirSummary {
        length: Samples::new(500),
        onset: Samples::new(0),
        latency: Samples::new(64),
        rt60: [Seconds::new(1.0); 3],
        energy: 1.0,
    };
    let mut f = Network::new(slow);
    f.stages.push(NetworkStage::Fir(fir)).unwrap();
    assert_eq!(e(&f, 0.1), 1_000.0);
    assert_eq!(e(&f, 0.6), 0.0);
    assert_eq!(f.modal_density().get(), 0.5);
}

/// A FIR reports its summary for `rt60` and its latency; the time-domain
/// stages have none; its length is its tail.
#[test]
fn a_fir_reports_its_summary() {
    let fir = FirSummary {
        length: Samples::new(24_000),
        onset: Samples::new(100),
        latency: Samples::new(256),
        rt60: [Seconds::new(2.0), Seconds::new(1.5), Seconds::new(1.0)],
        energy: 1.0,
    };
    let n = network(&[NetworkStage::Fir(fir)]);
    assert_eq!(Band::ALL.map(|b| n.rt60(b).get()), [2.0, 1.5, 1.0]);
    assert_eq!(n.latency(), Samples::new(256));
    assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Finite(Seconds::new(0.5)));
    assert_eq!(n.min_delay(), Some(SampleSpan::of(100.0)));
    assert_eq!(network(&[]).latency(), Samples::ZERO);
}

/// The shortest path: a series all-pass has a direct term, a tap or a line
/// its delay, the pre-delay adds; a network with no path has none to find.
#[test]
fn the_shortest_path_decides_the_direct_path() {
    let ap = NetworkStage::AllPass(
        Inline::from_slice(&[AllPass {
            delay: SampleSpan::of(100.0),
            gain: Gain::new(0.5),
            modulation: LineModulation::NONE,
            form: AllPassForm::Lattice,
        }])
        .unwrap(),
    );
    // A network is 111 KB: each is built and read in its own frame, never held across the test.
    let min_delay = |stages: &[NetworkStage]| network(stages).min_delay();
    let direct = |stages: &[NetworkStage]| network(stages).has_direct_path();
    assert!(direct(std::slice::from_ref(&ap)));
    let comb = NetworkStage::Fdn(fdn(
        &[line(100.0, Absorption::Gain(Amplitude::new(0.5)))],
        Matrix::Diagonal,
    ));
    let mut n = network(&[comb.clone(), ap]);
    assert_eq!(n.min_delay(), Some(SampleSpan::of(100.0)));
    assert!(!n.has_direct_path());
    n.pre_delay = Samples::new(20);
    assert_eq!(n.min_delay(), Some(SampleSpan::of(120.0)));
    // A lattice at g = 0 is a plain delay, no direct path; Freeverb's form
    // keeps its -1 at g = 0 and is direct.
    let plain = |form| {
        NetworkStage::AllPass(
            Inline::from_slice(&[AllPass {
                delay: SampleSpan::of(100.0),
                gain: Gain::new(0.0),
                modulation: LineModulation::NONE,
                form,
            }])
            .unwrap(),
        )
    };
    assert_eq!(
        min_delay(&[plain(AllPassForm::Lattice)]),
        Some(SampleSpan::of(100.0))
    );
    assert!(!direct(&[plain(AllPassForm::Lattice)]));
    assert!(direct(&[plain(AllPassForm::Freeverb)]));
    let tap = |d: f64| {
        NetworkStage::Taps(TapSet {
            taps: Inline::from_slice(&[Tap {
                delay: SampleSpan::of(d),
                gain: Gain::UNITY,
                channel: 0,
            }])
            .unwrap(),
        })
    };
    assert!(direct(&[tap(0.5)]));
    assert!(!direct(&[tap(1.0)]));
    assert_eq!(min_delay(&[tap(7.0), comb]), Some(SampleSpan::of(107.0)));
    assert_eq!(
        min_delay(&[]),
        Some(SampleSpan::ZERO),
        "no stages: nothing is added to the pre-delay"
    );
}

/// `NetworkCapacity::need` measures every figure of a network.
#[test]
fn a_capacity_measures_every_figure_of_its_network() {
    let g = Absorption::Gain(Amplitude::new(0.5));
    let mut f = fdn(
        &[
            line(100.0, g),
            Line {
                modulation: LineModulation {
                    depth: SampleSpan::of(2.0),
                    rate: tutti_units::Hz::new(1.0),
                },
                ..line(200.0, g)
            },
        ],
        Matrix::Hadamard,
    );
    f.output
        .rows
        .push(Gains::filled(Gain::UNITY, 2).unwrap())
        .unwrap();
    let mut n = network(&[NetworkStage::Fdn(f)]);
    n.pre_delay = Samples::new(10);
    let b = NetworkCapacity::need(&n, Db::new(-60.0));
    assert_eq!((b.lines, b.stages, b.taps, b.outputs), (2, 1, 0, 1));
    assert_eq!(
        (b.line, b.delay, b.pre_delay),
        (
            SampleSpan::of(202.0),
            SampleSpan::of(302.0),
            Samples::new(10)
        )
    );
}

fn one_pole(gain: f64, damp: f64) -> Absorption {
    Absorption::OnePole {
        gain: Amplitude::new(gain),
        damp,
    }
}

/// A one-pole whose pole is at or past the unit circle (or not a number)
/// has no response and is not stable, whatever its DC gain: the three-point
/// read of `|H|` alone would call `damp = 2.0` fine. A pole in
/// `(-1, 0)` is valid but peaks above its gain at Nyquist, which counts.
#[test]
fn a_one_pole_outside_the_unit_circle_is_not_stable() {
    let stable = |a| {
        network(&[NetworkStage::Fdn(fdn(
            &[line(100.0, a), line(130.0, a)],
            Matrix::Hadamard,
        ))])
        .is_stable()
    };
    for damp in [
        2.0,
        -2.0,
        1.0,
        -1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let a = one_pole(0.9, damp);
        for band in Band::ALL {
            assert_eq!(a.gain(band).get(), f64::INFINITY, "damp {damp}");
        }
        assert!(!stable(a), "damp {damp}");
        let f = fdn(&[line(100.0, a)], Matrix::Diagonal);
        assert_eq!(f.rt60_bound(RATE).get(), f64::INFINITY);
        let n = network(&[NetworkStage::Fdn(f)]);
        assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Unbounded);
    }
    // A valid low-pass pole is stable; a valid negative pole at gain 0.9
    // reads 0.9 * 1.5 / 0.5 = 2.7 at Nyquist and is not.
    assert!(stable(one_pole(0.9, 0.5)));
    let high = one_pole(0.9, -0.5).gain(Band::High).get();
    assert!((high - 2.7).abs() < 1e-5, "{high}");
    assert!(!stable(one_pole(0.9, -0.5)));
}

/// A line must have a positive length: a delay-free loop (length 0) is not
/// stable and not lossless, even at a gain of 1 under an orthogonal matrix. A
/// negative, NaN or infinite length, or modulation depth, is not a `SampleSpan`.
#[test]
fn a_delay_free_line_is_not_stable() {
    for l in [line(0.0, Absorption::Gain(Amplitude::new(0.5)))] {
        for gain in [0.5, 1.0] {
            let l = Line {
                absorption: Absorption::Gain(Amplitude::new(gain)),
                ..l
            };
            let f = fdn(
                &[l, line(130.0, Absorption::Gain(Amplitude::new(gain)))],
                Matrix::Hadamard,
            );
            assert!(!f.is_stable() && !f.is_lossless(), "{l:?} at {gain}");
        }
    }
    let ok = fdn(
        &[line(1.0, Absorption::Gain(Amplitude::new(0.5)))],
        Matrix::Diagonal,
    );
    assert!(ok.is_stable(), "one frame is a loop");
}

/// The orthogonality tolerance is sound. A circulant `1.0000045 I` of 16
/// lines has `A Aᵀ - I` entries of 9e-6: loose enough to pass for
/// orthogonal at a tolerance of 1e-5, which would let a gain of `1 - 2e-6`
/// (not lossless) through although the loop gain is `1 + 2.5e-6`. At 1e-6 it
/// is not orthogonal, the Frobenius bound (`4 g` here) applies, and it is
/// refused. The bound folded into the test is a bound: for a nearly
/// orthogonal matrix it is at least the true norm, and exactly 1 for an
/// exactly orthogonal one.
#[test]
fn the_orthogonality_tolerance_cannot_let_a_growing_loop_through() {
    let scaled = |c0: f64, n: usize| {
        let mut row = Gains::filled(Gain::SILENT, n).unwrap();
        row.as_mut_slice()[0] = Gain::new(c0);
        let lines: Vec<Line> = (0..n)
            .map(|i| {
                line(
                    100.0 + i as f64,
                    Absorption::Gain(Amplitude::new(1.0 - 2e-6)),
                )
            })
            .collect();
        fdn(&lines, Matrix::Circulant(row))
    };
    let loose = scaled(1.000_004_5, 16);
    assert!(!loose.orthogonal());
    assert!(
        !loose.is_stable(),
        "g = 1 - 2e-6 times norm 1.0000045 grows"
    );
    let exact = scaled(1.0, 64);
    assert!(exact.orthogonal() && exact.is_stable());
    assert_eq!(exact.norm_bound(), 1.0);
    // Nearly orthogonal: c = [1 + e/2, e/2] has norm a + b = 1 + e exactly.
    let eta = 9e-7f64;
    let near = fdn(
        &[
            line(100.0, Absorption::Gain(Amplitude::new(0.5))),
            line(130.0, Absorption::Gain(Amplitude::new(0.5))),
        ],
        Matrix::Circulant(gain_row(&[1.0 + eta / 2.0, eta / 2.0])),
    );
    let Matrix::Circulant(c) = &near.matrix else {
        unreachable!()
    };
    let truth = c[0].get() + c[1].get();
    assert!(near.orthogonal());
    assert!(
        near.norm_bound() >= truth,
        "{} < {truth}",
        near.norm_bound()
    );
    assert!(near.norm_bound() > 1.0);
}

/// For a mixing matrix with unequal lines the length-weighted RT60 is a
/// mean and undercuts the true decay, and the tail must not. Two lines of
/// 40 and 70 frames at 1 kHz, set for RT60 3 s and 1 s under a Hadamard
/// matrix: the estimate is `110 / (40/3 + 70)` = 1.32 s, the slowest line's
/// bound 3 s, and the state of the equations (simulated here, in `f64`)
/// last exceeds 1e-3 of its start at about 1.9 s: after the estimate, inside
/// the tail. For a diagonal matrix the bound equals the exact RT60.
#[test]
fn the_tail_covers_a_mixing_network_whose_lines_decay_differently() {
    let rate = SampleRate::of(1_000.0);
    let lines = [(40.0f64, 3.0f64), (70.0, 1.0)].map(|(m, t)| {
        line(
            m,
            Absorption::Gain(loop_gain(SampleSpan::of(m), Seconds::new(t), rate)),
        )
    });
    let mut mixed = Network::new(rate);
    mixed
        .stages
        .push(NetworkStage::Fdn(fdn(&lines, Matrix::Hadamard)))
        .unwrap();
    let estimate = mixed.rt60(Band::Mid).get();
    assert!((estimate - 1.32).abs() < 1e-3, "{estimate}");
    let bound = fdn(&lines, Matrix::Hadamard).rt60_bound(rate);
    assert!((bound.get() - 3.0).abs() < 1e-3);
    let DecayTime::Finite(tail) = mixed.tail(Db::new(-60.0)) else {
        panic!("finite")
    };
    assert!((tail.get() - (0.07 + 3.0)).abs() < 1e-3, "{tail:?}");

    // The equations, run: Hadamard (1/sqrt 2 [[1, 1], [1, -1]]) over two
    // shift registers, gain at the exit, an impulse in the first line.
    let g: Vec<f64> = lines
        .iter()
        .map(|l| l.absorption.gain(Band::Mid).get())
        .collect();
    let mut regs = [vec![0.0f64; 40], vec![0.0f64; 70]];
    regs[0][0] = 1.0;
    let energy = |r: &[Vec<f64>; 2]| r.iter().flatten().map(|x| x * x).sum::<f64>();
    let start = energy(&regs);
    let mut last_above = 0usize;
    for t in 1..20_000usize {
        let y = [regs[0].pop().unwrap() * g[0], regs[1].pop().unwrap() * g[1]];
        let h = std::f64::consts::FRAC_1_SQRT_2;
        regs[0].insert(0, h * (y[0] + y[1]));
        regs[1].insert(0, h * (y[0] - y[1]));
        if (energy(&regs) / start).sqrt() > 1e-3 {
            last_above = t;
        }
    }
    let actual = last_above as f64 / 1_000.0;
    assert!(
        actual > estimate,
        "the estimate {estimate} s undercuts the {actual} s the state needs"
    );
    assert!(
        actual <= tail.get(),
        "the tail {tail:?} must cover {actual} s"
    );

    let mut diagonal = Network::new(rate);
    diagonal
        .stages
        .push(NetworkStage::Fdn(fdn(&lines, Matrix::Diagonal)))
        .unwrap();
    let exact = diagonal.rt60(Band::Mid).get();
    let bound = fdn(&lines, Matrix::Diagonal).rt60_bound(rate);
    assert!((bound.get() - exact).abs() < 1e-4);
}

/// A measured-orthogonal matrix has a norm bound just above 1, and the
/// lag of the proof grows by `(m-1) log σ / -log(σρ)` frames; a structured
/// one has none.
#[test]
fn a_nearly_orthogonal_matrix_adds_a_lag_a_structured_one_does_not() {
    let g = Absorption::Gain(Amplitude::new(0.9));
    let lines = [line(100.0, g), line(130.0, g)];
    let structured = fdn(&lines, Matrix::Householder);
    assert_eq!(structured.lag_extra_frames(SampleRate::SR_48K), 0.0);
    let c = Matrix::Circulant(gain_row(&[1.000_000_2, 0.0]));
    let near = fdn(&lines, c);
    assert!(near.orthogonal() && near.lag_extra_frames(SampleRate::SR_48K) > 0.0);
    assert!(
        near.rt60_bound(SampleRate::SR_48K).get() > structured.rt60_bound(SampleRate::SR_48K).get()
    );
}

fn bands(low: f64, mid: f64, high: f64) -> Absorption {
    Absorption::Bands {
        low: Amplitude::new(low),
        mid: Amplitude::new(mid),
        high: Amplitude::new(high),
        crossover: [Hz::new(300.0), Hz::new(4_000.0)],
    }
}

/// The decay bound reads the absorption's largest response over every frequency, not its
/// three nominal band gains. Gains that fall with frequency peak at the low band's: the figure
/// is the nominal one exactly. A mid band above a short low band peaks between the crossovers
/// above every nominal gain: the bound is the line's own
/// `-60 m / (fs 20 log10 peak)`, longer than any band's, and a peak at or over 1 never decays.
#[test]
fn the_decay_bound_reads_the_peak_response_of_a_band_profile() {
    let one = |a: Absorption| fdn(&[line(1_000.0, a)], Matrix::Diagonal);

    let falling = one(bands(0.9, 0.7, 0.5));
    assert_eq!(falling.slowest_peak(RATE), falling.slowest(Band::Low));
    assert!(
        (falling.rt60_bound(RATE).get() - falling.rt60(Band::Low, RATE).get()).abs() < 1e-9,
        "the bound is the low band's decay"
    );

    let a = bands(0.05, 0.8, 0.8);
    let rising = one(a);
    let peak = a.sup(RATE);
    assert!(peak > 0.8, "{peak}");
    let (db_rho, _, _) = rising.slowest_peak(RATE).unwrap();
    for band in Band::ALL {
        assert!(db_rho > rising.slowest(band).unwrap().0, "{band:?}");
    }
    let want = -60.0 * 1_000.0 / (RATE.get() * 20.0 * peak.log10());
    assert!((rising.rt60_bound(RATE).get() - want).abs() < 1e-9 * want);
    assert!(rising.rt60_bound(RATE) > rising.rt60(Band::Mid, RATE));

    let over = bands(0.0, 0.99, 0.99);
    assert!(over.sup(RATE) > 1.0);
    assert_eq!(one(over).rt60_bound(RATE).get(), f64::INFINITY);
    let n = network(&[NetworkStage::Fdn(one(over))]);
    assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Unbounded);
}

/// The lines whose echoes can sum in one output channel deepen the floor the decay is measured
/// to: `20 log10 (Σ|c| / max|c|)` for the widest row. Four identical lines of 1 000 frames at
/// RT60 2 s, the row `[1, 1, 1, 1]` (4, +12.04 dB) over the row `[1, 0, 0, 0]`: the tail to
/// -60 dB is `1000 / fs + 2 (60 + 20 log10 4) / 60` seconds, and a row of unequal gains
/// `[1, 0.5, 0.5, 0]` is 2 lines. One line in every row is no margin.
#[test]
fn the_tail_adds_the_lines_that_sum_in_one_output_channel_to_the_floors_depth() {
    let g = loop_gain(SampleSpan::of(1_000.0), Seconds::new(2.0), RATE);
    let lines = [line(1_000.0, Absorption::Gain(g)); 4];
    let tail_with = |rows: &[&[f64]]| {
        let mut f = fdn(&lines, Matrix::Diagonal);
        let rows: Vec<Gains> = rows.iter().map(|r| gain_row(r)).collect();
        f.output = NetworkOutputMap {
            rows: Inline::from_slice(&rows).unwrap(),
        };
        let counted = f.coincident_lines(1.0);
        let DecayTime::Finite(t) = network(&[NetworkStage::Fdn(f)]).tail(Db::new(-60.0)) else {
            panic!("a damped line rings out")
        };
        (counted, t.get())
    };
    let transit = 1_000.0 / RATE.get();
    let at = |lines: f64| transit + 2.0 * (60.0 + 20.0 * lines.log10()) / 60.0;
    let (n, t) = tail_with(&[&[1.0, 1.0, 1.0, 1.0], &[1.0, 0.0, 0.0, 0.0]]);
    assert_eq!(n, 4.0);
    assert!((t - at(4.0)).abs() < 1e-6, "{t}");
    let (n, t) = tail_with(&[&[1.0, 0.5, 0.5, 0.0]]);
    assert_eq!(n, 2.0);
    assert!((t - at(2.0)).abs() < 1e-6, "{t}");
    let (n, t) = tail_with(&[&[0.0, 0.0, 1.0, 0.0], &[0.5, 0.0, 0.0, 0.0]]);
    assert_eq!(n, 1.0);
    assert!((t - at(1.0)).abs() < 1e-6, "{t}");
}

/// The width stage `(M + wS, M - wS)` mixes the output rows into each channel, and a mixing
/// matrix lets every tapped line's echo reach any line: with rows `[1, 1, 0, 0]` and
/// `[0, 0, 1, 1]` on diagonal lines, width 1 leaves each channel its two lines, width 0 puts all
/// four in both (`(M, M)`), and width 2 weighs the two rows `1.5` and `0.5`: `(1.5 + 1.5 + 0.5 +
/// 0.5) / 1.5` lines. A Householder matrix over the same four lines counts every tapped line.
#[test]
fn the_lines_that_sum_follow_the_width_and_a_mixing_matrix() {
    let lines = [line(1_000.0, Absorption::Gain(Amplitude::new(0.9))); 4];
    let with = |matrix: Matrix| {
        let mut f = fdn(&lines, matrix);
        let rows: Vec<Gains> = [[1.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 1.0]]
            .iter()
            .map(|r| gain_row(r))
            .collect();
        f.output = NetworkOutputMap {
            rows: Inline::from_slice(&rows).unwrap(),
        };
        f
    };
    let diagonal = with(Matrix::Diagonal);
    assert_eq!(diagonal.coincident_lines(1.0), 2.0);
    assert_eq!(diagonal.coincident_lines(0.0), 4.0);
    assert!((diagonal.coincident_lines(2.0) - 4.0 / 1.5).abs() < 1e-12);
    assert_eq!(with(Matrix::Householder).coincident_lines(1.0), 4.0);
}

/// A nested all-pass must be a lattice with `|g| < 1` for the loop analysis:
/// a Freeverb-form one (not unit magnitude) or `|g| >= 1` makes the line
/// unstable and not lossless, and a lattice adds its group delay without
/// loss.
#[test]
fn only_a_lattice_diffuser_is_inside_the_analysis() {
    let nested = |form, gain: f64| Line {
        diffuser: Some(AllPass {
            delay: SampleSpan::of(20.0),
            gain: Gain::new(gain),
            modulation: LineModulation::NONE,
            form,
        }),
        ..line(100.0, Absorption::Gain(Amplitude::new(1.0)))
    };
    let f = |l| {
        fdn(
            &[l, line(130.0, Absorption::Gain(Amplitude::new(1.0)))],
            Matrix::Hadamard,
        )
    };
    assert!(f(nested(AllPassForm::Lattice, 0.5)).is_lossless());
    assert!(f(nested(AllPassForm::Lattice, 0.5)).is_stable());
    for l in [
        nested(AllPassForm::Freeverb, 0.5),
        nested(AllPassForm::Lattice, 1.0),
    ] {
        assert!(!f(l).is_stable() && !f(l).is_lossless(), "{l:?}");
    }
}

/// A reflection `I - 2 v v^T` of a unit `v` is orthogonal (so a lossless loop
/// can use it), of a shorter `v` a contraction (`|1 - 2|v|^2| <= 1`, never a
/// gain), and of a longer one a gain `2|v|^2 - 1` the stability test refuses
/// at a loop gain of 1; a `v` of the wrong length fits no line count. The
/// norm is exact, not the generic bound.
#[test]
fn a_reflection_is_orthogonal_when_its_vector_is_a_unit() {
    let lines =
        [100.0, 130.0, 170.0, 190.0].map(|m| line(m, Absorption::Gain(Amplitude::new(1.0))));
    let reflect = |v: &[f64]| {
        network(&[NetworkStage::Fdn(fdn(
            &lines,
            Matrix::Reflection(gain_row(v)),
        ))])
    };
    let unit = reflect(&[0.5, 0.5, 0.5, 0.5]);
    assert!(unit.is_lossless() && unit.is_stable());
    // A unit vector off the flat one is just as orthogonal.
    let slanted = reflect(&[0.6, 0.8, 0.0, 0.0]);
    assert!(slanted.is_lossless());
    // Half the length: the eigenvalue along v is 1 - 2 * 0.25 = 0.5, so the
    // matrix is a contraction, not orthogonal: stable at gain 1 is refused
    // for lack of loss only, not for growth.
    let short = reflect(&[0.25, 0.25, 0.25, 0.25]);
    assert!(!short.is_lossless());
    // Longer than a unit: the eigenvalue 1 - 2 * 1.25 = -1.5 is a gain of 1.5.
    let long = reflect(&[1.0, 0.5, 0.0, 0.0]);
    assert!(!long.is_lossless() && !long.is_stable());
    let NetworkStage::Fdn(f) = &long.stages[0] else {
        unreachable!()
    };
    assert_eq!(f.norm_bound(), 1.5);
    // A unit vector that does not round exactly is a gain of 1 to within the
    // rounding of its entries, not the generic bound's `n` times that.
    let NetworkStage::Fdn(f) = &slanted.stages[0] else {
        unreachable!()
    };
    assert!(f.norm_bound() < 1.0 + 1.0e-7, "{}", f.norm_bound());
    let NetworkStage::Fdn(f) = &short.stages[0] else {
        unreachable!()
    };
    assert_eq!(
        f.norm_bound(),
        1.0,
        "a contraction counts as 1 where it is bounded"
    );
    // The wrong length fits no line count.
    let lossy =
        [100.0, 130.0, 170.0, 190.0].map(|m| line(m, Absorption::Gain(Amplitude::new(0.9))));
    let wrong = network(&[NetworkStage::Fdn(fdn(
        &lossy,
        Matrix::Reflection(gain_row(&[0.5, 0.5, 0.5])),
    ))]);
    assert!(!wrong.is_stable());
}
