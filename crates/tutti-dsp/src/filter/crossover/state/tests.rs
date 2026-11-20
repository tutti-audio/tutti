use super::super::CrossoverBlend;
use super::super::blend::Tpt;
use super::*;
use crate::testing::{amplitude_at, lock_in};
use tutti_units::{Hz, SampleRate};

/// One sample through `state` at `c`.
fn tick(state: &mut CrossoverState<f64>, c: &CrossoverBlend, x: f64) -> [f64; MAX_BANDS] {
    state.tick(&c.narrowed(), x)
}

const RATE: f64 = 48_000.0;
/// A whole number of periods of every tone on the 5 Hz grid.
const WINDOW: usize = 9_600;
const SETTLE: usize = 12_000;
/// The rounding of the input, each band, and the sum: at most `(bands + 2)` half-ulps of the
/// peak, each `2^-53`, in sections whose error gain is a few hundred at most, which the
/// lock-in only averages down.
const TOL: f64 = 1e-12;

/// A unit sine at `hz` (on the 5 Hz grid) at `rate`.
fn unit_sine(hz: f64, rate: f64, len: usize) -> Vec<f64> {
    (0..len)
        .map(|i| libm::sin(core::f64::consts::TAU * hz * i as f64 / rate))
        .collect()
}

/// The 5 Hz-grid frequencies of a log sweep from 20 Hz to 16 kHz.
fn sweep() -> Vec<f64> {
    (0..48)
        .map(|i| {
            let f = 20.0 * (16_000.0f64 / 20.0).powf(i as f64 / 47.0);
            (f / 5.0).round() * 5.0
        })
        .collect()
}

/// Band `b`'s phasor and the bands' summed amplitude for a unit tone at
/// `hz`, in steady state: the tree of `freqs` at 48 kHz run as the crossover runs it.
fn bands_at(freqs: &[f64], hz: f64) -> (Vec<(f64, f64)>, f64) {
    let freqs: Vec<Hz> = freqs.iter().map(|&f| Hz::new(f)).collect();
    let model = crate::testing::crossover_model(&freqs, SampleRate::of(RATE));
    let mut state = CrossoverState::<f64>::default();
    let c = model.blend();
    let x = unit_sine(hz, RATE, SETTLE + WINDOW);
    let nb = model.bands();
    let mut bands = vec![Vec::with_capacity(WINDOW); nb];
    let mut sum = Vec::with_capacity(WINDOW);
    for (i, &s) in x.iter().enumerate() {
        let out = tick(&mut state, &c, s);
        if i >= SETTLE {
            for (b, v) in bands.iter_mut().zip(out) {
                b.push(v);
            }
            sum.push(out[..nb].iter().sum::<f64>());
        }
    }
    (
        bands.iter().map(|b| lock_in(b, hz, RATE)).collect(),
        amplitude_at(&sum, hz, RATE),
    )
}

fn mag(z: (f64, f64)) -> f64 {
    z.0.hypot(z.1)
}

fn assert_flat(freqs: &[f64]) {
    for hz in sweep() {
        let (_, sum) = bands_at(freqs, hz);
        assert!(
            (sum - 1.0).abs() < TOL,
            "{} bands at {hz} Hz: the sum has amplitude {sum}",
            freqs.len() + 1
        );
    }
}

/// The two branches sum to unity magnitude across the audio band.
#[test]
fn two_bands_sum_to_flat_magnitude() {
    assert_flat(&[1_000.0]);
}

/// With the all-pass compensation, three and four bands sum flat too.
#[test]
fn three_and_four_bands_sum_to_flat_magnitude() {
    assert_flat(&[120.0, 2_500.0]);
    assert_flat(&[120.0, 1_000.0, 6_000.0]);
}

/// Six bands, the most, sum flat.
#[test]
fn six_bands_sum_to_flat_magnitude() {
    assert_flat(&[100.0, 300.0, 1_000.0, 3_000.0, 9_000.0]);
}

