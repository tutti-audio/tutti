use super::*;
use crate::pan_gains;
use crate::testing::noise;
use core::f64::consts::TAU;
use tutti_buffers::{BlockSide, InputOutput, Planes, PlanesMut};
use tutti_units::Pan;

const RATE: SampleRate = SampleRate::SR_48K;
/// 100 Hz at 48 kHz: 480 frames a cycle, the trough of a sine at frame 360.
const HZ: f64 = 100.0;
const PERIOD: usize = 480;

fn run<P>(frames: usize, blocks: &[usize], channels: &[Vec<f32>], mut f: P) -> Vec<Vec<f32>>
where
    P: FnMut(&mut InputOutput<'_, f32>) -> Result<(), BlockMismatch>,
{
    let mut out = vec![vec![0.0f32; frames]; channels.len()];
    let (mut at, mut k) = (0, 0);
    while at < frames {
        let end = (at + blocks[k % blocks.len()]).min(frames);
        k += 1;
        let ins: Vec<Vec<f32>> = channels.iter().map(|c| c[at..end].to_vec()).collect();
        let mut outs: Vec<Vec<f32>> = vec![vec![0.0; end - at]; channels.len()];
        let mut block = InputOutput::paired(
            Planes::from_vecs(&ins).unwrap(),
            PlanesMut::from_vecs(&mut outs).unwrap(),
        )
        .unwrap();
        f(&mut block).unwrap();
        for (o, part) in out.iter_mut().zip(&outs) {
            o[at..end].copy_from_slice(part);
        }
        at = end;
    }
    out
}

/// The tremolo gain `1 - (|d| - d u) / 2` of a sine wave at frame `n`.
fn sine_gain(d: f64, n: usize) -> f64 {
    let u = (TAU * HZ * n as f64 / RATE.get()).sin();
    1.0 - (d.abs() - d * u) / 2.0
}

fn tremolo(wave: OscillatorWave, rate: Hz, depth: f64) -> Tremolo {
    let mut t = Tremolo::new(wave, RATE);
    t.retarget(&TremoloControls {
        rate,
        depth: Depth::of(depth),
    });
    t
}

fn gains(depth: f64, frames: usize, blocks: &[usize]) -> Vec<f32> {
    let mut t = tremolo(OscillatorWave::Sine, Hz::new(HZ), depth);
    let ones = vec![vec![1.0f32; frames]];
    run(frames, blocks, &ones, |block| t.process(block)).remove(0)
}

/// Depth 0 is unity: the output is the input, bit for bit.
#[test]
fn depth_zero_is_unity() {
    let x = noise(1, 2_000, 1.0);
    let mut t = tremolo(OscillatorWave::Sine, Hz::new(HZ), 0.0);
    let out = run(2_000, &[2_000], std::slice::from_ref(&x), |block| {
        t.process(block)
    });
    assert_eq!(out[0], x);
}

/// Output is input times `1 - (|d| - d sin(2 pi f n / fs)) / 2` at each frame, for depth 1,
/// a mid depth 0.5 and the inverted depth -0.75.
#[test]
fn the_output_is_the_input_times_the_closed_form_gain() {
    let x = noise(2, 1_920, 1.0);
    for d in [1.0f64, 0.5, -0.75] {
        let mut t = tremolo(OscillatorWave::Sine, Hz::new(HZ), d);
        let out = run(1_920, &[1_920], std::slice::from_ref(&x), |block| {
            t.process(block)
        });
        for (n, (&y, &x)) in out[0].iter().zip(&x).enumerate() {
            let want = f64::from(x) * sine_gain(d, n);
            // The samples are `f32`, which sets the tolerance.
            assert!((f64::from(y) - want).abs() < 1e-5, "depth {d}, frame {n}");
        }
    }
}

/// At depth 1 the gain is 1 at the wave's peak (frame 120) and 0 at its trough (frame 360);
/// at depth -1 the other way round.
#[test]
fn depth_one_reaches_silence_at_the_trough() {
    let g = gains(1.0, PERIOD, &[PERIOD]);
    assert!((g[120] - 1.0).abs() < 1e-6 && g[360].abs() < 1e-6);
    let g = gains(-1.0, PERIOD, &[PERIOD]);
    assert!(g[120].abs() < 1e-6 && (g[360] - 1.0).abs() < 1e-6);
}

/// The gain repeats every `fs / rate` frames and not before: the troughs of two cycles are
/// 480 frames apart, and no frame between returns to the first trough's value.
#[test]
fn the_period_is_the_inverse_of_the_rate() {
    let g = gains(1.0, 3 * PERIOD, &[64]);
    for n in 0..2 * PERIOD {
        assert!((g[n] - g[n + PERIOD]).abs() < 1e-5, "frame {n}");
    }
    let trough = |lo: usize, hi: usize| {
        (lo..hi)
            .min_by(|&a, &b| g[a].partial_cmp(&g[b]).unwrap())
            .unwrap()
    };
    assert_eq!(trough(PERIOD, 2 * PERIOD) - trough(0, PERIOD), PERIOD);
}

/// The phase continues across blocks: blocks of 1, 7, 64 and 480 frames each give the closed
/// form at every frame of the stream.
#[test]
fn the_phase_continues_across_blocks() {
    for blocks in [&[1usize][..], &[7], &[64], &[480], &[3, 100, 17]] {
        let g = gains(0.8, 1_000, blocks);
        for (n, &g) in g.iter().enumerate() {
            assert!(
                (f64::from(g) - sine_gain(0.8, n)).abs() < 1e-5,
                "blocks {blocks:?}, frame {n}"
            );
        }
    }
}

/// Every channel of any width gets the same gain, and a rate of 0 holds the phase at 0 (the
/// wave 0: gain `1 - |d| / 2`).
#[test]
fn every_channel_takes_one_gain_and_rate_zero_holds() {
    let mut t = tremolo(OscillatorWave::Sine, Hz::new(HZ), 0.6);
    let ones = vec![vec![1.0f32; 100]; 3];
    let out = run(100, &[100], &ones, |block| t.process(block));
    for (c, channel) in out.iter().enumerate() {
        for (n, &y) in channel.iter().enumerate() {
            assert!((f64::from(y) - sine_gain(0.6, n)).abs() < 1e-6, "{c} {n}");
        }
    }
    let mut t = tremolo(OscillatorWave::Sine, Hz::ZERO, 0.6);
    let out = run(50, &[50], &ones[..1], |block| t.process(block));
    assert!(out[0].iter().all(|&y| (y - 0.7).abs() < 1e-6));
}

/// A square wave swings between the two ends of the gain: 1 over its high half-cycle and
/// `1 - d` over its low one (the frames clear of the edges).
#[test]
fn a_square_wave_switches_between_unity_and_one_minus_depth() {
    let mut t = tremolo(OscillatorWave::Square, Hz::new(HZ), 0.75);
    let ones = vec![vec![1.0f32; PERIOD]];
    let g = run(PERIOD, &[PERIOD], &ones, |block| t.process(block)).remove(0);
    assert!(g[10..230].iter().all(|&g| (g - 1.0).abs() < 1e-6));
    assert!(g[250..470].iter().all(|&g| (g - 0.25).abs() < 1e-6));
}

fn auto_pan(law: PanLaw, depth: f64) -> AutoPan {
    let mut p = AutoPan::new(OscillatorWave::Sine, law, RATE);
    p.retarget(&AutoPanControls {
        rate: Hz::new(HZ),
        depth: Depth::of(depth),
    });
    p
}

/// Left and right are the input times `pan_gains` at the position `d sin(2 pi f n / fs)`,
/// whatever the blocks, and the pair's power is 1 at every frame for both laws.
#[test]
fn auto_pan_places_by_pan_gains_at_the_lfo_position() {
    let ones = vec![vec![1.0f32; 960]; 2];
    for law in [PanLaw::SquareRoot, PanLaw::Rp036] {
        for d in [1.0f64, 0.4, -0.7] {
            let mut p = auto_pan(law, d);
            let out = run(960, &[37, 200], &ones, |block| p.process(block));
            for (n, (&out_l, &out_r)) in out[0].iter().zip(&out[1]).enumerate() {
                let u = (TAU * HZ * n as f64 / RATE.get()).sin();
                let (l, r) = pan_gains(Pan::new(d * u), law);
                assert!(
                    (f64::from(out_l) - l.get()).abs() < 1e-5,
                    "{law:?} {d} L {n}"
                );
                assert!(
                    (f64::from(out_r) - r.get()).abs() < 1e-5,
                    "{law:?} {d} R {n}"
                );
                let power = out_l * out_l + out_r * out_r;
                assert!((power - 1.0).abs() < 1e-5, "{law:?} {d} power {n}");
            }
        }
    }
}

/// At depth 1 the source is hard right at the wave's peak and hard left at its trough
/// ([`PanLaw::Rp036`] is exact at the ends); at depth 0 it stays at the centre, `sqrt(1/2)`
/// each side.
#[test]
fn auto_pan_sweeps_the_whole_field_at_full_depth_and_none_at_zero() {
    let ones = vec![vec![1.0f32; PERIOD]; 2];
    let mut p = auto_pan(PanLaw::Rp036, 1.0);
    let out = run(PERIOD, &[PERIOD], &ones, |block| p.process(block));
    assert!(out[0][120].abs() < 1e-6 && (out[1][120] - 1.0).abs() < 1e-6);
    assert!((out[0][360] - 1.0).abs() < 1e-6 && out[1][360].abs() < 1e-6);
    let mut p = auto_pan(PanLaw::SquareRoot, 0.0);
    let out = run(PERIOD, &[PERIOD], &ones, |block| p.process(block));
    let half = 0.5f32.sqrt();
    assert!(out.iter().flatten().all(|&y| (y - half).abs() < 1e-6));
}

/// A view of one channel is refused, as a pair is two channels: an output prefilled with a
/// sentinel keeps it.
#[test]
fn auto_pan_refuses_a_mono_view_untouched() {
    let mut p = auto_pan(PanLaw::SquareRoot, 1.0);
    let input = vec![vec![1.0f32; 10]];
    let mut output = vec![vec![7.0f32; 10]];
    let refused = p.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut output).unwrap(),
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
    assert!(output[0].iter().all(|&y| y == 7.0));
}

