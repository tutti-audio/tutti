use std::f64::consts::{PI, SQRT_2};

use tutti_buffers::{Planes, PlanesMut};
use tutti_types::{ChannelLayout, Hz, SampleRate};

use super::*;
use crate::{Complex64, CrossoverControls};

const RATE: SampleRate = SampleRate::SR_48K;

fn model(freqs: &[f32]) -> CrossoverModel {
    let f: Vec<Hz> = freqs.iter().map(|&f| Hz(f)).collect();
    CrossoverModel::new(&f, RATE).unwrap()
}

/// Runs `input` (one `Vec` per channel) through `bank` at `model` in blocks of the lengths
/// in `blocks`, cycled, and returns the output planes, `bands * channels` of them.
fn run<S: Float>(
    bank: &mut CrossoverBank,
    model: &CrossoverModel,
    input: &[Vec<S>],
    blocks: &[usize],
) -> Vec<Vec<S>> {
    let frames = input[0].len();
    let mut out = vec![vec![S::ZERO; frames]; input.len() * model.bands()];
    let mut at = 0;
    for &len in blocks.iter().cycle() {
        if at >= frames {
            break;
        }
        let end = (at + len).min(frames);
        let ins: Vec<&[S]> = input.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [S]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            model,
            Planes::new(&ins).expect("equal planes"),
            PlanesMut::new(&mut outs).expect("equal planes"),
        );
        at = end;
    }
    out
}