/// Each branch is 6.02 dB down at the crossover (half amplitude) and the
/// two are in phase there.
#[test]
fn each_branch_is_6_02_db_down_and_in_phase_at_the_crossover() {
    let (bands, _) = bands_at(&[1_000.0], 1_000.0);
    let (low, high) = (bands[0], bands[1]);
    assert!((mag(low) - 0.5).abs() < TOL, "low {}", mag(low));
    assert!((mag(high) - 0.5).abs() < TOL, "high {}", mag(high));
    // z_low / z_high = 1 when equal in magnitude and phase.
    let d = low.0 * high.0 + low.1 * high.1;
    let c = low.1 * high.0 - low.0 * high.1;
    let norm = high.0 * high.0 + high.1 * high.1;
    assert!((d / norm - 1.0).abs() < 4.0 * TOL, "ratio re {}", d / norm);
    assert!((c / norm).abs() < 4.0 * TOL, "ratio im {}", c / norm);
}

/// Each branch's magnitude is the analytic 4th-order one: low
/// `1 / (1 + x^4)`, high `x^4 / (1 + x^4)`, with `x` the frequency ratio
/// as the bilinear transform warps it, `tan(pi f / fs) / tan(pi fc / fs)`.
#[test]
fn branch_magnitudes_are_the_butterworth_squared_curve() {
    for hz in [100.0, 500.0, 800.0, 1_000.0, 1_250.0, 2_000.0, 8_000.0] {
        let (bands, _) = bands_at(&[1_000.0], hz);
        let warp = |f: f64| libm::tan(core::f64::consts::PI * f / RATE);
        let x4 = (warp(hz) / warp(1_000.0)).powi(4);
        assert!(
            (mag(bands[0]) - 1.0 / (1.0 + x4)).abs() < TOL,
            "low at {hz}"
        );
        assert!(
            (mag(bands[1]) - x4 / (1.0 + x4)).abs() < TOL,
            "high at {hz}"
        );
    }
}

/// Every band of a 3- or 4-band split has the same phase at every
/// frequency it carries: `LP^2`, `HP^2` and the all-pass all have phase
/// `-2 arg D`. Compared where a band has at least 0.1 of the tone (a
/// weaker band's phase is below the lock-in's resolution of `TOL / 0.1`).
#[test]
fn every_band_has_the_same_phase() {
    for freqs in [vec![120.0, 2_500.0], vec![120.0, 1_000.0, 6_000.0]] {
        let mut compared = 0;
        for hz in [
            100.0, 120.0, 200.0, 500.0, 1_000.0, 2_500.0, 4_000.0, 6_000.0, 9_000.0,
        ] {
            let (bands, _) = bands_at(&freqs, hz);
            let loud: Vec<_> = bands.iter().filter(|z| mag(**z) > 0.1).collect();
            for pair in loud.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let cross = (a.0 * b.1 - a.1 * b.0) / (mag(*a) * mag(*b));
                let dot = (a.0 * b.0 + a.1 * b.1) / (mag(*a) * mag(*b));
                assert!(
                    cross.abs() < 2.0 * TOL / 0.1 && dot > 0.0,
                    "{freqs:?} at {hz}: {cross} {dot}"
                );
                compared += 1;
            }
        }
        assert!(
            compared >= 3,
            "{freqs:?}: only {compared} overlapping pairs"
        );
    }
}

/// The gain of the highest split a crossover solves, `tan(pi SOLVE_CEILING)`.
fn max_gain() -> f64 {
    (core::f64::consts::PI * crate::SOLVE_CEILING).tan()
}

/// The two ends of the range: a split at the floor (1 Hz, `g` of about 1e-4, a section
/// that is nearly a Jordan block) and at the cap (`g` of about 318, a pole near -1).
fn ends(splits: usize) -> [CrossoverBlend; 2] {
    let at = |lo: f64, hi: f64| {
        let f: Vec<Hz> = [lo, hi][..splits].iter().map(|&f| Hz::new(f)).collect();
        // Strictly increasing and under Nyquist, as a crossover accepts; `design_into` clamps to the cap.
        crate::testing::crossover_model(&f, SampleRate::of(RATE)).blend()
    };
    [at(1.0, 2.0), at(23_951.0, 23_952.0)]
}

/// The inputs a bound is held for, each full scale: a constant, an alternation at Nyquist and
/// a seeded `+-1` sequence.
fn full_scale(kind: usize, i: usize, rng: &mut u32) -> f64 {
    match kind {
        0 => 1.0,
        1 => [1.0, -1.0][i % 2],
        _ => {
            *rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if *rng >> 31 == 0 { 1.0 } else { -1.0 }
        }
    }
}

fn norm(s: &Section<f64>) -> f64 {
    s.0[0].hypot(s.0[1])
}

