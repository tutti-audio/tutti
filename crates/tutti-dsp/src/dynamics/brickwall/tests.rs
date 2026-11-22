use crate::testing::uniform;
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::Db;

use super::*;
use crate::testing::noise;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

/// The ceiling the `f32` clip runs at: the amplitude of `db`, narrowed once.
fn ceiling_of(db: f64) -> f32 {
    Db::new(db).to_amplitude().get() as f32
}

/// The ceiling a fed `f32` control signal value runs at: `10^(db / 20)` computed in `f32`, as the per-frame
/// read does.
fn fed_ceiling_of(db: f32) -> f32 {
    libm::powf(10.0, db / 20.0)
}

fn controls(db: f64) -> BrickwallControls {
    BrickwallControls {
        ceiling: Db::new(db),
    }
}

/// The ceilings the tests run at: a grid across the range and seeded draws.
fn ceilings() -> Vec<f64> {
    let mut all = vec![-96.0, -60.0, -24.0, -6.0, -0.3, 0.0, 3.0, 12.0];
    let mut rng = SplitMix64::new(24);
    all.extend((0..64).map(|_| uniform(&mut rng, -80.0, 20.0)));
    all
}

/// A bank set to `controls`, rendering `x` on both channels, held or with `signal`, in
/// blocks of `block`.
fn render_blocks(
    controls: BrickwallControls,
    x: &[f32],
    signal: Option<&[f32]>,
    block: usize,
) -> Vec<Vec<f32>> {
    let mut bank = BrickwallBank::<f32>::new();
    bank.retarget(&controls);
    let mut out = vec![vec![0.0f32; x.len()]; 2];
    let mut start = 0;
    while start < x.len() {
        let end = (start + block).min(x.len());
        let ins = [&x[start..end]; 2];
        let signals = BrickwallSignals {
            ceiling: signal.map(|l| Mono::new(&l[start..end])),
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[start..end]).collect();
        bank.process(
            signals,
            &mut InputOutput::paired(
                Planes::new(&ins).unwrap(),
                PlanesMut::new(&mut outs).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        start = end;
    }
    out
}

/// `xs` on one channel through a bank set to `db`.
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
    let mut rng = SplitMix64::new(8);
    let mut xs: Vec<f32> = vec![0.0, -0.0, 1.0, -1.0, 0.5, 1e-30, 1e30, f32::MAX, f32::MIN];
    xs.extend((0..200).map(|_| uniform(&mut rng, -4.0, 4.0)));
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

/// The render is the clamp of the definition at the set ceiling, whatever the block cut
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
/// `signal[i]` dB, a NaN frame at the set ceiling, and `+-inf` frames are
/// ceilings. Whatever the block cut.
#[test]
fn a_fed_ceiling_is_read_per_frame() {
    let x = noise(6, 1024, 2.0);
    let mut signal: Vec<f32> = (0..x.len()).map(|i| -((i % 40) as f32) * 0.75).collect();
    signal[3] = f32::NAN;
    signal[100] = f32::INFINITY;
    signal[101] = f32::NEG_INFINITY;
    let base = -6.0;
    let held = ceiling_of(base);
    for block in [1, 17, 64, 512] {
        for out in render_blocks(controls(base), &x, Some(&signal), block) {
            for (i, (&y, &x)) in out.iter().zip(&x).enumerate() {
                let want = if signal[i].is_nan() {
                    definition(x, held)
                } else {
                    definition(x, fed_ceiling_of(signal[i]))
                };
                assert_eq!(y, want, "block {block}, frame {i}, signal {}", signal[i]);
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

/// An `f64` clipper clamps in `f64`: over `-20 dB` a signal of more than 24 bits leaves at the
/// closed form `10^(-20 / 20)`, to 1e-15, and under `0 dB` it comes out as it went in.
#[test]
fn an_f64_brickwall_clamps_at_the_f64_amplitude_and_passes_what_is_inside() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(64);
    let render = |db: f64| -> Vec<f64> {
        let mut bank = BrickwallBank::<f64>::new();
        bank.retarget(&controls(db));
        let mut y = vec![0.0f64; x.len()];
        bank.process(
            BrickwallSignals::default(),
            &mut InputOutput::paired(
                Planes::new(&[&x[..]]).unwrap(),
                PlanesMut::new(&mut [&mut y[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        y
    };
    assert_close_f64(
        &render(-20.0),
        &vec![Db::new(-20.0).to_amplitude().get(); x.len()],
        1e-15,
    );
    assert_close_f64(&render(0.0), &x, 0.0);
}

/// A ceiling whose amplitude is past `f32`'s largest finite value (800 dB is `1e40`) passes
/// everything, an infinity included, as `+inf` dB does: it is not clamped to `f32::MAX`.
#[test]
fn an_f32_ceiling_of_800_db_passes_an_infinity_unchanged() {
    let x = [1.0e30f32, -3.0, 0.0, f32::INFINITY, f32::NEG_INFINITY];
    let mut bank = BrickwallBank::<f32>::new();
    bank.retarget(&controls(800.0));
    let mut y = [0.0f32; 5];
    bank.process(
        BrickwallSignals::default(),
        &mut InputOutput::paired(
            Planes::new(&[&x[..]]).unwrap(),
            PlanesMut::new(&mut [&mut y[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(y, x);
}
