use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_units::Db;

use super::*;
use crate::testing::noise;

/// The ceiling the `f32` clip runs at: the amplitude of `db`, narrowed once.
fn ceiling_of(db: f64) -> f32 {
    Db(db).to_amplitude().get() as f32
}

fn controls(db: f64) -> BrickwallControls {
    BrickwallControls { ceiling: Db(db) }
}

/// The ceilings the tests run at: a grid across the range and seeded draws.
fn ceilings() -> Vec<f64> {
    let mut all = vec![-96.0, -60.0, -24.0, -6.0, -0.3, 0.0, 3.0, 12.0];
    let mut rng = StdRng::seed_from_u64(24);
    all.extend((0..64).map(|_| rng.random_range(-80.0..20.0)));
    all
}

/// A bank aimed at `controls`, rendering `x` on both channels, held or with `lane`, in
/// blocks of `block`.
fn render_blocks(
    controls: BrickwallControls,
    x: &[f32],
    lane: Option<&[f32]>,
    block: usize,
) -> Vec<Vec<f32>> {
    let mut bank = BrickwallBank::new();
    bank.aim(&controls);
    let mut out = vec![vec![0.0f32; x.len()]; 2];
    let mut start = 0;
    while start < x.len() {
        let end = (start + block).min(x.len());
        let ins = [&x[start..end]; 2];
        let lanes = BrickwallLanes {
            ceiling: lane.map(|l| Mono::new(&l[start..end])),
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        bank.process(
            lanes,
            Planes::new(&ins).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        );
        start = end;
    }
    out
}

/// `xs` on one channel through a bank aimed at `db`.
fn through(db: f64, xs: &[f32]) -> Vec<f32> {
    render_blocks(controls(db), xs, None, 64).remove(0)
}

/// The ceiling of `db` from its definition, `10^(db/20)`.
fn amplitude_of(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// `y = clamp(x, -c, c)`, from the definition.
fn definition(x: f32, c: f32) -> f32 {
    if x > c {
        c
    } else if x < -c {
        -c
    } else {
        x
    }
}

/// The ceiling is the amplitude of the controls' dB: a sample far over it comes out at
/// `10^(db/20)` (to `f32`), `+inf` dB leaves it as it is, `-inf` dB mutes, and a NaN ceiling
/// is the default, 0 dB.
#[test]
fn the_ceiling_is_the_amplitude_of_the_decibels() {
    for db in [-96.0f64, -24.0, -6.0, -0.3, 0.0, 3.0, 12.0] {
        let c = f64::from(through(db, &[1e30])[0]);
        assert!(
            (c / amplitude_of(db) - 1.0).abs() < 1e-6,
            "{db} dB: {c} against {}",
            amplitude_of(db)
        );
    }
    assert_eq!(through(f64::INFINITY, &[1e30])[0], 1e30);
    assert_eq!(through(f64::NEG_INFINITY, &[1e30])[0], 0.0);
    assert_eq!(
        through(f64::NAN, &[2.0])[0],
        1.0,
        "a NaN ceiling is the default, 0 dB"
    );
}

/// The definition: `y = clamp(x, -c, c)` over a ramp, a grid and seeded draws, and the four
/// things that follow from it, each stated on its own: the output never passes the ceiling
/// (`|y| <= c`), the clip is odd (`f(-x) = -f(x)`), idempotent (`f(f(x)) = f(x)`) and the
/// identity inside the ceiling.
#[test]
fn the_bank_is_the_clamp_and_its_four_consequences() {
    let mut rng = StdRng::seed_from_u64(8);
    let mut xs: Vec<f32> = vec![0.0, -0.0, 1.0, -1.0, 0.5, 1e-30, 1e30, f32::MAX, f32::MIN];
    xs.extend((0..200).map(|_| rng.random_range(-4.0..4.0)));
    xs.extend((-400..=400).map(|i| i as f32 * 0.01));
    let negated: Vec<f32> = xs.iter().map(|x| -x).collect();
    for db in ceilings() {
        let ceiling = ceiling_of(db);
        let y = through(db, &xs);
        let again = through(db, &y);
        let odd = through(db, &negated);
        for (i, &x) in xs.iter().enumerate() {
            assert_eq!(y[i], definition(x, ceiling), "{db} dB at {x}");
            assert!(
                y[i].abs() <= ceiling,
                "{db} dB at {x}: {} past the ceiling",
                y[i]
            );
            assert_eq!(odd[i], -y[i], "{db} dB is not odd at {x}");
            assert_eq!(again[i], y[i], "{db} dB is not idempotent at {x}");
            if x.abs() <= ceiling {
                assert_eq!(y[i], x, "{db} dB changed {x}, inside the ceiling");
            }
        }
    }
}

/// `+inf` dB passes everything (a finite sample, an infinite one) and `-inf` mutes.
#[test]
fn infinite_ceilings_pass_everything_and_mute() {
    let xs = [1e30f32, -3.0, 0.0, f32::INFINITY, f32::NEG_INFINITY];
    assert_eq!(through(f64::INFINITY, &xs), xs);
    assert!(
        through(f64::NEG_INFINITY, &xs)
            .iter()
            .all(|y| y.abs() == 0.0)
    );
}

/// The render is the clamp of the definition at the aimed ceiling, whatever the block cut
/// (1, 17, 64, 512 frames) and on every channel.
#[test]
fn the_render_is_the_clamp_at_any_block() {
    let x = noise(5, 2048, 2.0);
    for db in [-18.0f64, -6.0, 0.0, 6.0] {
        let c = ceiling_of(db);
        for block in [1, 17, 64, 512] {
            for (ch, out) in render_blocks(controls(db), &x, None, block)
                .iter()
                .enumerate()
            {
                for (i, (&y, &x)) in out.iter().zip(&x).enumerate() {
                    assert_eq!(
                        y,
                        definition(x, c),
                        "{db} dB, block {block}, ch {ch}, frame {i}"
                    );
                }
            }
        }
    }
}

/// A fed ceiling is read **per frame**: frame `i` is clamped at the amplitude of
/// `lane[i]` dB, a NaN frame at the aimed ceiling, and `+-inf` frames are
/// ceilings. Whatever the block cut.
#[test]
fn a_fed_ceiling_is_read_per_frame() {
    let x = noise(6, 1024, 2.0);
    let mut lane: Vec<f32> = (0..x.len()).map(|i| -((i % 40) as f32) * 0.75).collect();
    lane[3] = f32::NAN;
    lane[100] = f32::INFINITY;
    lane[101] = f32::NEG_INFINITY;
    let base = -6.0;
    let held = ceiling_of(base);
    for block in [1, 17, 64, 512] {
        for out in render_blocks(controls(base), &x, Some(&lane), block) {
            for (i, (&y, &x)) in out.iter().zip(&x).enumerate() {
                let want = if lane[i].is_nan() {
                    definition(x, held)
                } else {
                    definition(x, ceiling_of(f64::from(lane[i])))
                };
                assert_eq!(y, want, "block {block}, frame {i}, lane {}", lane[i]);
            }
        }
    }
}

/// The output never passes the ceiling, for any input: a full-scale square (every sample
/// at the rails), an impulse train, a +-1e30 burst and infinities, at ceilings from -60 to
/// 0 dB. The ceiling is the **sample** peak: the clamp is memoryless, so what lies between
/// two samples is not bounded.
#[test]
fn the_output_never_passes_the_ceiling_for_a_square_an_impulse_train_or_a_burst() {
    let n = 4096;
    let square: Vec<f32> = (0..n)
        .map(|i| if (i / 16) % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let train: Vec<f32> = (0..n)
        .map(|i| if i % 100 == 0 { 16.0 } else { 0.0 })
        .collect();
    let burst: Vec<f32> = (0..n)
        .map(|i| match i % 4 {
            0 => 1e30,
            1 => -1e30,
            2 => f32::INFINITY,
            _ => f32::NEG_INFINITY,
        })
        .collect();
    for db in [-60.0f64, -18.0, -6.0, -0.3, 0.0] {
        let ceiling = ceiling_of(db);
        for (name, x) in [("square", &square), ("train", &train), ("burst", &burst)] {
            for out in render_blocks(controls(db), x, None, 64) {
                let peak = out.iter().fold(0.0f32, |m, y| m.max(y.abs()));
                assert!(
                    peak <= ceiling,
                    "{name} at {db} dB: peak {peak} over {ceiling}"
                );
            }
        }
        // A full-scale square above the ceiling lands on it exactly, so the bound is met.
        let out = render_blocks(controls(db), &square, None, 64);
        let peak = out[0].iter().fold(0.0f32, |m, y| m.max(y.abs()));
        assert_eq!(
            peak,
            ceiling.min(1.0),
            "the square reaches the ceiling at {db} dB"
        );
    }
}

/// The input is converted at the edges: the clamp over `f64` views is the clamp of the
/// `f32` the bank runs in.
#[test]
fn an_f64_view_is_clamped_like_the_f32_it_converts_to() {
    let x32 = noise(11, 512, 2.0);
    let x64: Vec<f64> = x32.iter().map(|&x| f64::from(x)).collect();
    let mut bank = BrickwallBank::new();
    bank.aim(&controls(-6.0));
    let mut y64 = vec![0.0f64; x64.len()];
    bank.process(
        BrickwallLanes::default(),
        Planes::new(&[&x64[..]]).unwrap(),
        PlanesMut::new(&mut [&mut y64[..]]).unwrap(),
    );
    let c = ceiling_of(-6.0);
    for (i, (&y, &x)) in y64.iter().zip(&x32).enumerate() {
        assert_eq!(y, f64::from(definition(x, c)), "frame {i}");
    }
}