/// The state and the outputs stay within the derived bound for a split that switches between
/// 1 Hz and its cap every frame and every 16 frames, for 10^6 frames at full scale (the
/// docs' bound: a section's state `|s|_2 <= S M` and tap `<= T M` for any sequence of gains up
/// to `G`; a band of `n` splits has at most `2 n` sections in series, the section at depth `d`
/// is fed by a signal within `T^d M`, so every state is within `S T^(2n-1) M` and every band
/// within `T^(2n) M`).
#[test]
fn the_states_and_bands_stay_within_the_bound_under_a_switching_split() {
    let section = tutti_fixtures::tpt::tpt_section_bound(max_gain());
    for splits in [1usize, 2] {
        let [low, high] = ends(splits);
        let state_bound = section.state * section.tap.powi(2 * splits as i32 - 1);
        let band_bound = section.tap.powi(2 * splits as i32);
        for period in [1usize, 16] {
            for kind in 0..3 {
                let mut state = CrossoverState::<f64>::default();
                let mut rng = 7u32;
                let (mut peak_state, mut peak_band) = (0.0f64, 0.0f64);
                for i in 0..1_000_000usize {
                    let c = if (i / period).is_multiple_of(2) {
                        &low
                    } else {
                        &high
                    };
                    let out = state.tick(&c.narrowed(), full_scale(kind, i, &mut rng));
                    peak_band = out.iter().fold(peak_band, |p, &y| p.max(y.abs()));
                    for s in (state.first.iter())
                        .chain(&state.low)
                        .chain(&state.high)
                        .chain(state.all_pass.iter().flatten())
                    {
                        peak_state = peak_state.max(norm(s));
                    }
                }
                assert!(
                    peak_state <= state_bound && peak_band <= band_bound,
                    "{splits} splits, period {period}, input {kind}: state {peak_state} over \\
                     {state_bound}, band {peak_band} over {band_bound}"
                );
            }
        }
    }
}

/// With zero input a section's state never grows, whatever the gain does: `|s'|^2 = |s|^2 - 4 g
/// k bp^2` for `P = I`. The first section of split 0 sees the input itself, so after a loud input
/// its state, carried through a split that switches every frame between 1 Hz and its cap and
/// through random gains, is non-increasing at every frame.
#[test]
fn a_section_at_zero_input_never_gains_energy_under_a_switching_split() {
    let [low, high] = ends(1);
    let mut state = CrossoverState::<f64>::default();
    let mut rng = 3u32;
    for i in 0..2_000 {
        state.tick(&low.narrowed(), full_scale(2, i, &mut rng));
    }
    let mut energy = norm(&state.first[0]).powi(2);
    assert!(energy > 0.0);
    for i in 0..200_000usize {
        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let g = 10f64.powf(f64::from(rng >> 8) / f64::from(1u32 << 24) * 7.0 - 4.5);
        let random = CrossoverBlend {
            bands: 2,
            split: [Tpt::new(g); crate::MAX_SPLITS],
        };
        let c = match i % 3 {
            0 => &low,
            1 => &high,
            _ => &random,
        };
        state.tick(&c.narrowed(), 0.0);
        let now = norm(&state.first[0]).powi(2);
        assert!(
            now <= energy * (1.0 + 1e-12),
            "frame {i}: {now} over {energy}"
        );
        energy = now;
    }
}

