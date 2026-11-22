use std::f64::consts::PI;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Drive, Hz, Resonance, SampleRate};

use super::*;
use crate::Complex64;
use crate::{LadderControls, LadderDesign};
use tutti_buffers::InputOutput;

const RATE: SampleRate = SampleRate::SR_48K;
const TYPES: [LadderType; 4] = [
    LadderType::LP12,
    LadderType::LP24,
    LadderType::HP12,
    LadderType::HP24,
];

fn design<F: Float + SimdLanes>(ty: LadderType, fc: f64, res: f64, drive: f64) -> LadderBlend<F> {
    let controls = LadderControls {
        ty,
        freq: Hz::new(fc),
        res: Resonance::new(res),
        drive: Drive::new(drive),
    };
    LadderBlend::from(&LadderModel::new(controls, RATE))
}

/// Runs `input` (one `Vec` per channel) through `bank` in blocks of the lengths in
/// `blocks`, cycled, and returns the output per channel.
fn run<F: Float + SimdLanes>(
    bank: &mut LadderBank<F>,
    ramp: &mut Ramp<LadderBlend<F>>,
    input: &[Vec<F>],
    blocks: &[usize],
) -> Vec<Vec<F>> {
    let frames = input[0].len();
    let mut out = vec![vec![F::ZERO; frames]; input.len()];
    let mut at = 0;
    for &len in blocks.iter().cycle() {
        if at >= frames {
            break;
        }
        let end = (at + len).min(frames);
        let ins: Vec<&[F]> = input.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
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

/// A unit impulse of `amplitude` and then `frames - 1` zeros.
fn impulse(amplitude: f64, frames: usize) -> Vec<f64> {
    let mut x = vec![0.0; frames];
    x[0] = amplitude;
    x
}

/// The ladder held at `ty`, `fc`, `res` (unity drive): the impulse response of a mono `f64`
/// bank in `f64` audio.
fn held_response(ty: LadderType, fc: f64, res: f64, amplitude: f64, frames: usize) -> Vec<f64> {
    let mut bank = LadderBank::<f64>::new(ChannelLayout::MONO);
    let mut ramp = Ramp::new(design(ty, fc, res, 1.0));
    run(
        &mut bank,
        &mut ramp,
        &[impulse(amplitude, frames)],
        &[frames],
    )
    .remove(0)
}

/// The DTFT of `y` at `hz`.
fn dtft(y: &[f64], hz: f64) -> Complex64 {
    let w = 2.0 * PI * hz / RATE.get();
    y.iter()
        .enumerate()
        .map(|(n, &v)| Complex64::from_polar(v, -w * n as f64))
        .sum()
}

/// The continuous ladder in `s/wc`: `1/((1 + s/wc)^4 + k)` for the 24 dB low-pass, with the
/// other taps over the same denominator (`L = 1/(1 + s/wc)` is the stage; the taps are
/// `L^2 u`, `L^4 u`, `(1 - L)^2 u`, `(1 - L)^4 u` with `u = x / (1 + k L^4)`).
/// Mapped to `z` by the bilinear map prewarped at the cutoff,
/// `s/wc = (1/g)(1 - z^-1)/(1 + z^-1)`, `g = tan(pi fc / fs)`, which is the map the
/// ladder's trapezoidal stages realise.
fn closed_form(ty: LadderType, fc: f64, res: f64, hz: f64) -> Complex64 {
    let g = (PI * fc / RATE.get()).tan();
    let k = 4.0 * res;
    let zi = Complex64::from_polar(1.0, -2.0 * PI * hz / RATE.get());
    let s = (1.0 - zi) / (1.0 + zi) / g;
    let p = Complex64::new(1.0, 0.0) + s;
    let den = p * p * p * p + k;
    match ty {
        LadderType::LP12 => p * p / den,
        LadderType::LP24 => Complex64::new(1.0, 0.0) / den,
        LadderType::HP12 => s * s * p * p / den,
        LadderType::HP24 => s * s * s * s / den,
    }
}

/// The held response is the continuous ladder `1/((1 + s/wc)^4 + k)` (and the other three
/// taps over its denominator), `k = 4 res`, under the bilinear map prewarped at the
/// cutoff. The DTFT of the impulse response of an `f64` bank in `f64` audio, with an
/// impulse of 1e-7 so the saturator is linear (`tanh x = x - x^3/3`: 3e-14 of the signal),
/// is the closed form's value to 1e-9 at probes from 50 Hz to 20 kHz. The response is
/// 32 768 frames long: the slowest pole, at `res` 0.9, has continuous real part
/// `wc (-1 + k^(1/4)/sqrt2) = -0.026 wc`, so 1e-12 is 1 100 frames at 1 kHz and 7 000 at
/// 100 Hz. A cutoff un-prewarped, a stage gain off by a part in 1e6 or a tap read from
/// the wrong stage moves a probe by far more than the tolerance.
#[test]
fn the_held_response_is_the_bilinear_image_of_the_continuous_ladder() {
    const N: usize = 1 << 15;
    let amplitude = 1e-7;
    for ty in TYPES {
        for (fc, res) in [
            (1_000.0f64, 0.0f64),
            (1_000.0, 0.5),
            (1_000.0, 0.9),
            (4_000.0, 0.9),
            (100.0, 0.7),
        ] {
            let y: Vec<f64> = held_response(ty, fc, res, amplitude, N)
                .iter()
                .map(|v| v / amplitude)
                .collect();
            for hz in [50.0, 500.0, 975.0, 1_000.0, 2_000.0, 9_000.0, 20_000.0] {
                let want = closed_form(ty, fc, res, hz);
                let got = dtft(&y, hz);
                assert!(
                    (got - want).norm() < 1e-9 * want.norm().max(1.0),
                    "{ty:?} fc {fc} res {res} at {hz} Hz: {got} against {want}"
                );
            }
        }
    }
}

/// The pole of the continuous ladder nearest the imaginary axis,
/// `s = wc (-1 + k^(1/4) e^(i pi/4))` (from `(1 + s/wc)^4 = -k`), taken by the bilinear map
/// prewarped at the cutoff, `z = (1 + g s/wc) / (1 - g s/wc)`: the discrete pole whose
/// modulus is the per-frame decay and whose angle is the frequency of the ringing.
fn dominant_pole(fc: f64, res: f64) -> Complex64 {
    let g = (PI * fc / RATE.get()).tan();
    let k = 4.0 * res;
    let s = Complex64::new(-1.0, 0.0) + Complex64::from_polar(k.powf(0.25), PI / 4.0);
    (1.0 + g * s) / (1.0 - g * s)
}

/// How far `y` over `len` frames from `start` is from a mode with the pole pair `z`,
/// `y[n+1] = 2 Re(z) y[n] - |z|^2 y[n-1]`, relative to the window's peak.
fn mode_residual(y: &[f64], start: usize, len: usize, z: Complex64) -> f64 {
    let window = &y[start - 1..start + len + 1];
    let peak = window.iter().fold(0.0f64, |p, &v| p.max(v.abs()));
    let worst = window
        .windows(3)
        .map(|w| (w[2] - 2.0 * z.re * w[1] + z.norm_sqr() * w[0]).abs())
        .fold(0.0f64, f64::max);
    worst / peak
}

/// The peak of `y` over the 48 frames from `start`, a whole period of 1 kHz at 48 kHz.
fn peak48(y: &[f64], start: usize) -> f64 {
    y[start..start + 48]
        .iter()
        .fold(0.0, |p, &v| f64::max(p, v.abs()))
}

/// At `k = 4` (a resonance of 1) the continuous poles are `s = +-i wc` and the prewarped map
/// puts the discrete ones on the unit circle at the cutoff: from an impulse the ladder
/// sustains a sinusoid at `fc`. An impulse of 2e-4 keeps the saturator's input under 1e-2,
/// where `tanh` costs under 1e-5 of the loop gain, so the loop is the linear one (the other
/// poles are at `-2 wc`, `e^-2000` of the start by frame 10 000). From frame 10 000 on, 100
/// frames at each of 10 000, 50 000 and 100 000 satisfy the recurrence of a sinusoid at
/// `fc`, `y[n+1] = 2 cos(w) y[n] - y[n-1]`, to 1e-6 of the peak (a pole at radius
/// `1 - 1e-5` or a frequency 0.01% off the cutoff misses it), and the peak over a period is
/// the same at the three to 1e-3: neither decaying nor growing over two seconds.
#[test]
fn the_ladder_self_oscillates_at_the_cutoff_at_k_four() {
    const N: usize = 100_148;
    let y = held_response(LadderType::LP24, 1_000.0, 1.0, 2e-4, N);
    let on_the_circle = Complex64::from_polar(1.0, 2.0 * PI * 1_000.0 / RATE.get());
    let pole = dominant_pole(1_000.0, 1.0);
    assert!(
        (pole - on_the_circle).norm() < 1e-12,
        "{pole} against {on_the_circle}"
    );
    let reference = peak48(&y, 10_000);
    assert!(reference > 1e-6, "it rings: {reference}");
    for start in [10_000, 50_000, 100_000] {
        let residual = mode_residual(&y, start, 100, on_the_circle);
        assert!(residual < 1e-6, "frame {start}: {residual}");
        let ratio = peak48(&y, start) / reference;
        assert!((ratio - 1.0).abs() < 1e-3, "frame {start}: {ratio}");
    }
}

/// Just below `k = 4` the ringing is the dominant pole's: at resonances 0.99 and 0.95 the
/// frames from 8 000 and from 32 000 satisfy the recurrence of the decaying sinusoid
/// `y[n+1] = 2 Re(z) y[n] - |z|^2 y[n-1]` for the pole `z` of the closed form
/// ([`dominant_pole`]), to 1e-6 of the window's peak, and the decay between them is
/// `|z|^24000` (`e^-7.9` at 0.99): the log of the ratio of the mode's amplitudes at the two
/// windows is `24000 ln|z|` to 1%.
#[test]
fn just_below_k_four_the_oscillation_decays_at_the_poles_modulus() {
    for res in [0.99f64, 0.95] {
        let y = held_response(LadderType::LP24, 1_000.0, res, 1e-4, 40_000);
        let z = dominant_pole(1_000.0, res);
        assert!(z.norm() < 1.0);
        for start in [8_000, 32_000] {
            let residual = mode_residual(&y, start, 100, z);
            assert!(residual < 1e-6, "res {res} frame {start}: {residual}");
        }
        // The amplitude of the mode at `n` from two successive frames of the decaying
        // sinusoid `a r^n cos(th n + ph)`:
        // `(a r^n sin th)^2 = y[n]^2 + r^2 y[n-1]^2 - 2 r cos(th) y[n] y[n-1]`.
        let (r, th) = (z.norm(), z.arg());
        let amp = |n: usize| {
            let sq = y[n].powi(2) + (r * y[n - 1]).powi(2) - 2.0 * r * th.cos() * y[n] * y[n - 1];
            sq.sqrt() / th.sin()
        };
        let got = (amp(32_000) / amp(8_000)).ln();
        let want = 24_000.0 * z.norm().ln();
        assert!(
            (got - want).abs() < 0.01 * want.abs(),
            "res {res}: {got} against {want}"
        );
    }
}

/// The resonance is held at 1: `LadderDesign` reads `k = 4 min(res, 1)`, `k = 4` being where
/// the continuous poles reach the imaginary axis
/// (`the_ladder_self_oscillates_at_the_cutoff_at_k_four`).
#[test]
fn the_resonance_ceiling_is_k_four() {
    for res in [1.0f64, 1.5, 2.0, 1e30, f64::INFINITY] {
        assert_eq!(
            LadderDesign::new(Hz::new(1_000.0), Resonance::new(res), RATE).k,
            4.0,
            "{res}"
        );
    }
    assert!(LadderDesign::new(Hz::new(1_000.0), Resonance::new(0.999), RATE).k < 4.0);
}

/// Past the knee the loop is bounded whatever the controls. The saturator limits the loop
/// input `u` to `|u| <= 1`; a trapezoidal stage `G (1 + z^-1) / (1 - p z^-1)` with
/// `p = 1 - 2 G` has an impulse response of l1 norm 1 for `G <= 1/2` and `2 G < 2`
/// above, so the stage `n` output is at most `2^n`, and the taps `LP12`, `LP24`, `HP12` and
/// `HP24` at most 4, 16, 9 (`1 + 2 * 2 + 4`) and 81 (`1 + 4 * 2 + 6 * 4 + 4 * 8 + 16`, the
/// binomial weights).
/// A loud noise through the highest drive, the highest resonance and the extremes of the
/// cutoff stays under them and is finite.
#[test]
fn the_output_is_bounded_by_the_saturator_and_the_stages_l1_norm() {
    let bound = |ty| match ty {
        LadderType::LP12 => 4.0,
        LadderType::HP12 => 9.0,
        LadderType::LP24 => 16.0,
        _ => 81.0,
    };
    let noise: Vec<f64> = (0..20_000)
        .map(|i| f64::from((i * 7919) % 2003) / 1001.0 - 1.0)
        .collect();
    for ty in TYPES {
        for fc in [1.0f64, 100.0, 12_000.0, 23_900.0] {
            for (res, drive) in [(1.0f64, 1e3f64), (1.0, 1.0), (0.5, 1e6)] {
                let mut bank = LadderBank::<f64>::new(ChannelLayout::MONO);
                let mut ramp = Ramp::new(design(ty, fc, res, drive));
                let y = run(&mut bank, &mut ramp, std::slice::from_ref(&noise), &[64]).remove(0);
                let peak = y.iter().fold(0.0f64, |p, &v| p.max(v.abs()));
                assert!(
                    peak.is_finite() && peak <= bound(ty),
                    "{ty:?} {fc} Hz res {res} drive {drive}: {peak}"
                );
            }
        }
    }
}

/// Block independence: a ramp in flight, rendered in uneven blocks, gives the same frames
/// bit for bit as one block, because frame `i` reads step `taken + 1 + i` however the frames
/// are cut.
#[test]
fn any_split_renders_the_frames_of_one_block() {
    let input: Vec<Vec<f64>> = (0..3)
        .map(|c| {
            (0..997)
                .map(|i| ((i * 7 + c * 13) % 101) as f64 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |blocks: &[usize]| {
        let mut bank = LadderBank::<f64>::new(ChannelLayout::from(3u16));
        let mut ramp = Ramp::new(design::<f64>(LadderType::LP24, 300.0, 0.3, 1.0)).steps(16);
        ramp.retarget(design(LadderType::LP24, 6_000.0, 0.8, 4.0));
        run(&mut bank, &mut ramp, &input, blocks)
    };
    let whole = render(&[997]);
    for blocks in [
        &[1usize][..],
        &[5, 3, 64],
        &[15, 1, 1, 200],
        &[16],
        &[7, 33],
    ] {
        assert_eq!(render(blocks), whole, "blocks {blocks:?}");
    }
}

/// A ramp is the linear blend of the two designs, coefficients and drive alike, arriving
/// exactly on the new design at its last frame: an impulse at frame `i` into a bank with no
/// state comes out as one `nonlinear_ladder_step` at the set `(i + 1) / 16` of the way from the old
/// to the new, written out here (`g1` and `k` linearly, the drive `1 -> 4`), and at frame
/// 15 as the held render of the new design gives it, bit for bit.
#[test]
fn a_ramp_is_linear_and_arrives_on_the_new_design() {
    let (old, new) = (
        design::<f64>(LadderType::HP12, 500.0, 0.3, 1.0),
        design::<f64>(LadderType::HP12, 5_000.0, 0.3, 4.0),
    );
    for i in [0usize, 7, 15] {
        let mut ramp = Ramp::new(old).steps(16);
        ramp.retarget(new);
        let mut x = vec![0.0f64; 16];
        x[i] = 0.5;
        let y = run(
            &mut LadderBank::<f64>::new(ChannelLayout::MONO),
            &mut ramp,
            &[x],
            &[16],
        )
        .remove(0);
        let t = (i + 1) as f64 / 16.0;
        let blend = |a: f64, b: f64| a + (b - a) * t;
        let c = LadderCoefficients {
            g1: blend(old.coefficients.g1, new.coefficients.g1),
            k: blend(old.coefficients.k, new.coefficients.k),
        };
        let want =
            nonlinear_ladder_step(&c, LadderType::HP12, &mut [0.0; 4], 0.5 * blend(1.0, 4.0));
        assert!(
            (y[i] - want).abs() <= 1e-13 * want.abs(),
            "frame {i}: {} against {want}",
            y[i]
        );
    }
    let mut ramp = Ramp::new(old).steps(16);
    ramp.retarget(new);
    let mut x = vec![0.0f64; 16];
    x[15] = 0.5;
    let y = run(
        &mut LadderBank::<f64>::new(ChannelLayout::MONO),
        &mut ramp,
        &[x.clone()],
        &[16],
    )
    .remove(0);
    let mut held = Ramp::new(new);
    let z = run(
        &mut LadderBank::<f64>::new(ChannelLayout::MONO),
        &mut held,
        &[x],
        &[16],
    )
    .remove(0);
    assert_eq!(y[15], z[15]);
    assert_ne!(y[15], 0.0);
    assert!(!ramp.moving());
}

/// A retarget in the middle of a ramp starts from the design in force there, not the one it
/// was heading for: 8 frames into a ramp from 500 Hz to 5 kHz (the half-way `g1`, `k` being
/// equal at both ends), a `retarget` back to 500 Hz makes the first frame after it step 1 of 16
/// from the half-way design to the old one, written out here.
#[test]
fn a_retarget_in_flight_goes_on_from_where_the_ramp_was() {
    let (a, b) = (
        design::<f64>(LadderType::HP12, 500.0, 0.3, 1.0),
        design::<f64>(LadderType::HP12, 5_000.0, 0.3, 1.0),
    );
    let mut ramp = Ramp::new(a).steps(16);
    ramp.retarget(b);
    let mut bank = LadderBank::<f64>::new(ChannelLayout::MONO);
    run(&mut bank, &mut ramp, &[vec![0.0f64; 8]], &[8]);
    ramp.retarget(a);
    let got = run(&mut bank, &mut ramp, &[vec![0.5f64]], &[1])[0][0];
    let mid = 0.5 * (a.coefficients.g1 + b.coefficients.g1);
    let c = LadderCoefficients {
        g1: mid + (a.coefficients.g1 - mid) / 16.0,
        k: a.coefficients.k,
    };
    let want = nonlinear_ladder_step(&c, LadderType::HP12, &mut [0.0; 4], 0.5);
    assert!(
        (got - want).abs() <= 1e-13 * want.abs(),
        "{got} against {want}"
    );
}

/// A blend keeps the target's tap: a ramp between two types reads the new type from the
/// first frame, so the response is a tap and never a mix of two.
#[test]
fn a_ramp_changes_the_tap_at_once() {
    let (lp, hp) = (
        design::<f64>(LadderType::LP24, 1_000.0, 0.0, 1.0),
        design::<f64>(LadderType::HP24, 1_000.0, 0.0, 1.0),
    );
    let mut ramp = Ramp::new(lp).steps(16);
    ramp.retarget(hp);
    let blended = ramp.at(8);
    assert_eq!(blended.ty, LadderType::HP24);
}

/// Channels are independent: a bank of any width gives each channel exactly what a
/// one-channel bank gives it, held or ramping, whatever group the width falls into.
#[test]
fn each_channel_is_its_own_filter() {
    for width in [1u16, 2, 5, 8, 9, 16] {
        let input: Vec<Vec<f64>> = (0..width)
            .map(|c| {
                (0..300)
                    .map(|i| (((i + 1) * (c as usize + 3)) % 37) as f64 / 18.0 - 1.0)
                    .collect()
            })
            .collect();
        let ramp = || {
            let mut g = Ramp::new(design::<f64>(LadderType::LP24, 800.0, 0.4, 1.0)).steps(16);
            g.retarget(design(LadderType::LP24, 2_000.0, 0.7, 3.0));
            g
        };
        let mut bank = LadderBank::<f64>::new(ChannelLayout::from(width));
        let all = run(&mut bank, &mut ramp(), &input, &[300]);
        for (c, ch) in input.iter().enumerate() {
            let mut one = LadderBank::<f64>::new(ChannelLayout::MONO);
            let alone = run(&mut one, &mut ramp(), std::slice::from_ref(ch), &[300]).remove(0);
            assert_eq!(all[c], alone, "width {width}, channel {c}");
        }
    }
}

/// `reset` is silence: after it, a zero input gives exactly zero output.
#[test]
fn a_reset_bank_is_silent() {
    let mut bank = LadderBank::<f64>::new(ChannelLayout::STEREO);
    let mut ramp = Ramp::new(design(LadderType::LP24, 300.0, 0.9, 1.0));
    let noise: Vec<f64> = (0..512)
        .map(|i| ((i * 31) % 17) as f64 / 8.0 - 1.0)
        .collect();
    run(&mut bank, &mut ramp, &[noise.clone(), noise], &[512]);
    bank.reset();
    let out = run(
        &mut bank,
        &mut ramp,
        &[vec![0.0f64; 64], vec![0.0; 64]],
        &[64],
    );
    assert!(out.iter().flatten().all(|&s| s == 0.0));
}

/// A ramp's midpoint between two designs the small-signal model keeps stable is itself
/// stable: an impulse through the half-way design decays (the last thousand frames of
/// 12 000 peak below half of the first thousand's). The slowest pole in the swept range is
/// at 20 Hz with a resonance of 0.7 (`k = 2.8`): its real part is
/// `wc (1 - k^(1/4) / sqrt 2) = 0.085 wc`, a decay of `2.2e-4` a frame, so the last window
/// sits `e^-2.2` (0.11) of the first at the worst, under the one half asserted; a pole slower
/// than `6e-5` a frame (a blend leaving the stable set, whose radius reaches 1) fails. 200 pairs drawn from a seeded sweep of
/// reachable designs: every type, a cutoff from 20 Hz to 3 kHz, a resonance up to 0.7 and
/// a drive from 0.1 to 8. The reach stops where the linearisation has a pole outside the
/// circle (a high cutoff with a middling resonance, a resonance of 1); a ramp that
/// extrapolated, or a blend that left the stable set, fails.
#[test]
fn a_ramps_midpoint_is_stable() {
    const FRAMES: usize = 12_000;
    let mut seed = 24u32;
    let mut next = |lo: f64, hi: f64| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        lo + (hi - lo) * f64::from(seed >> 8) / f64::from(1u32 << 24)
    };
    fn reachable(next: &mut dyn FnMut(f64, f64) -> f64) -> LadderBlend<f64> {
        let ty = TYPES[next(0.0, 4.0) as usize % 4];
        design(ty, next(20.0, 3_000.0), next(0.0, 0.7), next(0.1, 8.0))
    }
    for _ in 0..200 {
        let (a, b) = (reachable(&mut next), reachable(&mut next));
        let mid = a.lerp(&b, 8, 16);
        let mut ramp = Ramp::new(mid);
        let mut bank = LadderBank::<f64>::new(ChannelLayout::MONO);
        let y = run(&mut bank, &mut ramp, &[impulse(1.0, FRAMES)], &[FRAMES]).remove(0);
        let peak = |r: core::ops::Range<usize>| y[r].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let (first, last) = (peak(0..1_000), peak(FRAMES - 1_000..FRAMES));
        assert!(
            first.is_finite() && last.is_finite() && last <= 0.5 * first,
            "{a:?} -> {b:?}: first {first}, last {last}"
        );
    }
}

/// A 5 Hz low-pass at 192 kHz, the ladder's range extreme, run in `f32` at a small signal.
///
/// Probe: a sine of amplitude `A` at the cutoff, so `W = 1` and the zero-resonance 24 dB
/// low-pass is `1 / (1 + j)^4 = -1/4` (each stage the bilinear image of `1 / (1 + s)`, prewarped
/// at the cutoff). The saturator `tanh` takes the input first and cuts the fundamental by `A^2 /
/// 4` of itself, `2.5e-7` at `A = 1e-3`, and rounding the stage gain `G = g / (1 + g)` to `f32`
/// moves each stage's corner by at most `2^-24` of itself, `2e-7` of the gain at the corner.
///
/// Noise: a stage's state takes about three roundings a step, each uniform within `2^-24` of
/// a value of at most `A`, `2^-24 A` together as a standard deviation. The state leaks at
/// `2 G` a frame, so an injection's variance sums to at most `1 / (4 G)` frames of itself in
/// its own stage and no more through the stages after it (each a low-pass of unit gain),
/// `1 / G` over the four: the output's deviation has standard deviation at most `2^-24 A / sqrt
/// G = 6.6e-6 A`. The test measures `N = 192 000` frames, whose expected maximum is `sqrt(2 ln N)
/// = 4.9` standard deviations; the bound is 1.5 times that, 7.4 of them, for the non-Gaussian
/// rounding tails and the correlated state noise.
#[test]
fn a_five_hz_low_pass_at_192_khz_holds_its_gain_and_noise_in_f32() {
    use crate::testing::{sine_fit, steady_deviation};
    const FS: f64 = 192_000.0;
    const FC: f64 = 5.0;
    const A: f64 = 1e-3;
    let whole = FS as usize;
    let controls = LadderControls {
        ty: LadderType::LP24,
        freq: Hz::new(FC),
        res: Resonance::new(0.0),
        drive: Drive::UNITY,
    };
    let w = 2.0 * PI * FC / FS;
    let input: Vec<f32> = (0..2 * whole)
        .map(|n| (A * libm::sin(w * n as f64)) as f32)
        .collect();
    let model = LadderModel::new(controls, SampleRate::of(FS));
    let mut bank = LadderBank::<f32>::new(ChannelLayout::MONO);
    let mut ramp = Ramp::new(LadderBlend::<f32>::from(&model));
    let y = run(&mut bank, &mut ramp, &[input], &[2 * whole]).remove(0);
    let y: Vec<f64> = y.iter().map(|&v| f64::from(v)).collect();

    let h = Complex64::new(-0.25 * A, 0.0);
    let stage_gain = libm::tan(PI * FC / FS);
    let sigma = 2f64.powi(-24) * A / stage_gain.sqrt();
    let bound = 1.5 * (2.0 * (whole as f64).ln()).sqrt() * sigma;
    let fit = sine_fit(&y, w, whole);
    assert!((fit - h).norm() < bound, "the fit {fit} against {h}");
    let worst = steady_deviation(&y, w, whole, h);
    assert!(worst < bound, "deviation {worst:e} over {bound:e}");
}
