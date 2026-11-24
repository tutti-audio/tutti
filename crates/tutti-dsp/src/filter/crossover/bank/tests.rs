use std::f64::consts::{PI, SQRT_2};

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Hz, SampleRate};

use super::*;
use crate::{Complex64, CrossoverBands, CrossoverControls, CrossoverModel, SOLVE_CEILING};
use tutti_buffers::{BlockSide, InputOutput};

const RATE: SampleRate = SampleRate::SR_48K;

fn model(freqs: &[f64]) -> CrossoverModel {
    let f: Vec<Hz> = freqs.iter().map(|&f| Hz::new(f)).collect();
    crate::testing::crossover_model(&f, RATE)
}

/// The grid the node's ramp takes: a design arrives in 16 frames.
const RAMP: u32 = 16;

/// A ramp at rest on `model`.
fn at_rest(model: &CrossoverModel) -> Ramp<CrossoverBlend> {
    Ramp::new(model.blend()).steps(RAMP)
}

/// Runs `input` (one `Vec` per channel) through `bank` at the ramp `at` in blocks of the
/// lengths in `blocks`, cycled, and returns the output planes, `bands * channels` of them.
fn run_ramp(
    bank: &mut CrossoverBank<f64>,
    ramp: &mut Ramp<CrossoverBlend>,
    input: &[Vec<f64>],
    blocks: &[usize],
) -> Vec<Vec<f64>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0; frames]; input.len() * ramp.target().bands()];
    let mut at = 0;
    for &len in blocks.iter().cycle() {
        if at >= frames {
            break;
        }
        let end = (at + len).min(frames);
        let ins: Vec<&[f64]> = input.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            ramp,
            &mut InputOutput::paired(
                Planes::new(&ins).expect("equal planes"),
                PlanesMut::new(&mut outs).expect("equal planes"),
            )
            .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// [`run_ramp`] at `model`, held.
fn run(
    bank: &mut CrossoverBank<f64>,
    model: &CrossoverModel,
    input: &[Vec<f64>],
    blocks: &[usize],
) -> Vec<Vec<f64>> {
    run_ramp(bank, &mut at_rest(model), input, blocks)
}

/// The unit impulse response of every band of a mono tree.
fn impulse_response(freqs: &[f64], frames: usize) -> Vec<Vec<f64>> {
    let m = model(freqs);
    let mut x = vec![0.0f64; frames];
    x[0] = 1.0;
    run(
        &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
        &m,
        &[x],
        &[frames],
    )
}

/// The DTFT of `y` at `hz`.
fn dtft(y: &[f64], hz: f64) -> Complex64 {
    let w = 2.0 * PI * hz / RATE.get();
    y.iter()
        .enumerate()
        .map(|(n, &v)| Complex64::from_polar(v, -w * n as f64))
        .sum()
}

/// `x = tan(pi f / fs) / tan(pi fc / fs)`: the frequency against a split's, as the
/// bilinear transform prewarped at the split warps it.
fn x(hz: f64, split: f64) -> f64 {
    (PI * hz / RATE.get()).tan() / (PI * split / RATE.get()).tan()
}

const SETS: [&[f64]; 4] = [
    &[1_000.0],
    &[120.0, 2_500.0],
    &[120.0, 1_000.0, 6_000.0],
    &[100.0, 300.0, 1_000.0, 3_000.0, 9_000.0],
];
const PROBES: [f64; 9] = [
    20.0, 100.0, 300.0, 999.0, 1_000.0, 2_500.0, 6_000.0, 9_000.0, 20_000.0,
];
/// The impulse responses ring out long before this (the slowest pole, a 100 Hz split's
/// Butterworth section, has radius 0.987 at 48 kHz: 1e-12 in 2 100 frames, twice for its
/// double pole); the DTFT of the truncated response is off by under 1e-12.
const N: usize = 1 << 15;
const TOL: f64 = 1e-9;

/// The bands sum to a second-order Butterworth all-pass per split. Each split's
/// low-pass and high-pass are `1/D` and `s^2/D` squared, `D = s^2 + sqrt2 s + 1` (the
/// Linkwitz-Riley form), and `1/D^2 + s^4/D^2 = (s^2 - sqrt2 s + 1)/D`, because
/// `(s^2 + 1)^2 - 2 s^2 = s^4 + 1`. A band split off before a later split runs through that
/// split's all-pass, so the sum is the product over the splits. The sum's magnitude is 1 at
/// every probe, and its phase is `-2 atan2(sqrt2 x, 1 - x^2)` per split, with `x` the
/// prewarped frequency against the split's (`tan(pi f / fs) / tan(pi fc / fs)`, the
/// bilinear map `s = (1/g)(1 - z^-1)/(1 + z^-1)`). Compared as the complex value of the DTFT of the
/// summed impulse responses, to 1e-9; a band missing its all-pass, a split off its
/// warped frequency or a sign wrong in a section moves the sum by a fraction of 1.
#[test]
fn the_bands_sum_to_the_all_pass_of_the_splits() {
    for freqs in SETS {
        let bands = impulse_response(freqs, N);
        let sum: Vec<f64> = (0..N).map(|n| bands.iter().map(|b| b[n]).sum()).collect();
        for hz in PROBES {
            let phase: f64 = freqs
                .iter()
                .map(|&fc| {
                    let x = x(hz, fc);
                    -2.0 * (SQRT_2 * x).atan2(1.0 - x * x)
                })
                .sum();
            let want = Complex64::from_polar(1.0, phase);
            let got = dtft(&sum, hz);
            assert!(
                (got - want).norm() < TOL,
                "{freqs:?} at {hz} Hz: {got} against {want}"
            );
        }
    }
}

/// Each band is `HP^2` of every split below it, `LP^2` of its own and the all-pass of every
/// split above; `LP^2` is `1 / (1 + x^4)` and `HP^2` is `x^4 / (1 + x^4)` in magnitude (the
/// squared Butterworth response, `|1/D(jx)|^2 = 1 / (1 + x^4)`). The DTFT of every band's
/// impulse response has that magnitude at the probes, the splits among them, to 1e-9.
/// A band is exactly 6.02 dB down (half) at its split where no earlier split's high-pass
/// reaches it, and the two bands of a split are equal there.
#[test]
fn each_band_is_the_product_of_the_splits_sections() {
    for freqs in SETS {
        let bands = impulse_response(freqs, N);
        let probes = PROBES.iter().copied().chain(freqs.iter().copied());
        for hz in probes {
            for (b, y) in bands.iter().enumerate() {
                let want: f64 = freqs
                    .iter()
                    .enumerate()
                    .map(|(j, &fc)| {
                        let x4 = x(hz, fc).powi(4);
                        match j.cmp(&b) {
                            std::cmp::Ordering::Less => x4 / (1.0 + x4),
                            std::cmp::Ordering::Equal => 1.0 / (1.0 + x4),
                            std::cmp::Ordering::Greater => 1.0,
                        }
                    })
                    .product();
                let got = dtft(y, hz).norm();
                assert!(
                    (got - want).abs() < TOL,
                    "{freqs:?} band {b} at {hz} Hz: {got} against {want}"
                );
            }
        }
    }
    let bands = impulse_response(&[1_000.0], N);
    for y in &bands {
        let half = dtft(y, 1_000.0).norm();
        assert!((half - 0.5).abs() < TOL, "{half}");
    }
    let ratio = dtft(&bands[0], 1_000.0) / dtft(&bands[1], 1_000.0);
    assert!((ratio - Complex64::new(1.0, 0.0)).norm() < TOL, "{ratio}");
}

/// The bands of a tree are written band-major: channel `c`'s band `b` is plane
/// `b * width + c`; each channel is the mono tree of its own input, and a channel with no
/// input leaves its planes silent.
#[test]
fn the_planes_are_band_major_and_each_channel_is_its_own_tree() {
    let m = model(&[300.0, 3_000.0]);
    let noise = |seed: usize| -> Vec<f64> {
        (0..700)
            .map(|i| (((i + 1) * (seed + 3)) % 41) as f64 / 20.0 - 1.0)
            .collect()
    };
    let input = vec![noise(1), vec![0.0; 700], noise(5)];
    let all = run(
        &mut CrossoverBank::<f64>::new(ChannelLayout::from(3u16)),
        &m,
        &input,
        &[700],
    );
    assert_eq!(all.len(), 9);
    for c in 0..3 {
        let alone = run(
            &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
            &m,
            std::slice::from_ref(&input[c]),
            &[700],
        );
        for b in 0..3 {
            assert_eq!(all[b * 3 + c], alone[b], "band {b} of channel {c}");
        }
    }
    assert!(
        all[1]
            .iter()
            .chain(&all[4])
            .chain(&all[7])
            .all(|&s| s == 0.0)
    );
}

/// Block independence: the design changes at frame 200 and 1 000 (a ramp of 16 frames from
/// each), and any other cut of the stream into calls gives the same frames bit for bit.
#[test]
fn any_split_renders_the_frames_of_the_whole() {
    let models = [
        model(&[300.0, 3_000.0]),
        model(&[450.0, 2_000.0]),
        model(&[300.0, 9_000.0]),
    ];
    let cuts = [0usize, 200, 1_000, 1_501];
    let input: Vec<Vec<f64>> = (0..2)
        .map(|c| {
            (0..1_501)
                .map(|i| ((i * 7 + c * 13) % 101) as f64 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |blocks: &[usize]| {
        let mut bank = CrossoverBank::<f64>::new(ChannelLayout::STEREO);
        let mut ramp = at_rest(&models[0]);
        let mut out = vec![vec![0.0f64; 1_501]; 6];
        for (w, m) in cuts.windows(2).zip(&models) {
            ramp.retarget(m.blend());
            let ins: Vec<Vec<f64>> = input.iter().map(|c| c[w[0]..w[1]].to_vec()).collect();
            for (plane, piece) in out
                .iter_mut()
                .zip(run_ramp(&mut bank, &mut ramp, &ins, blocks))
            {
                plane[w[0]..w[1]].copy_from_slice(&piece);
            }
        }
        out
    };
    let whole = render(&[1_501]);
    for blocks in [&[1usize][..], &[7, 33, 64], &[16], &[199, 1, 1_000]] {
        assert_eq!(render(blocks), whole, "blocks {blocks:?}");
    }
}

/// A step in the splits is a linear ramp of each split's integrator gain
/// `g = tan(pi f / fs)` over the grid, reached exactly. After a zero input has left the state
/// at 0, an impulse at frame `m` comes out of band 0 as the first coefficients of its
/// sections at the gains of step `m + 1` of 16: `a3^2 (1 - 2 sqrt2 a2')` with
/// `a3 = g^2 / (1 + g (g + sqrt2))` of split 0 (the low-pass twice) and `a2' = g' / (1 + g' (g'
/// + sqrt2))` of split 1 (its all-pass), the gains blended linearly (`g` of 1 kHz to
/// `g` of 4 kHz, and 5 kHz to 9 kHz). From step 16 on it is the target's, to the last bit.
#[test]
fn a_step_in_the_splits_ramps_linearly_in_the_gain_and_lands_exactly() {
    let (from, to) = (model(&[1_000.0, 5_000.0]), model(&[4_000.0, 9_000.0]));
    let g = |hz: f64| (PI * hz / RATE.get()).tan();
    let at = |m: usize, a: f64, b: f64| {
        let t = (m + 1).min(16) as f64 / 16.0;
        let g = g(a) + (g(b) - g(a)) * t;
        (g, g / (1.0 + g * (g + SQRT_2)))
    };
    for m in [0usize, 1, 7, 14, 15, 16, 25] {
        let mut x = vec![0.0f64; m + 1];
        x[m] = 1.0;
        let mut ramp = at_rest(&from);
        ramp.retarget(to.blend());
        let out = run_ramp(
            &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
            &mut ramp,
            &[x],
            &[m + 1],
        );
        let (g0, a2_0) = at(m, 1_000.0, 4_000.0);
        let a3 = g0 * a2_0;
        let (_, a2_1) = at(m, 5_000.0, 9_000.0);
        let want = a3 * a3 * (1.0 - 2.0 * SQRT_2 * a2_1);
        assert!(
            (out[0][m] - want).abs() < 1e-15,
            "frame {m}: {} against {want}",
            out[0][m]
        );
    }
    let (low, all_pass) = (to.coeffs[0].low().b0, to.coeffs[1].all_pass().b0);
    let mut ramp = at_rest(&from);
    ramp.retarget(to.blend());
    let mut x = vec![0.0f64; 40];
    x[39] = 1.0;
    let out = run_ramp(
        &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
        &mut ramp,
        &[x],
        &[40],
    );
    assert!(
        (out[0][39] - low * low * all_pass).abs() < 1e-15,
        "{}",
        out[0][39]
    );
    assert_eq!(ramp.now(), to.blend());
}

/// A ramp through the bank keeps the sections inside the derived bound for a split that
/// moves between 1 Hz and its cap every grid period with full-scale input: the bound of one
/// section is `tutti_fixtures::tpt`'s, in the product of the sections of a band.
#[test]
fn a_split_ramping_between_the_extremes_stays_within_the_derived_bound() {
    let (lo, hi) = (model(&[1.0]), model(&[23_952.0]));
    // The cap's gain.
    let section = tutti_fixtures::tpt::tpt_section_bound((PI * SOLVE_CEILING).tan());
    let bound = section.tap * section.tap;
    let frames = 200_000;
    for (name, alternate) in [("constant", false), ("alternating", true)] {
        let input: Vec<f64> = (0..frames)
            .map(|i| if alternate { [1.0, -1.0][i % 2] } else { 1.0 })
            .collect();
        let mut bank = CrossoverBank::<f64>::new(ChannelLayout::MONO);
        let mut ramp = at_rest(&lo);
        let mut peak = 0.0f64;
        for (period, chunk) in input.chunks(16).enumerate() {
            ramp.retarget(if period.is_multiple_of(2) { &hi } else { &lo }.blend());
            let out = run_ramp(&mut bank, &mut ramp, &[chunk.to_vec()], &[chunk.len()]);
            peak = out.iter().flatten().fold(peak, |p, &s| p.max(s.abs()));
        }
        assert!(
            peak.is_finite() && peak <= bound,
            "{name}: {peak} over {bound}"
        );
    }
}

/// `reset` is silence: after it, a zero input gives exactly zero in every band.
#[test]
fn a_reset_bank_is_silent() {
    let m = model(&[200.0, 2_000.0, 8_000.0]);
    let mut bank = CrossoverBank::<f64>::new(ChannelLayout::STEREO);
    let noise: Vec<f64> = (0..512)
        .map(|i| ((i * 31) % 17) as f64 / 8.0 - 1.0)
        .collect();
    run(&mut bank, &m, &[noise.clone(), noise], &[512]);
    bank.reset();
    let out = run(&mut bank, &m, &[vec![0.0f64; 64], vec![0.0; 64]], &[64]);
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}

/// A block whose output has fewer planes than `bands * width`, or whose input has fewer
/// channels than the bank, is refused whole: nothing is written, and nothing is filled as far
/// as it goes.
#[test]
fn a_block_of_another_shape_is_refused_untouched() {
    let m = model(&[500.0, 4_000.0]);
    let input: Vec<Vec<f64>> = (0..2).map(|_| vec![0.5f64; 300]).collect();
    let ins: Vec<&[f64]> = input.iter().map(|c| &c[..]).collect();
    // Planes 0..=3 are bands 0 and 1 of both channels; band 2 is missing.
    let mut out = vec![vec![9.0f64; 300]; 4];
    let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let refused = CrossoverBank::<f64>::new(ChannelLayout::STEREO).process(
        &mut at_rest(&m),
        &mut InputOutput::paired(
            Planes::new(&ins).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(
        refused,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 6,
            found: 4
        })
    );
    assert!(out.iter().flatten().all(|&s| s == 9.0));
    // One input channel into a stereo bank.
    let mut out = vec![vec![9.0f64; 300]; 6];
    let mut outs: Vec<&mut [f64]> = out.iter_mut().map(|c| &mut c[..]).collect();
    let refused = CrossoverBank::<f64>::new(ChannelLayout::STEREO).process(
        &mut at_rest(&m),
        &mut InputOutput::paired(
            Planes::new(&ins[..1]).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(
        refused,
        Err(BlockMismatch::Width {
            side: BlockSide::Input,
            expected: 2,
            found: 1
        })
    );
    assert!(out.iter().flatten().all(|&s| s == 9.0));
}

/// A set at the limits of the controls is solved without leaving the bank's bound: every
/// band of every split stays under the sum of the sections' l1 norms (the model's
/// `l1_norm`), on a loud noise input.
#[test]
fn the_bands_stay_within_the_models_bound() {
    use crate::Lti;
    for set in [[20.0f64, 21.0], [23_000.0, 23_900.0], [100.0, 20_000.0]] {
        let controls = CrossoverControls {
            frequencies: [
                Hz::new(set[0]),
                Hz::new(set[1]),
                Hz::ZERO,
                Hz::ZERO,
                Hz::ZERO,
            ],
        };
        let m = CrossoverModel::designed(CrossoverBands::of(3), &controls, RATE);
        let noise: Vec<f64> = (0..20_000)
            .map(|i| ((i * 7919) % 2003) as f64 / 1001.0 - 1.0)
            .collect();
        let out = run(
            &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
            &m,
            &[noise],
            &[64],
        );
        let peak = out.iter().flatten().fold(0.0f64, |p, &s| p.max(s.abs()));
        assert!(peak.is_finite() && peak <= m.l1_norm(), "{set:?}: {peak}");
    }
}

/// The bands of an `f64` tree on a signal of more than 24 bits sum to the Butterworth all-pass
/// of each split in turn: the recursion of `(s^2 - sqrt2 s + 1) / (s^2 + sqrt2 s + 1)` in
/// `f64` direct form I, from rest, frame for frame to 1e-12.
#[test]
fn an_f64_crossover_sums_to_the_all_pass_recursion_of_its_splits() {
    use crate::testing::butterworth_all_pass;
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let n = 4_800;
    let x = wide_signal(n);
    for splits in [&[1_000.0f64][..], &[500.0, 4_000.0]] {
        let bands = run(
            &mut CrossoverBank::<f64>::new(ChannelLayout::MONO),
            &model(splits),
            std::slice::from_ref(&x),
            &[n],
        );
        let got: Vec<f64> = (0..n).map(|i| bands.iter().map(|b| b[i]).sum()).collect();
        let want = splits
            .iter()
            .fold(x.clone(), |y, &fc| butterworth_all_pass(&y, fc, RATE.get()));
        assert_close(&got, &want, 1e-12);
    }
}

/// A 5 Hz split at 192 kHz, the crossover's range extreme, run in `f32`: both bands at the
/// split are `-6.02 dB` (`LP^2` and `HP^2` of the Butterworth pair, each `(1 / (j k))^2 =
/// -1 / 2` at `W = 1`, `k = sqrt 2`), in phase with each other.
///
/// Noise: a section's state words are at most `1 / sqrt 2` and each is rounded to `2^-24` a
/// step (uniform, standard deviation `2^-24 / sqrt 12 = 1.7e-8`); the pair's poles leak at
/// `g k / 2` a frame, `g = tan(pi fc / fs) = 8.2e-5`, so the injections add as a random walk of
/// `tau = 1 / (g k)` = 8 643 frames, and the section's output deviates by `2^-24 / sqrt 12 *
/// sqrt((k^2 + 1) tau) = 2.8e-6` (`k^2 + 1` the squared gain from the states to the taps).
/// A band is two sections in a row, the second's gain at the split at most 1, so its deviation
/// is at most two of those, `5.6e-6`. The test measures `N = 192 000` frames, whose expected
/// maximum is `sqrt(2 ln N) = 4.9` standard deviations; the bound is 1.5 times that, 7.4 of them,
/// for the non-Gaussian rounding tails and the correlated state noise.
#[test]
fn a_five_hz_split_at_192_khz_holds_its_bands_and_noise_in_f32() {
    use crate::testing::{sine_fit, steady_deviation};
    const FS: f64 = 192_000.0;
    const FC: f64 = 5.0;
    let whole = FS as usize;
    let w = 2.0 * PI * FC / FS;
    let model = crate::testing::crossover_model(&[Hz::new(FC)], SampleRate::of(FS));
    let input: Vec<f32> = (0..2 * whole)
        .map(|n| libm::sin(w * n as f64) as f32)
        .collect();
    let mut out = vec![vec![0.0f32; 2 * whole]; 2];
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    CrossoverBank::<f32>::new(ChannelLayout::MONO)
        .process(
            &mut at_rest(&model),
            &mut InputOutput::paired(
                Planes::new(&[&input[..]]).unwrap(),
                PlanesMut::new(&mut outs).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let g = libm::tan(PI * FC / FS);
    let tau = 1.0 / (g * SQRT_2);
    let sigma = 2.0 * 2f64.powi(-24) / 12f64.sqrt() * (3.0 * tau).sqrt();
    let bound = 1.5 * (2.0 * (whole as f64).ln()).sqrt() * sigma;
    let want = Complex64::new(-0.5, 0.0);
    for (band, plane) in out.iter().enumerate() {
        let y: Vec<f64> = plane.iter().map(|&v| f64::from(v)).collect();
        let fit = sine_fit(&y, w, whole);
        assert!((fit - want).norm() < bound, "band {band}: {fit}");
        let worst = steady_deviation(&y, w, whole, want);
        assert!(worst < bound, "band {band}: {worst:e} over {bound:e}");
    }
}