/// A held split rings out at the rate the section's own matrix says. At zero input the state
/// moves by the Cayley transform of `g A`, `C = (I + g A)(I - g A)^-1`, so `|s_n|_2 <= |C^n|_2
/// |s_0|_2`, computed here from `A` alone as the smallest `n` with `|C^n| |s_0| <= 1e-6`:
/// after a switching phase that leaves the first section loud, the real section is under 1e-6
/// after exactly that many frames of a held 1 kHz split.
#[test]
fn a_held_split_rings_out_by_the_time_its_matrix_gives() {
    let [low, high] = ends(1);
    let mut state = CrossoverState::<f64>::default();
    let mut rng = 11u32;
    for i in 0..4_000usize {
        let c = if (i / 16).is_multiple_of(2) {
            &low
        } else {
            &high
        };
        state.tick(&c.narrowed(), full_scale(2, i, &mut rng));
    }
    let s0 = norm(&state.first[0]);
    assert!(s0 > 1e-3, "the switching leaves the section loud: {s0}");
    let g = (core::f64::consts::PI * 1_000.0 / RATE).tan();
    let k = core::f64::consts::SQRT_2;
    // C = (I + gA)(I - gA)^-1, A = [[-k, -1], [1, 0]], with the 2x2 inverse written out.
    let m = [[1.0 + g * k, g], [-g, 1.0]];
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    let inv = [
        [m[1][1] / det, -m[0][1] / det],
        [-m[1][0] / det, m[0][0] / det],
    ];
    let p = [[1.0 - g * k, -g], [g, 1.0]];
    let c: [[f64; 2]; 2] = core::array::from_fn(|i| {
        core::array::from_fn(|j| p[i][0] * inv[0][j] + p[i][1] * inv[1][j])
    });
    let mul = |a: [[f64; 2]; 2], b: [[f64; 2]; 2]| -> [[f64; 2]; 2] {
        core::array::from_fn(|i| core::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j]))
    };
    // The spectral norm: the root of the largest eigenvalue of C^T C.
    let spectral = |a: [[f64; 2]; 2]| -> f64 {
        let ata = [
            [
                a[0][0] * a[0][0] + a[1][0] * a[1][0],
                a[0][0] * a[0][1] + a[1][0] * a[1][1],
            ],
            [
                a[0][0] * a[0][1] + a[1][0] * a[1][1],
                a[0][1] * a[0][1] + a[1][1] * a[1][1],
            ],
        ];
        let (tr, det) = (
            ata[0][0] + ata[1][1],
            ata[0][0] * ata[1][1] - ata[0][1] * ata[1][0],
        );
        (tr / 2.0 + (tr * tr / 4.0 - det).max(0.0).sqrt()).sqrt()
    };
    let (mut power, mut n) = (c, 1usize);
    while spectral(power) * s0 > 1e-6 {
        power = mul(power, c);
        n += 1;
        assert!(n < 100_000, "never decays");
    }
    let held = crate::testing::crossover_model(&[Hz::new(1_000.0)], SampleRate::of(RATE)).blend();
    for _ in 0..n {
        state.tick(&held.narrowed(), 0.0);
    }
    assert!(
        norm(&state.first[0]) <= 1e-6,
        "{} after {n} frames",
        norm(&state.first[0])
    );
}

/// The step's identities at any gain, for the matrices the docs name: `V(s') - V(s) = -2 g
/// v^T Q v + 4 g (P v)_1 x` with `P = [[1, e], [e, 1 + k e]]`, `Q = diag(2k - 2e, 2e)` and
/// `e = 3/8`, and `|s'|^2 - |s|^2 = -4 g k bp^2 + 4 g bp x` for `P = I`, from random states and
/// inputs at gains from 1e-4 to 300.
#[test]
fn the_energy_identities_of_the_docs_hold_at_every_gain() {
    const E: f64 = 3.0 / 8.0;
    let k = core::f64::consts::SQRT_2;
    let v = |s: [f64; 2]| s[0] * s[0] + 2.0 * E * s[0] * s[1] + (1.0 + k * E) * s[1] * s[1];
    let mut rng = 5u32;
    let mut next = || {
        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(rng >> 8) / f64::from(1u32 << 24)
    };
    for _ in 0..10_000 {
        let g = 10f64.powf(next() * 6.5 - 4.0);
        let tpt = Tpt::new(g);
        let mut section = Section([next() * 4.0 - 2.0, next() * 4.0 - 2.0]);
        let x = next() * 2.0 - 1.0;
        let before = section.0;
        let taps = section.tick(&tpt.narrowed(), x);
        let after = section.0;
        let (bp, lp) = (taps.bp, taps.lp);
        let dv = v(after) - v(before);
        let want = -2.0 * g * ((2.0 * k - 2.0 * E) * bp * bp + 2.0 * E * lp * lp)
            + 4.0 * g * (bp + E * lp) * x;
        let scale = v(before).max(v(after)).max(1.0);
        assert!(
            (dv - want).abs() < 1e-9 * scale,
            "g {g}: {dv} against {want}"
        );
        let de = after[0].powi(2) + after[1].powi(2) - before[0].powi(2) - before[1].powi(2);
        let want = -4.0 * g * k * bp * bp + 4.0 * g * bp * x;
        assert!(
            (de - want).abs() < 1e-9 * scale,
            "g {g}: {de} against {want}"
        );
    }
}

type M2 = [[f64; 2]; 2];

fn mul2(a: M2, b: M2) -> M2 {
    core::array::from_fn(|i| core::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j]))
}