/// At `F = f64` the LFO, the gain and the pan law all run in `f64`: a tremolo's gain is the
/// closed form `1 - (|d| - d u) / 2` and an auto-pan's pair is `pan_gains` at `d u`, each to
/// 1e-12 (the `f64` phase is exact to 2^-53 cycles and the sine's argument rounds at 1e-15),
/// where an `f32` LFO is off by 1e-7.
#[test]
fn an_f64_tremolo_and_auto_pan_are_their_closed_forms_in_f64() {
    let (frames, d) = (960usize, 0.7f64);
    let ones = vec![1.0f64; frames];
    let mut t = tremolo(OscillatorWave::Sine, Hz::new(HZ), d);
    let mut y = vec![0.0f64; frames];
    t.process(
        &mut InputOutput::paired(
            Planes::new(&[&ones[..]]).unwrap(),
            PlanesMut::new(&mut [&mut y[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for (n, &y) in y.iter().enumerate() {
        assert!((y - sine_gain(d, n)).abs() < 1e-12, "tremolo frame {n}");
    }
    for law in [PanLaw::SquareRoot, PanLaw::Rp036] {
        let mut p = auto_pan(law, d);
        let (mut l, mut r) = (vec![0.0f64; frames], vec![0.0f64; frames]);
        p.process(
            &mut InputOutput::paired(
                Planes::new(&[&ones[..], &ones[..]]).unwrap(),
                PlanesMut::new(&mut [&mut l[..], &mut r[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        for n in 0..frames {
            let u = (TAU * HZ * n as f64 / RATE.get()).sin();
            let (gl, gr) = pan_gains(Pan::new(d * u), law);
            assert!((l[n] - gl.get()).abs() < 1e-12, "{law:?} left {n}");
            assert!((r[n] - gr.get()).abs() < 1e-12, "{law:?} right {n}");
        }
    }
}