/// The unit impulse response of every band of a mono tree, in `f64` audio so the output is
/// the state's, unrounded.
fn impulse_response(freqs: &[f32], frames: usize) -> Vec<Vec<f64>> {
    let m = model(freqs);
    let mut x = vec![0.0f64; frames];
    x[0] = 1.0;
    run(
        &mut CrossoverBank::new(ChannelLayout::MONO),
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

const SETS: [&[f32]; 4] = [
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
                    let x = x(hz, f64::from(fc));
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
        let probes = PROBES
            .iter()
            .copied()
            .chain(freqs.iter().map(|&f| f64::from(f)));
        for hz in probes {
            for (b, y) in bands.iter().enumerate() {
                let want: f64 = freqs
                    .iter()
                    .enumerate()
                    .map(|(j, &fc)| {
                        let x4 = x(hz, f64::from(fc)).powi(4);
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
    let noise = |seed: usize| -> Vec<f32> {
        (0..700)
            .map(|i| (((i + 1) * (seed + 3)) % 41) as f32 / 20.0 - 1.0)
            .collect()
    };
    let input = vec![noise(1), vec![0.0; 700], noise(5)];
    let all = run(
        &mut CrossoverBank::new(ChannelLayout::from(3u16)),
        &m,
        &input,
        &[700],
    );
    assert_eq!(all.len(), 9);
    for c in 0..3 {
        let alone = run(
            &mut CrossoverBank::new(ChannelLayout::MONO),
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

/// Block independence: the model changes at frame 200 and 1 000, and any other cut of the
/// stream into calls gives the same frames bit for bit.
#[test]
fn any_split_renders_the_frames_of_the_whole() {
    let models = [
        model(&[300.0, 3_000.0]),
        model(&[450.0, 2_000.0]),
        model(&[300.0, 9_000.0]),
    ];
    let cuts = [0usize, 200, 1_000, 1_501];
    let input: Vec<Vec<f32>> = (0..2)
        .map(|c| {
            (0..1_501)
                .map(|i| ((i * 7 + c * 13) % 101) as f32 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |blocks: &[usize]| {
        let mut bank = CrossoverBank::new(ChannelLayout::STEREO);
        let mut out = vec![vec![0.0f32; 1_501]; 6];
        for (w, m) in cuts.windows(2).zip(&models) {
            let ins: Vec<Vec<f32>> = input.iter().map(|c| c[w[0]..w[1]].to_vec()).collect();
            for (plane, piece) in out.iter_mut().zip(run(&mut bank, m, &ins, blocks)) {
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

/// A model is whole from the next frame: after a zero input has left the state at 0, an
/// impulse through the model comes out of band 0 as the product of the first coefficients of
/// the band's sections (the low-pass twice, then the all-pass of the later split), the
/// model's own and not a blend with the one before.
#[test]
fn a_model_takes_effect_whole() {
    let first = model(&[300.0, 3_000.0]);
    let second = model(&[900.0, 5_000.0]);
    let mut bank = CrossoverBank::new(ChannelLayout::MONO);
    run(&mut bank, &first, &[vec![0.0f64; 40]], &[40]);
    let out = run(&mut bank, &second, &[vec![1.0f64]], &[1]);
    let (low, all_pass) = (second.coeffs[0].low.b0, second.coeffs[1].all_pass.b0);
    assert_eq!(out[0][0], low * low * all_pass);
}

/// `reset` is silence: after it, a zero input gives exactly zero in every band.
#[test]
fn a_reset_bank_is_silent() {
    let m = model(&[200.0, 2_000.0, 8_000.0]);
    let mut bank = CrossoverBank::new(ChannelLayout::STEREO);
    let noise: Vec<f32> = (0..512)
        .map(|i| ((i * 31) % 17) as f32 / 8.0 - 1.0)
        .collect();
    run(&mut bank, &m, &[noise.clone(), noise], &[512]);
    bank.reset();
    let out = run(&mut bank, &m, &[vec![0.0f32; 64], vec![0.0; 64]], &[64]);
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}

/// An output view with fewer planes than `bands * width` is filled as far as it goes, with the
/// planes the full view would give, and a view of more channels than the input has leaves the
/// rest untouched.
#[test]
fn a_narrow_view_is_filled_as_far_as_it_goes() {
    let m = model(&[500.0, 4_000.0]);
    let input: Vec<Vec<f32>> = (0..2)
        .map(|c| {
            (0..300)
                .map(|i| ((i + c * 5) % 23) as f32 / 11.0 - 1.0)
                .collect()
        })
        .collect();
    let full = run(
        &mut CrossoverBank::new(ChannelLayout::STEREO),
        &m,
        &input,
        &[300],
    );
    // Planes 0..=3: bands 0 and 1 of both channels; band 2 is missing.
    let mut out = vec![vec![9.0f32; 300]; 4];
    let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    CrossoverBank::new(ChannelLayout::STEREO).process(
        &m,
        Planes::new(&ins).unwrap(),
        PlanesMut::new(&mut outs).unwrap(),
    );
    assert_eq!(out, full[..4]);
    // One input channel into a stereo bank: channel 1's planes are not touched.
    let mut out = vec![vec![9.0f32; 300]; 6];
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
    CrossoverBank::new(ChannelLayout::STEREO).process(
        &m,
        Planes::new(&ins[..1]).unwrap(),
        PlanesMut::new(&mut outs).unwrap(),
    );
    for b in 0..3 {
        assert_eq!(out[b * 2], full[b * 2]);
        assert!(out[b * 2 + 1].iter().all(|&s| s == 9.0), "band {b}");
    }
}

/// A set at the limits of the controls is solved without leaving the bank's bound: every
/// band of every split stays under the sum of the sections' l1 norms (the model's
/// `l1_norm`), on a loud noise input.
#[test]
fn the_bands_stay_within_the_models_bound() {
    use crate::Lti;
    for set in [[20.0f32, 21.0], [23_000.0, 23_900.0], [100.0, 20_000.0]] {
        let controls = CrossoverControls {
            frequencies: [Hz(set[0]), Hz(set[1]), Hz(0.0), Hz(0.0), Hz(0.0)],
        };
        let m = CrossoverModel::designed(3, &controls, RATE).unwrap();
        let noise: Vec<f32> = (0..20_000)
            .map(|i| ((i * 7919) % 2003) as f32 / 1001.0 - 1.0)
            .collect();
        let out = run(
            &mut CrossoverBank::new(ChannelLayout::MONO),
            &m,
            &[noise],
            &[64],
        );
        let peak = out.iter().flatten().fold(0.0f32, |p, &s| p.max(s.abs()));
        assert!(
            peak.is_finite() && f64::from(peak) <= m.l1_norm(),
            "{set:?}: {peak}"
        );
    }
}
