use std::f64::consts::PI;

use crate::Complex64;
use tutti_buffers::{Planes, PlanesMut};
use tutti_types::ChannelLayout;
use tutti_units::{Db, Hz, Q, SampleRate};

use super::*;
use crate::filter::svf::model::tests::{TYPES, rbj};
use crate::{Lerp, SvfControls, SvfModel, SvfType};

const RATE: SampleRate = SampleRate::SR_48K;

fn coefficients(ty: SvfType, fc: f32, q: f32, gain: f32) -> SvfCoefficients<f64> {
    let controls = SvfControls {
        ty,
        freq: Hz::new(fc),
        q: Q::new(q),
        gain: Db::new(gain),
    };
    SvfCoefficients::from(&SvfModel::new(controls, RATE))
}

/// Runs `input` (one `Vec` per channel) through `bank` in blocks of the lengths in
/// `blocks`, cycled, and returns the output per channel.
fn run(
    bank: &mut SvfBank<f64>,
    glide: &mut Glide<SvfCoefficients<f64>>,
    input: &[Vec<f32>],
    blocks: &[usize],
) -> Vec<Vec<f32>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0f32; frames]; input.len()];
    let mut at = 0;
    for &len in blocks.iter().cycle() {
        if at >= frames {
            break;
        }
        let end = (at + len).min(frames);
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            glide,
            Planes::new(&ins).expect("equal planes"),
            PlanesMut::new(&mut outs).expect("equal planes"),
        );
        at = end;
    }
    out
}

/// The held bank's response is the RBJ cookbook's closed form: the DFT of its impulse
/// response, long enough to have decayed below 1e-12, matches the cookbook biquad of the
/// same controls at every probe frequency. The state is f64 and the output f32, so each
/// output sample is the exact one rounded to within `2^-24 |y[n]|`, and the DFT is off
/// by at most `2^-24 Σ|y[n]|` (plus 1e-12 for the f64 sums and the truncated decay).
#[test]
fn the_held_response_is_the_rbj_closed_form() {
    const N: usize = 1 << 15;
    for ty in TYPES {
        for (fc, q, gain) in [(1_000.0f32, 0.707f32, 9.0f32), (250.0, 4.0, -12.0)] {
            let mut bank = SvfBank::new(ChannelLayout::MONO);
            let mut glide = Glide::landed(coefficients(ty, fc, q, gain));
            let mut x = vec![0.0f32; N];
            x[0] = 1.0;
            let y = run(&mut bank, &mut glide, &[x], &[N]).remove(0);
            let rounding =
                y.iter().map(|&v| f64::from(v.abs())).sum::<f64>() * 2f64.powi(-24) + 1e-12;
            let want = rbj(ty, f64::from(fc), f64::from(q), f64::from(gain), RATE.get());
            for f in [50.0, f64::from(fc), 3_000.0, 15_000.0] {
                let w = 2.0 * PI * f / RATE.get();
                let got: Complex64 = y
                    .iter()
                    .enumerate()
                    .map(|(n, &v)| Complex64::from_polar(f64::from(v), -w * n as f64))
                    .sum();
                let h = want.response(w);
                assert!(
                    (got - h).norm() <= rounding,
                    "{ty:?} fc {fc} Q {q} {gain} dB at {f} Hz: {got} against {h}"
                );
            }
        }
    }
}