fn apply(a: M2, v: [f64; 2]) -> [f64; 2] {
    [
        a[0][0] * v[0] + a[0][1] * v[1],
        a[1][0] * v[0] + a[1][1] * v[1],
    ]
}

/// The impulse responses of a section's four taps at a held gain `g`, `n` samples, from its
/// matrices alone: with `A = [[-k, -1], [1, 0]]` and `R = (I - g A)^-1`, the taps are `v = R
/// (s + g b x)` and the state `s' = 2 v - s`, so an impulse gives `v_0 = g R b`, `s_1 = 2 v_0`
/// and `v_n = R C^(n-1) s_1` with `C = (I + g A) R`. The order is bp, lp, hp, ap.
fn impulse_taps(g: f64, n: usize) -> [Vec<f64>; 4] {
    let k = core::f64::consts::SQRT_2;
    let m = [[1.0 + g * k, g], [-g, 1.0]];
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    let r = [
        [m[1][1] / det, -m[0][1] / det],
        [-m[1][0] / det, m[0][0] / det],
    ];
    let c = mul2([[1.0 - g * k, -g], [g, 1.0]], r);
    let v0 = apply(r, [g, 0.0]);
    let mut s = [2.0 * v0[0], 2.0 * v0[1]];
    let mut out: [Vec<f64>; 4] = core::array::from_fn(|_| Vec::with_capacity(n));
    for i in 0..n {
        let v = if i == 0 { v0 } else { apply(r, s) };
        if i > 0 {
            s = apply(c, s);
        }
        let x = if i == 0 { 1.0 } else { 0.0 };
        out[0].push(v[0]);
        out[1].push(v[1]);
        out[2].push(x - k * v[0] - v[1]);
        out[3].push(x - 2.0 * k * v[0]);
    }
    out
}

/// A section at a held gain is time-invariant, so under `|x| <= M` a tap's peak is its l1 norm
/// times `M`, reached by the input that is the sign of the time-reversed impulse response. The
/// impulse responses come from the section's matrices (`impulse_taps`), not from the
/// code: the section reproduces them to 1e-12, and the worst-case input drives each tap to its
/// l1 norm to 1e-9 and no random `+-1` input passes it. A section whose gain grew by 1.1 or more
/// would show it, where the switching bound (about 10^3 times the worst measured) would not.
#[test]
fn a_held_section_peaks_at_its_l1_norm_and_no_higher() {
    for g in [0.02f64, 0.0661, 1.0, 20.0] {
        let tpt = Tpt::new(g);
        let n = 4_000_000.min((40.0 / g.min(1.0)) as usize * 40);
        let h = impulse_taps(g, n);
        let mut section = Section::<f64>::default();
        for i in 0..200 {
            let t = section.tick(&tpt.narrowed(), if i == 0 { 1.0 } else { 0.0 });
            let got = [t.bp, t.lp, t.high_pass(), t.all_pass()];
            for (tap, want) in h.iter().enumerate() {
                assert!((got[tap] - want[i]).abs() < 1e-12, "g {g} tap {tap} at {i}");
            }
        }
        for (tap, h) in h.iter().enumerate() {
            let l1: f64 = h.iter().map(|v| v.abs()).sum();
            assert!(
                h[n - 1].abs() < 1e-12,
                "g {g}: the response is not over in {n} frames"
            );
            // The worst input: x[j] = sign h[m - j], read at the last frame m = n - 1.
            let mut section = Section::<f64>::default();
            let mut last = 0.0;
            for j in 0..n {
                let x = if h[n - 1 - j] >= 0.0 { 1.0 } else { -1.0 };
                let t = section.tick(&tpt.narrowed(), x);
                last = [t.bp, t.lp, t.high_pass(), t.all_pass()][tap];
            }
            assert!(
                (last - l1).abs() < 1e-9 * l1.max(1.0),
                "g {g} tap {tap}: {last} against {l1}"
            );
            let mut section = Section::<f64>::default();
            let mut rng = 17u32;
            for i in 0..200_000 {
                let t = section.tick(&tpt.narrowed(), full_scale(2, i, &mut rng));
                let y = [t.bp, t.lp, t.high_pass(), t.all_pass()][tap];
                assert!(
                    y.abs() <= l1 * (1.0 + 1e-12),
                    "g {g} tap {tap}: {y} over {l1}"
                );
            }
        }
    }
}