/// Block independence: a glide in flight, rendered in uneven blocks, gives the same
/// frames bit for bit as one block, because frame `i` reads step `taken + 1 + i` however
/// the frames are cut.
#[test]
fn any_split_renders_the_frames_of_one_block() {
    let input: Vec<Vec<f32>> = (0..3)
        .map(|c| {
            (0..997)
                .map(|i| ((i * 7 + c * 13) % 101) as f32 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |blocks: &[usize]| {
        let mut bank = SvfBank::new(ChannelLayout::from(3u16));
        let mut glide = Glide::landed(coefficients(SvfType::LowPass, 300.0, 0.707, 0.0)).by(16);
        glide.aim(coefficients(SvfType::LowPass, 6_000.0, 2.0, 0.0));
        run(&mut bank, &mut glide, &input, blocks)
    };
    let whole = render(&[997]);
    for blocks in [&[1usize][..], &[5, 3, 64], &[15, 1, 1, 200], &[16]] {
        assert_eq!(render(blocks), whole, "blocks {blocks:?}");
    }
}

/// A blended block lands on the new set at its last step: an impulse on the glide's 16th
/// frame, through a bank with no state, comes out as the new set's `a3` (with no state a
/// low-pass's output is `a3 x`), and the glide has no steps left.
#[test]
fn a_glide_arrives_on_the_new_set_at_its_last_frame() {
    let new = coefficients(SvfType::LowPass, 5_000.0, 0.707, 0.0);
    let mut blended = Glide::landed(coefficients(SvfType::LowPass, 500.0, 0.707, 0.0)).by(16);
    blended.aim(new);
    let mut x = vec![0.0f32; 16];
    x[15] = 1.0;
    let mut bank = SvfBank::new(ChannelLayout::MONO);
    let y = run(&mut bank, &mut blended, &[x], &[16]).remove(0);
    assert_eq!(y[15].to_bits(), (new.a3 as f32).to_bits());
    assert!(y[..15].iter().all(|&v| v == 0.0));
    assert!(!blended.moving());
}

/// Channels are independent: a bank of any width gives each channel exactly what a
/// one-channel bank gives it, held or gliding, whatever group the width falls into.
#[test]
fn each_channel_is_its_own_filter() {
    for width in [1u16, 2, 5, 8, 9, 16] {
        let input: Vec<Vec<f32>> = (0..width)
            .map(|c| {
                (0..300)
                    .map(|i| (((i + 1) * (c as usize + 3)) % 37) as f32 / 18.0 - 1.0)
                    .collect()
            })
            .collect();
        let glide = || {
            let mut g = Glide::landed(coefficients(SvfType::Bell, 800.0, 1.5, 6.0)).by(16);
            g.aim(coefficients(SvfType::Bell, 2_000.0, 0.8, -9.0));
            g
        };
        let mut bank = SvfBank::new(ChannelLayout::from(width));
        let all = run(&mut bank, &mut glide(), &input, &[300]);
        for (c, ch) in input.iter().enumerate() {
            let mut one = SvfBank::new(ChannelLayout::MONO);
            let alone = run(&mut one, &mut glide(), std::slice::from_ref(ch), &[300]).remove(0);
            assert_eq!(all[c], alone, "width {width}, channel {c}");
        }
    }
}

/// `reset` is silence: after it, a zero input gives exactly zero output.
#[test]
fn a_reset_bank_is_silent() {
    let mut bank = SvfBank::new(ChannelLayout::STEREO);
    let mut glide = Glide::landed(coefficients(SvfType::BandPass, 300.0, 50.0, 0.0));
    let noise: Vec<f32> = (0..512)
        .map(|i| ((i * 31) % 17) as f32 / 8.0 - 1.0)
        .collect();
    run(&mut bank, &mut glide, &[noise.clone(), noise], &[512]);
    bank.reset();
    let out = run(
        &mut bank,
        &mut glide,
        &[vec![0.0; 64], vec![0.0; 64]],
        &[64],
    );
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}

/// A glide's midpoint between two reachable designs is itself stable: an impulse
/// through the half-way coefficients decays (the last thousand frames of 150,000 peak
/// below half of the first thousand's). A biquad's stable region in `(a1, a2)` is convex,
/// so a blend of two stable sets stays inside; a glide that extrapolated would leave it.
#[test]
fn a_glides_midpoint_is_stable() {
    const FRAMES: usize = 150_000;
    let mut designs = Vec::new();
    for ty in TYPES {
        for (fc, q, gain) in [
            (20.0f32, 0.5f32, -24.0f32),
            (1_000.0, 0.707, 0.0),
            (12_000.0, 20.0, 24.0),
        ] {
            designs.push(coefficients(ty, fc, q, gain));
        }
    }
    for a in &designs {
        for b in &designs {
            let mid = a.lerp(b, 8, 16);
            let mut glide = Glide::landed(mid);
            let mut bank = SvfBank::new(ChannelLayout::MONO);
            let mut x = vec![0.0f32; FRAMES];
            x[0] = 1.0;
            let y = run(&mut bank, &mut glide, &[x], &[FRAMES]).remove(0);
            let peak = |r: core::ops::Range<usize>| y[r].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let (first, last) = (peak(0..1_000), peak(FRAMES - 1_000..FRAMES));
            assert!(
                first.is_finite() && last.is_finite() && last <= 0.5 * first,
                "{a:?} -> {b:?}: first {first}, last {last}"
            );
        }
    }
}
