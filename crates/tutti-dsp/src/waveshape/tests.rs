use super::*;
use crate::testing::noise;
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_fixtures::signal::amplitude_at;
use tutti_units::Hz;
use tutti_units::SampleRate;

/// The curves, pinned.
///
/// The formulas follow the common waveshaper definitions; these values
/// pin them.
///
/// The four curves built from `+ * / round floor clamp abs` are pinned
/// **exactly**: IEEE-754 rounds each of those correctly, so the result is
/// the same on every platform. `tanh` and `atan` are libm
/// quality-of-implementation and differ in the last ulp between C runtimes, so
/// those two are pinned against the f64 closed form with a tolerance instead.
#[test]
fn curves_are_pinned() {
    let exact = [
        (ShapeKind::HardClip, 0.75, 2.0, 1.0),
        (ShapeKind::HardClip, -0.2, 2.0, -0.4),
        (ShapeKind::HardClip, -3.0, 1.0, -1.0),
        (ShapeKind::Softsign, 1.0, 1.0, 0.5),
        (ShapeKind::Softsign, -3.0, 1.0, -0.75),
        (ShapeKind::Crush, 0.3, 4.0, 0.25),
        // Drive below one level per unit floors to one.
        (ShapeKind::Crush, 0.6, 0.5, 1.0),
        (ShapeKind::Crush, 0.3, 0.5, 0.0),
        // SoftCrush passes through each step's midpoint and each integer.
        (ShapeKind::SoftCrush, 1.5, 1.0, 1.5),
        (ShapeKind::SoftCrush, 1.0, 1.0, 1.0),
        (ShapeKind::SoftCrush, 0.25, 2.0, 0.25),
    ];
    for (kind, x, drive, want) in exact {
        let got = kind.apply::<f32>(x, Drive::new(drive));
        assert_eq!(
            got.to_bits(),
            f32::to_bits(want),
            "{kind:?}({x}, drive {drive}) = {got}, want exactly {want}"
        );
    }

    // Off the midpoint the S-curve is a rational: smooth9(1/4) is
    // 12826 / 262144 exactly, so at hardness 1 a quarter step lands there.
    let quarter = ShapeKind::SoftCrush.apply::<f32>(0.25, Drive::new(1.0));
    assert!((f64::from(quarter) - 12826.0 / 262144.0).abs() < 1e-7);

    let half_pi = std::f64::consts::FRAC_PI_2;
    let closed = [
        (ShapeKind::Tanh, 0.5f32, 2.0f64, 1.0f64.tanh()),
        (ShapeKind::Tanh, -1.0, 3.0, (-3.0f64).tanh()),
        (ShapeKind::Atan, 1.0, 1.0, half_pi.atan() / half_pi),
        (
            ShapeKind::Atan,
            -0.5,
            4.0,
            (-half_pi * 2.0).atan() / half_pi,
        ),
    ];
    for (kind, x, drive, want) in closed {
        let got = f64::from(kind.apply(x, Drive::new(drive)));
        assert!(
            (got - want).abs() < 1e-6,
            "{kind:?}({x}, drive {drive}) = {got}, want {want}"
        );
    }
}

const KINDS: [ShapeKind; 7] = [
    ShapeKind::Tanh,
    ShapeKind::Atan,
    ShapeKind::Softsign,
    ShapeKind::HardClip,
    ShapeKind::TriangleFold,
    ShapeKind::Crush,
    ShapeKind::SoftCrush,
];

/// The most an `f32` render may differ from the `f64` closed form: the soft staircase's
/// degree-nine polynomial loses a few `f32` ulps of its range.
const TOL: f64 = 1e-4;

/// The curve's closed form in `f64`, at `drive` (the staircases' levels floored at 1).
fn closed(kind: ShapeKind, x: f32, drive: f64) -> f64 {
    closed_f64(kind, f64::from(x), drive)
}

/// [`closed`] at an `f64` input.
fn closed_f64(kind: ShapeKind, x: f64, d: f64) -> f64 {
    let half_pi = std::f64::consts::FRAC_PI_2;
    match kind {
        ShapeKind::Tanh => (x * d).tanh(),
        ShapeKind::Atan => (x * d * half_pi).atan() / half_pi,
        ShapeKind::Softsign => x * d / (1.0 + (x * d).abs()),
        ShapeKind::HardClip => (x * d).clamp(-1.0, 1.0),
        ShapeKind::TriangleFold => {
            2.0 / std::f64::consts::PI * (std::f64::consts::FRAC_PI_2 * x * d).sin().asin()
        }
        ShapeKind::Crush => (x * d).round() / d.max(1.0),
        ShapeKind::SoftCrush => {
            let (d, y) = (d.max(1.0), x * d.max(1.0));
            let t = y - y.floor();
            let smooth = t.powi(5)
                * (126.0 - 420.0 * t + 540.0 * t * t - 315.0 * t.powi(3) + 70.0 * t.powi(4));
            (y.floor() + smooth) / d
        }
    }
}

const RATE: SampleRate = SampleRate::SR_48K;

// The block tests below run an `F = f32` bank on `f32` views, the session's audio format, so
// their tolerances are `f32`'s; the `f64` tests at the end run the bank at `F = f64`.

/// A bank of `kind` at rest on `drive`.
fn at_rest(kind: ShapeKind, drive: f64) -> WaveshaperBank<f32> {
    let mut bank = WaveshaperBank::<f32>::new(kind, RATE);
    bank.retarget(&WaveshaperControls {
        drive: Drive::new(drive),
    });
    bank.jump_to_target();
    bank
}

/// `bank` over `channels` (each a plane), one block, with `signal` the drive per frame.
fn run(
    bank: &mut WaveshaperBank<f32>,
    signal: Option<&[f32]>,
    channels: &[Vec<f32>],
) -> Vec<Vec<f32>> {
    let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
    let mut out = vec![vec![0.0f32; channels[0].len()]; channels.len()];
    let planes = Planes::new(&refs).unwrap();
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    bank.process(
        signal.map(Mono::new),
        &mut InputOutput::paired(planes, PlanesMut::new(&mut outs).unwrap()).unwrap(),
    )
    .unwrap();
    out
}

/// A block through an at-rest bank is the curve of each sample at the drive: the loop for
/// each kind picks the curve it names and reads its own channel at its own frame.
#[test]
fn a_held_block_is_the_curve_of_each_sample() {
    for kind in KINDS {
        let mut bank = at_rest(kind, 2.5);
        let ins: Vec<Vec<f32>> = (0..3).map(|c| noise(c + 7, 64, 1.0)).collect();
        for (c, (y, x)) in run(&mut bank, None, &ins).iter().zip(&ins).enumerate() {
            for (i, (&y, &x)) in y.iter().zip(x).enumerate() {
                let want = closed(kind, x, 2.5);
                assert!(
                    (f64::from(y) - want).abs() < TOL,
                    "{kind:?} ch{c} frame {i}: {y} vs {want}"
                );
            }
        }
    }
}

/// With a control signal that moves every frame, every channel's sample is the curve at that frame's
/// drive, across the chunk boundary.
#[test]
fn a_signal_is_read_per_frame_on_every_channel() {
    let signal: Vec<f32> = (0..150).map(|i| 1.0 + 0.125 * i as f32).collect();
    for kind in KINDS {
        let mut bank = at_rest(kind, 1.0);
        let ins: Vec<Vec<f32>> = (0..3).map(|c| noise(c + 11, 150, 1.0)).collect();
        for (c, (y, x)) in run(&mut bank, Some(&signal), &ins)
            .iter()
            .zip(&ins)
            .enumerate()
        {
            for (i, (&y, &x)) in y.iter().zip(x).enumerate() {
                let want = closed(kind, x, f64::from(signal[i]));
                assert!(
                    (f64::from(y) - want).abs() < TOL,
                    "{kind:?} ch{c} frame {i}: {y} vs {want}"
                );
            }
        }
    }
}

/// A control signal frame that is not finite reads the drive in force, a finite one its own.
#[test]
fn a_non_finite_signal_frame_reads_the_drive_in_force() {
    let mut bank = at_rest(ShapeKind::HardClip, 2.0);
    let signal = [f32::NAN, 4.0, f32::INFINITY, f32::NEG_INFINITY];
    let out = run(&mut bank, Some(&signal), &[vec![0.25; 4]]);
    assert_eq!(out[0], [0.5, 1.0, 0.5, 0.5]);
}

/// A drive set past the held one ramps over `GAIN_RAMP`: frame `i` of a hard clip of a
/// constant is the line `1 + (3 - 1) * (i + 1) / n`, in whole blocks or in pieces, and the
/// block after it is on the target.
#[test]
fn a_drive_step_ramps_linearly_over_the_ramp() {
    let n = GAIN_RAMP.to_samples_ceil(RATE).get();
    let x = 0.2f32;
    for block in [n + 40, 7] {
        let mut bank = at_rest(ShapeKind::HardClip, 1.0);
        bank.retarget(&WaveshaperControls {
            drive: Drive::new(3.0),
        });
        let mut out = Vec::new();
        while out.len() < n + 40 {
            out.extend(run(&mut bank, None, &[vec![x; block]]).remove(0));
        }
        for (i, &y) in out.iter().enumerate().take(n) {
            let drive = 1.0 + 2.0 * (i + 1) as f64 / n as f64;
            assert!(
                (f64::from(y) - f64::from(x) * drive).abs() < 1e-6,
                "block {block}, frame {i}: {y}"
            );
        }
        assert!(
            out[n..].iter().all(|&y| y.to_bits() == (x * 3.0).to_bits()),
            "block {block}: on the target"
        );
    }
}

/// Each channel is shaped by its own samples alone.
#[test]
fn channels_are_independent() {
    let mut bank = at_rest(ShapeKind::HardClip, 1.0);
    let out = run(&mut bank, None, &[vec![4.0], vec![0.5]]);
    assert_eq!((out[0][0], out[1][0]), (1.0, 0.5));
}

/// `jump_to_target` lands the drive on its target at once: the block after is the held curve.
#[test]
fn settle_lands_on_the_target() {
    let mut bank = at_rest(ShapeKind::Softsign, 1.0);
    bank.retarget(&WaveshaperControls {
        drive: Drive::new(3.0),
    });
    bank.jump_to_target();
    let out = run(&mut bank, None, &[vec![1.0]]);
    assert_eq!(out[0][0], 0.75);
}

/// A non-finite control signal frame in the middle of a ramp reads the ramp's value at that frame:
/// frame `i` of a hard clip of `0.2` is `0.2 (1 + 2 (i + 1) / n)`, and a finite frame in
/// the control signal (frame 5, drive 2) reads its own.
#[test]
fn a_non_finite_signal_frame_mid_ramp_reads_the_ramp() {
    let n = GAIN_RAMP.to_samples_ceil(RATE).get();
    let mut bank = at_rest(ShapeKind::HardClip, 1.0);
    bank.retarget(&WaveshaperControls {
        drive: Drive::new(3.0),
    });
    let mut signal = vec![f32::NAN; 20];
    signal[5] = 2.0;
    let out = run(&mut bank, Some(&signal), &[vec![0.2; 20]]).remove(0);
    for (i, &y) in out.iter().enumerate() {
        let drive = if i == 5 {
            2.0
        } else {
            1.0 + 2.0 * (i + 1) as f64 / n as f64
        };
        assert!((f64::from(y) - 0.2 * drive).abs() < 1e-6, "frame {i}: {y}");
    }
}

/// Drive below 0 shapes as 0 and a staircase's below 1 as 1, on the control signal and on the ramp.
#[test]
fn the_drive_floors_hold_on_the_signal_and_the_ramp() {
    // Control signal: a hard clip at -2 is drive 0 (silence); a crush at 0.5 is one level per unit.
    let mut clip = at_rest(ShapeKind::HardClip, 1.0);
    let out = run(&mut clip, Some(&[-2.0; 4]), &[vec![0.25; 4]]);
    assert_eq!(out[0], [0.0; 4]);
    let mut crush = at_rest(ShapeKind::Crush, 4.0);
    let out = run(&mut crush, Some(&[0.5, 0.0, -1.0, 0.5]), &[vec![0.6; 4]]);
    assert_eq!(out[0], [1.0; 4]);
    // Hardness 1 puts a quarter on the soft step's rational 12826 / 262144; hardness 4 would
    // put it on 0.25.
    let mut soft = at_rest(ShapeKind::SoftCrush, 4.0);
    let out = run(&mut soft, Some(&[0.5, 0.0, -1.0, 0.5]), &[vec![0.25; 4]]);
    for y in &out[0] {
        assert!((f64::from(*y) - 12826.0 / 262144.0).abs() < 1e-6, "{y}");
    }
    // Ramp: a drive set below the floor never reads below it, on the way or on arrival.
    let mut clip = at_rest(ShapeKind::HardClip, 1.0);
    clip.retarget(&WaveshaperControls {
        drive: Drive::new(-2.0),
    });
    let n = GAIN_RAMP.to_samples_ceil(RATE).get();
    let out = run(&mut clip, None, &[vec![0.25; n + 10]]).remove(0);
    for (i, &y) in out.iter().enumerate() {
        // The line 1 -> -2, floored at 0.
        let drive = (1.0 - 3.0 * (i + 1) as f64 / n as f64).max(0.0);
        assert!((f64::from(y) - 0.25 * drive).abs() < 1e-6, "frame {i}: {y}");
    }
    let mut crush = at_rest(ShapeKind::Crush, 4.0);
    crush.retarget(&WaveshaperControls {
        drive: Drive::new(-3.0),
    });
    let out = run(&mut crush, None, &[vec![0.6; n + 10]]).remove(0);
    // One level per unit once the line is under 1: round(0.6) = 1; above it the line is at
    // hardness h and the output is round(0.6 h) / h.
    for (i, &y) in out.iter().enumerate() {
        let h = (4.0 - 7.0 * (i + 1) as f64 / n as f64).max(1.0);
        let want = (0.6 * h).round() / h;
        assert!(
            (f64::from(y) - want).abs() < 1e-5,
            "frame {i}: {y} vs {want}"
        );
    }
    assert_eq!(out[n + 9], 1.0);
}

// Antiderivative antialiasing.

/// The antialiasing orders `kind` has, first order up.
fn orders_of(kind: ShapeKind) -> impl Iterator<Item = Antialiasing> {
    [Antialiasing::FirstOrder, Antialiasing::SecondOrder]
        .into_iter()
        .filter(move |&order| order <= kind.max_antialiasing())
}

/// A bank of `kind` at rest on `drive`, antialiased to `order`.
fn antialiased(kind: ShapeKind, drive: f64, order: Antialiasing) -> WaveshaperBank<f32> {
    let mut bank = WaveshaperBank::<f32>::new(kind, RATE)
        .antialiasing(order)
        .expect("an order the kind has");
    bank.retarget(&WaveshaperControls {
        drive: Drive::new(drive),
    });
    bank.jump_to_target();
    bank
}

/// The mean of `f` over the segment from `a` to `b`, by the midpoint rule over `n` cells.
fn mean(f: impl Fn(f64) -> f64, a: f64, b: f64, n: usize) -> f64 {
    (0..n)
        .map(|i| f(a + (b - a) * (i as f64 + 0.5) / n as f64))
        .sum::<f64>()
        / n as f64
}

/// First order is the mean of the curve over the segment between two inputs: each output
/// frame, for every kind, is the midpoint-rule mean of the closed-form curve (at the frame's
/// drive, in `f64`) from the input before to this one, to 1e-4 (the staircases' jumps are the
/// quadrature's error: 1/h of a jump over twice the 20 000 cells, 1e-5). This pins `F1` of
/// every kind, the stable `ln cosh` and the `SoftCrush` polynomial among them.
#[test]
fn first_order_is_the_mean_of_the_curve_over_the_segment() {
    let drive = 2.5;
    for kind in KINDS {
        let mut bank = antialiased(kind, drive, Antialiasing::FirstOrder);
        let x = noise(21, 64, 1.0);
        let y = run(&mut bank, None, std::slice::from_ref(&x)).remove(0);
        let mut before = 0.0;
        for (i, (&y, &x)) in y.iter().zip(&x).enumerate() {
            let want = mean(|t| closed_f64(kind, t, drive), before, f64::from(x), 20_000);
            assert!(
                (f64::from(y) - want).abs() < 1e-4,
                "{kind:?} frame {i}: {y} vs {want}"
            );
            before = f64::from(x);
        }
    }
}

/// `F1` of a curve at `u` by the midpoint rule from 0 over `n` cells.
fn integral(f: &impl Fn(f64) -> f64, u: f64, n: usize) -> f64 {
    u * mean(f, 0.0, u, n)
}

/// Second order is `2 (D(a, b) - D(b, c)) / (a - c)` with `D(p, q)` the mean of `F1` over the
/// segment from `q` to `p`, and `F1` the integral of the closed-form curve `g` (here in input
/// units): the `D` of every window of `x` by the midpoint rule over 800 cells, `F1` by
/// `f1` (given), to `bound`. The bank runs at `F = f64`, so the bound is the quadrature's own
/// error.
fn assert_second_order_is_quadrature(
    kind: ShapeKind,
    drive: f64,
    x: &[f32],
    f1: impl Fn(f64) -> f64,
    bound: f64,
) {
    let wide: Vec<f64> = x.iter().map(|&v| f64::from(v)).collect();
    let y = through_f64(kind, drive, Antialiasing::SecondOrder, &wide);
    let d = |p: f64, q: f64| mean(&f1, q, p, 800);
    for (i, w) in x.windows(3).enumerate() {
        let [c, b, a] = [w[0], w[1], w[2]].map(f64::from);
        // Where `a == c` the quotient is its limit, `2 (F1(a) - D(a, b)) / (a - b)`.
        let want = if a == c {
            2.0 * (f1(a) - d(a, b)) / (a - b)
        } else {
            2.0 * (d(a, b) - d(b, c)) / (a - c)
        };
        let y = y[i + 2];
        assert!(
            (y - want).abs() < bound,
            "{kind:?} drive {drive} frame {}: {y} vs {want}",
            i + 2
        );
    }
}

/// `F1` of the closed-form curve at `drive`, by the midpoint rule from 0 over 800 cells.
fn quadrature_f1(kind: ShapeKind, drive: f64) -> impl Fn(f64) -> f64 {
    move |t| integral(&|u| closed_f64(kind, u, drive), t, 800)
}

/// Second order, for the two kinds that have it, on noise: to 1e-5 (the midpoint rule's error
/// at a clip's kink is under 2e-6 at that count).
#[test]
fn second_order_is_the_divided_difference_of_the_mean_antiderivative() {
    for kind in [ShapeKind::Softsign, ShapeKind::HardClip] {
        let x = noise(22, 48, 1.0);
        assert_second_order_is_quadrature(kind, 2.5, &x, quadrature_f1(kind, 2.5), 1e-5);
    }
}

/// The two limits, where the curve is not linear (so a wrong limit is not the identity's
/// answer): `Softsign` at drive 2 on 0.4 and 1.3 alternating, where every window has
/// `a == c` and the second-order limit runs, and `Softsign` at drive 1 on `0.1, 0.10005, 0.9`,
/// where `D(b, c)` has `|b - c|` under the threshold. Both to 1e-5 of the quadrature.
#[test]
fn the_second_order_limits_match_quadrature_on_a_nonlinear_curve() {
    let alternating: Vec<f32> = (0..8).map(|i| if i % 2 == 0 { 0.4 } else { 1.3 }).collect();
    let close: Vec<f32> = [0.1, 0.10005, 0.9].repeat(3);
    for (drive, x) in [(2.0, alternating), (1.0, close)] {
        let kind = ShapeKind::Softsign;
        assert_second_order_is_quadrature(kind, drive, &x, quadrature_f1(kind, drive), 1e-5);
    }
}

/// At drive 1000 on a slow ramp (`u` near 1000, each frame moves it by about 5e-4) the
/// antiderivatives are large and their quotients cancel: the threshold, relative to the
/// inputs' size, sends them to the limit, and the output matches the quadrature to 1e-6. The
/// quadrature takes `F1` as its value at 1 (by 200 000 cells) plus the integral from there,
/// so the cell error is one constant that cancels in the differences.
#[test]
fn second_order_holds_at_large_u_on_a_slow_signal() {
    let kind = ShapeKind::Softsign;
    let drive = 1000.0;
    let g = |t: f64| closed_f64(kind, t, drive);
    let base = integral(&g, 1.0, 200_000);
    let f1 = |t: f64| base + (t - 1.0) * mean(g, 1.0, t, 16);
    let x: Vec<f32> = (0..16)
        .map(|i| (1.0 + 5e-7 * f64::from(i)) as f32)
        .collect();
    assert_second_order_is_quadrature(kind, drive, &x, f1, 1e-6);
}

// The conditioned quotients at `F = f32`.
//
// The ADAA quotients are formed from differences that do not cancel (see `adaa.rs`), so at any
// gap they are within a few `eps` of the closed form, not `eps / gap`. The oracle is the closed
// form's antiderivative quotient in `f64`, where its own cancellation (`2^-29` of `f32`'s over
// the gaps below) is negligible.

/// `F1` of each kind's unit curve in `f64`, the integral of `g` from 0.
fn closed_f1(kind: ShapeKind, u: f64) -> f64 {
    match kind {
        ShapeKind::Tanh => {
            u.abs() + libm::log1p(libm::exp(-2.0 * u.abs())) - std::f64::consts::LN_2
        }
        ShapeKind::Atan => u * libm::atan(u) - 0.5 * libm::log1p(u * u),
        ShapeKind::Softsign => u.abs() - libm::log1p(u.abs()),
        ShapeKind::HardClip => {
            if u.abs() <= 1.0 {
                0.5 * u * u
            } else {
                u.abs() - 0.5
            }
        }
        ShapeKind::Crush => {
            let k = libm::round(u);
            k * u - 0.5 * k * k
        }
        _ => unreachable!("the conditioned kinds"),
    }
}

/// `F2` of `Softsign`'s and `HardClip`'s unit curve in `f64`, odd.
fn closed_f2(kind: ShapeKind, u: f64) -> f64 {
    let a = u.abs();
    match kind {
        ShapeKind::Softsign => (0.5 * a * a + a - (1.0 + a) * libm::log1p(a)).copysign(u),
        _ => {
            let m = if a <= 1.0 {
                a * a * a / 6.0
            } else {
                1.0 / 6.0 + 0.5 * (a * a - a)
            };
            m.copysign(u)
        }
    }
}

/// The inputs the pairs below sit at, and the kinks of the hard clip among them.
const GUARD_INPUTS: [f32; 7] = [0.3, -0.6, 0.97, -1.02, 2.0, -4.0, 8.0];

/// The gaps of the sweeps, relative to the larger of 1 and the magnitudes: `1e-5` to 1.
fn gaps() -> impl Iterator<Item = f64> {
    (0..=10).map(|k| 10f64.powf(-5.0 + f64::from(k) * 0.5))
}

/// The first-order output for the pair `(b, a)` through a unit-drive `f32` bank of `kind`, and
/// the closed-form mean of the curve over `[b, a]` in `f64`, with the `f32` inputs' values.
fn first_order_pair(kind: ShapeKind, b: f32, a: f32) -> (f64, f64) {
    let mut bank = antialiased(kind, 1.0, Antialiasing::FirstOrder);
    let y = run(&mut bank, None, &[vec![b, a]]).remove(0)[1];
    let pre = if kind == ShapeKind::Atan {
        std::f64::consts::FRAC_PI_2
    } else {
        1.0
    };
    let post = if kind == ShapeKind::Atan {
        2.0 / std::f64::consts::PI
    } else {
        1.0
    };
    let (ua, ub) = (f64::from(a) * pre, f64::from(b) * pre);
    let want = post * (closed_f1(kind, ua) - closed_f1(kind, ub)) / (ua - ub);
    (f64::from(y), want)
}

/// At `f32` the first-order quotient of every kind with a conditioned form is within `16 eps
/// max(1, |x|)` of the closed-form mean at every gap from `1e-5` to 1 of the scale, and at the
/// guard's own gap `eps^(1/3)` and below where a plain quotient is off by `eps / gap`. The
/// quotient is the sum of at most four terms of magnitude at most 2 (the curve is bounded by
/// 1.6, and a staircase's by `|x|`), each carrying at most two roundings and a library error of
/// `eps` (the scale `max(1, |x|)` for the staircase). Measured at most `3.2 eps`.
#[test]
fn f32_first_order_is_within_a_few_epsilon_of_the_closed_form_at_every_gap() {
    let eps = f64::from(f32::EPSILON);
    for kind in [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
    ] {
        for x0 in GUARD_INPUTS {
            let scale = f64::from(x0).abs().max(1.0);
            for gap in gaps() {
                let b = x0;
                let a = (f64::from(x0) + gap * scale) as f32;
                let (y, want) = first_order_pair(kind, b, a);
                let bound = 16.0 * eps * scale;
                assert!(
                    (y - want).abs() <= bound,
                    "{kind:?} x0 {x0} gap {gap}: {y} against {want}"
                );
            }
        }
    }
}

/// The plain `f32` quotient `(F1(a) - F1(b)) / (a - b)` of `Softsign`'s antiderivative, swept
/// over the same gaps, is off by about `eps / gap`: at its worst more than ten times the
/// conditioned form's bound `16 eps max(1, |x|)` (measured 3e-3 to 4e-1 against 2e-6 to 1.5e-5).
#[test]
fn a_plain_f32_quotient_at_a_small_gap_is_over_ten_times_the_conditioned_bound() {
    let eps = f64::from(f32::EPSILON);
    let f1 = |u: f32| u.abs() - u.abs().ln_1p();
    for x0 in GUARD_INPUTS {
        let scale = f64::from(x0).abs().max(1.0);
        let worst = gaps()
            .filter(|&gap| gap < 1e-3)
            .map(|gap| {
                let a = (f64::from(x0) + gap * scale) as f32;
                let want = (closed_f1(ShapeKind::Softsign, f64::from(a))
                    - closed_f1(ShapeKind::Softsign, f64::from(x0)))
                    / (f64::from(a) - f64::from(x0));
                (f64::from((f1(a) - f1(x0)) / (a - x0)) - want).abs()
            })
            .fold(0.0, f64::max);
        assert!(
            worst > 10.0 * 16.0 * eps * scale,
            "x0 {x0}: the plain quotient's worst error {worst}"
        );
    }
}

/// The second-order output for the window `(c, b, a)` (newest `a`) through a unit-drive `f32`
/// bank of `kind`, and the closed-form `2 (D(a, b) - D(b, c)) / (a - c)` in `f64`, `D(p, q)` the
/// quotient of the closed-form second antiderivative.
fn second_order_window(kind: ShapeKind, c: f32, b: f32, a: f32) -> (f64, f64) {
    let mut bank = antialiased(kind, 1.0, Antialiasing::SecondOrder);
    let y = run(&mut bank, None, &[vec![c, b, a]]).remove(0)[2];
    let [fa, fb, fc] = [a, b, c].map(f64::from);
    let d = |p: f64, q: f64| (closed_f2(kind, p) - closed_f2(kind, q)) / (p - q);
    (f64::from(y), 2.0 * (d(fa, fb) - d(fb, fc)) / (fa - fc))
}

/// The windows of the second-order tests: three inputs `x0`, `x0 + s`, `x0 + 2.7 s` (unequal
/// steps) for steps `s` from `2e-3` to `0.2` of the scale, the guard `(36 eps)^(1/3)` (0.016) among
/// them.
fn windows() -> impl Iterator<Item = (f32, f32, f32, f64)> {
    GUARD_INPUTS.into_iter().flat_map(|x0| {
        let scale = f64::from(x0).abs().max(1.0);
        [2e-3, 4e-3, 1e-2, 3e-2, 0.2].into_iter().map(move |rel| {
            let s = rel * scale;
            let b = (f64::from(x0) + s) as f32;
            let a = (f64::from(x0) + 2.7 * s) as f32;
            (x0, b, a, scale)
        })
    })
}

/// At `f32` the hard clip's second-order quotient is exact in form, a sum of terms of one sign
/// and the cubes' divided differences: within `16 eps max(1, |x|)` of the closed form at every
/// window, those that straddle a kink among them (measured at most `2.5 eps`).
#[test]
fn f32_second_order_hard_clip_is_within_a_few_epsilon_of_the_closed_form() {
    let eps = f64::from(f32::EPSILON);
    for (c, b, a, scale) in windows() {
        let (y, want) = second_order_window(ShapeKind::HardClip, c, b, a);
        assert!(
            (y - want).abs() <= 16.0 * eps * scale,
            "window {c} {b} {a}: {y} against {want}"
        );
    }
}

/// At `f32` the `Softsign` second-order quotient, outside the guard `(36 eps)^(1/3)` (a span of
/// `a - c` of the guard or more), is within `2 (e(a, b) + e(b, c)) / |a - c| + 3 eps |y|` of
/// the closed form, where `e(p, q) = 8 eps (2 + max(|p|, |q|))` is the error of the mean of `F1`
/// (four terms of magnitude at most `2 + |u|` with two roundings each): the nested quotient is
/// `eps` over the span, not `eps` over its square. Inside the guard the limit, the curve at the
/// mean of the three, is the closed form to the Taylor term `g'' sum((x - mean)^2) / 24` (`|g''|
/// <= 2`) and the curve's own rounding of `3 eps`. Measured 1.7e-5 at most outside (before the
/// conditioning, 3.1e-4).
#[test]
fn f32_second_order_softsign_holds_the_closed_form_to_its_derived_bound() {
    let eps = f64::from(f32::EPSILON);
    let guard = libm::cbrt(36.0 * eps);
    for (c, b, a, scale) in windows() {
        let (y, want) = second_order_window(ShapeKind::Softsign, c, b, a);
        let [fa, fb, fc] = [a, b, c].map(f64::from);
        let span = (fa - fc).abs();
        let bound = if span < guard * scale {
            let mean = (fa + fb + fc) / 3.0;
            let spread: f64 = [fa, fb, fc].iter().map(|v| (v - mean) * (v - mean)).sum();
            spread / 12.0 + 3.0 * eps
        } else {
            let e = |p: f64, q: f64| 8.0 * eps * (2.0 + p.abs().max(q.abs()));
            2.0 * (e(fa, fb) + e(fb, fc)) / span + 3.0 * eps * want.abs()
        };
        assert!(
            (y - want).abs() <= bound,
            "window {c} {b} {a}: {y} against {want}, bound {bound}"
        );
    }
}

/// Inside the guard with unequal steps the limit is the curve at the mean of the three inputs,
/// not at the midpoint of the outer two: for `a = -0.00648`, `b = -0.00678`, `c = -0.02502`
/// the closed form is within the bound `sum((x - mean)^2) / 12 + 3 eps` of `g(mean)`, and the
/// midpoint's value, off by `g' (m - mean)` (3e-3), is over 100 times that bound from it.
#[test]
fn the_f32_second_order_limit_with_unequal_steps_is_the_curve_at_the_mean_of_the_three() {
    let eps = f64::from(f32::EPSILON);
    let (c, b, a) = (-0.02502f32, -0.00678f32, -0.00648f32);
    let (y, want) = second_order_window(ShapeKind::Softsign, c, b, a);
    let [fa, fb, fc] = [a, b, c].map(f64::from);
    let mean = (fa + fb + fc) / 3.0;
    let spread: f64 = [fa, fb, fc].iter().map(|v| (v - mean) * (v - mean)).sum();
    let bound = spread / 12.0 + 3.0 * eps;
    assert!(
        (y - want).abs() <= bound,
        "{y} against {want}, bound {bound}"
    );
    let midpoint = 0.5 * (fa + fc);
    let g = |u: f64| u / (1.0 + u.abs());
    assert!(
        (g(midpoint) - want).abs() > 100.0 * bound,
        "the midpoint's limit is far from the closed form"
    );
}

/// The orders compare as `Off < FirstOrder < SecondOrder`: a kind refuses an order above its
/// highest.
#[test]
fn the_orders_are_ordered() {
    assert!(Antialiasing::Off < Antialiasing::FirstOrder);
    assert!(Antialiasing::FirstOrder < Antialiasing::SecondOrder);
}

/// The fundamental's period in frames for the harmonic tests: 750 Hz at 48 kHz.
const PERIOD: usize = 64;

/// A drive-`drive` sine of unit amplitude at 750 Hz through `kind` antialiased to `order`: the
/// amplitude of each of the harmonics `ks` over 32 whole periods, after the first.
fn harmonics(kind: ShapeKind, drive: f64, order: Antialiasing, ks: &[usize]) -> Vec<f64> {
    let w0 = std::f64::consts::TAU / PERIOD as f64;
    let x: Vec<f32> = (0..33 * PERIOD)
        .map(|i| (w0 * i as f64).sin() as f32)
        .collect();
    let y = run(&mut antialiased(kind, drive, order), None, &[x]).remove(0);
    ks.iter()
        .map(|&k| amplitude_at(&y[PERIOD..], Hz::new(750.0 * k as f64), RATE))
        .collect()
}

/// The first-order output's harmonic `k` of a 750 Hz fundamental is the curve's Fourier
/// coefficient times `sinc(k w0 / 2)`, the droop of the one-frame mean `F1` takes. (Not
/// `cos(k w0 / 2)`: that is the droop of the two-point average, which a mean over the segment
/// is not.) The clipped sine has the closed form, with `t = asin(1 / a)` and `a` the drive,
/// `b_k = 4 / pi (a I_k + cos(k t) / k)` for odd `k`, `I_1 = (t - sin t cos t) / 2` and
/// `I_k = (sin((k-1) t) / (k-1) - sin((k+1) t) / (k+1)) / 2`; tanh's coefficient is the
/// 4096-point trapezoid rule over a period (the integrand is periodic and analytic, so its error
/// is far under 1e-12). The residue left is the naive curve's own aliasing and the
/// piecewise-linear chord of the sampled sine; harmonics 1 to 7 hold to 0.5% of the
/// coefficient (the worst measured, hard clip's 7th, is 0.25%).
#[test]
fn first_order_harmonics_are_the_fourier_coefficients_under_the_frame_mean_droop() {
    use std::f64::consts::{PI, TAU};
    let w0 = TAU / PERIOD as f64;
    let droop = |k: usize| {
        let h = k as f64 * w0 / 2.0;
        h.sin() / h
    };
    let ks = [1, 3, 5, 7];
    let a = 3.0f64;
    let t = (1.0 / a).asin();
    let clip = |k: usize| {
        let kf = k as f64;
        let i = if k == 1 {
            0.5 * (t - t.sin() * t.cos())
        } else {
            0.5 * (((kf - 1.0) * t).sin() / (kf - 1.0) - ((kf + 1.0) * t).sin() / (kf + 1.0))
        };
        4.0 / PI * (a * i + (kf * t).cos() / kf)
    };
    let tanh = |k: usize| {
        let n = 4096;
        (0..n)
            .map(|j| {
                let th = TAU * j as f64 / n as f64;
                (a * th.sin()).tanh() * (k as f64 * th).sin()
            })
            .sum::<f64>()
            * 2.0
            / n as f64
    };
    let rows: [(ShapeKind, &dyn Fn(usize) -> f64); 2] =
        [(ShapeKind::HardClip, &clip), (ShapeKind::Tanh, &tanh)];
    for (kind, coefficient) in rows {
        let got = harmonics(kind, a, Antialiasing::FirstOrder, &ks);
        for (&k, got) in ks.iter().zip(got) {
            let want = coefficient(k) * droop(k);
            assert!(
                (got - want).abs() < 5e-3 * coefficient(k),
                "{kind:?} harmonic {k}: {got} vs {want}"
            );
        }
    }
}

/// The aliased energy of a 5 kHz sine, 0.9 in amplitude, at 44.1 kHz through `kind` at `drive`
/// antialiased to `order`: the root-sum-square of the amplitudes at the folds of the 5th, 7th,
/// 9th, 11th, 13th and 15th harmonics (19.1, 9.1, 0.9, 10.9, 20.9 and 13.2 kHz), none of which
/// is a harmonic itself. The window is 3 528 frames, 400 periods of the fold spacing of 100 Hz,
/// so each bin is exact.
fn alias_energy(kind: ShapeKind, drive: f64, order: Antialiasing) -> f64 {
    let rate = 44_100.0;
    let x: Vec<f32> = (0..441 * 9)
        .map(|i| (0.9 * (std::f64::consts::TAU * 5_000.0 * i as f64 / rate).sin()) as f32)
        .collect();
    let y = run(&mut antialiased(kind, drive, order), None, &[x]).remove(0);
    [19_100.0, 9_100.0, 900.0, 10_900.0, 20_900.0, 13_200.0]
        .iter()
        .map(|&hz| amplitude_at(&y[441..], Hz::new(hz), SampleRate::of(rate)).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Antialiasing takes the aliased energy of a driven 5 kHz sine under the naive curve's. A
/// figure relative to the naive shaper at the same input is the property: antiderivative
/// antialiasing claims an attenuation of the aliases against the memoryless curve, not an
/// absolute level, which follows the curve, the drive and the frequency. The margins are set
/// under what is measured, at drive 3: first order, at least 5 dB for `Tanh`,
/// `Atan`, `Softsign` and `HardClip` (measured 6.8 to 7.1 dB) and 3 dB for the two staircases
/// (9.2 and 13.7 dB); second order, at least 12 dB for `HardClip` (16.1) and `Softsign` (15.1).
#[test]
fn antialiasing_lowers_the_aliased_energy() {
    let db = |kind, drive, order| {
        20.0 * (alias_energy(kind, drive, order) / alias_energy(kind, drive, Antialiasing::Off))
            .log10()
    };
    for (kind, margin) in [
        (ShapeKind::Tanh, 5.0),
        (ShapeKind::Atan, 5.0),
        (ShapeKind::Softsign, 5.0),
        (ShapeKind::HardClip, 5.0),
        (ShapeKind::Crush, 3.0),
        (ShapeKind::SoftCrush, 3.0),
    ] {
        let first = db(kind, 3.0, Antialiasing::FirstOrder);
        assert!(first < -margin, "{kind:?} first order: {first} dB");
    }
    for (kind, margin) in [(ShapeKind::HardClip, 12.0), (ShapeKind::Softsign, 12.0)] {
        let second = db(kind, 3.0, Antialiasing::SecondOrder);
        assert!(second < -margin, "{kind:?} second order: {second} dB");
    }
}

/// A constant input is the curve itself, to 1e-6 (the `f64` curve rounded to `f32`), for every
/// kind and order, once the history holds the constant: the segment has no length, and the
/// fallback is the curve at its midpoint (first order), or at the mean of the three (second).
#[test]
fn a_constant_input_is_the_curve() {
    for kind in KINDS {
        for order in orders_of(kind) {
            let mut bank = antialiased(kind, 2.5, order);
            for x in [0.3f32, -0.7, 0.0] {
                let y = run(&mut bank, None, &[vec![x; 6]]).remove(0);
                let want = closed(kind, x, 2.5);
                for (i, &y) in y.iter().enumerate().skip(2) {
                    assert!(
                        (f64::from(y) - want).abs() < 1e-6,
                        "{kind:?} {order:?} at {x}, frame {i}: {y} vs {want}"
                    );
                }
            }
        }
    }
}

/// A drive step on a constant input is the curve's own step and no more: the history is the
/// input before the drive, so the segment from the last input to this one stays a point, and
/// frame by frame the output is the curve at that frame's drive. (With a history after the
/// drive the first frame at the new drive would be a mean over the segment from `0.2` to `0.8`.)
#[test]
fn a_drive_step_makes_no_step_beyond_the_curve() {
    let mut signal = vec![1.0f32; 16];
    signal[8..].fill(4.0);
    for (kind, order) in [
        (ShapeKind::HardClip, Antialiasing::FirstOrder),
        (ShapeKind::HardClip, Antialiasing::SecondOrder),
        (ShapeKind::Softsign, Antialiasing::SecondOrder),
        (ShapeKind::Tanh, Antialiasing::FirstOrder),
    ] {
        let mut bank = antialiased(kind, 1.0, order);
        let y = run(&mut bank, Some(&signal), &[vec![0.2; 16]]).remove(0);
        for (i, (&y, &d)) in y.iter().zip(&signal).enumerate().skip(2) {
            let want = closed(kind, 0.2, f64::from(d));
            assert!(
                (f64::from(y) - want).abs() < 1e-6,
                "{kind:?} {order:?} frame {i}: {y} vs {want}"
            );
        }
    }
}

/// Inside the clip the curve is the identity, so the antialiased filter is linear and its
/// output is exact: first order is `(x + x') / 2`, second order `(x + x' + x'') / 3`, including
/// where `x = x''` and the second-order limit `(2 m + x') / 3` takes over.
#[test]
fn inside_the_clip_the_orders_are_the_two_and_three_point_means() {
    let x = noise(23, 48, 0.9);
    let first = run(
        &mut antialiased(ShapeKind::HardClip, 1.0, Antialiasing::FirstOrder),
        None,
        std::slice::from_ref(&x),
    )
    .remove(0);
    let second = run(
        &mut antialiased(ShapeKind::HardClip, 1.0, Antialiasing::SecondOrder),
        None,
        std::slice::from_ref(&x),
    )
    .remove(0);
    #[allow(clippy::needless_range_loop)]
    for (i, w) in x.windows(3).enumerate() {
        let [c, b, a] = [w[0], w[1], w[2]].map(f64::from);
        let i = i + 2;
        assert!(
            (f64::from(first[i]) - (a + b) / 2.0).abs() < 1e-6,
            "first {i}"
        );
        assert!(
            (f64::from(second[i]) - (a + b + c) / 3.0).abs() < 1e-6,
            "second {i}"
        );
    }
    // The input alternates 0.1, 0.3: every frame has x == x''.
    let alternating: Vec<f32> = (0..16)
        .map(|i| if i % 2 == 0 { 0.1 } else { 0.3 })
        .collect();
    let y = run(
        &mut antialiased(ShapeKind::HardClip, 1.0, Antialiasing::SecondOrder),
        None,
        std::slice::from_ref(&alternating),
    )
    .remove(0);
    for i in 2..16 {
        let (m, b) = (f64::from(alternating[i]), f64::from(alternating[i - 1]));
        assert!(
            (f64::from(y[i]) - (2.0 * m + b) / 3.0).abs() < 1e-6,
            "limit {i}"
        );
    }
}

/// The delay of the orders is the centroid of the impulse response of a small impulse inside
/// the clip: `[1/2, 1/2]` at first order, half a frame, `[1/3, 1/3, 1/3]` at second, one frame.
/// `Antialiasing::delay` is that, rounded to the nearest frame, a half up, and divided down by
/// the rate factor.
#[test]
fn the_delay_is_the_centroid_of_the_impulse_response() {
    let mut x = vec![0.0f32; 8];
    x[0] = 0.25;
    for (order, centroid, ring) in [
        (Antialiasing::FirstOrder, 0.5, 1),
        (Antialiasing::SecondOrder, 1.0, 2),
    ] {
        let y = run(
            &mut antialiased(ShapeKind::HardClip, 1.0, order),
            None,
            std::slice::from_ref(&x),
        )
        .remove(0);
        let total: f64 = y.iter().map(|&v| f64::from(v)).sum();
        let at: f64 = y
            .iter()
            .enumerate()
            .map(|(i, &v)| i as f64 * f64::from(v))
            .sum();
        assert!((total - 0.25).abs() < 1e-7, "{order:?}: unit gain");
        assert!((at / total - centroid).abs() < 1e-6, "{order:?}: {y:?}");
        assert!(
            y[ring + 1..].iter().all(|&v| v == 0.0),
            "{order:?}: rings on"
        );
        assert!(y[ring] != 0.0, "{order:?}: {ring} frames of ring-out");
        assert_eq!(order.ring_out(None), Samples::new(ring), "{order:?}");
        assert_eq!(order.delay(None), Samples::new(1), "{order:?}");
    }
    assert_eq!(Antialiasing::Off.delay(None), Samples::ZERO);
    assert_eq!(Antialiasing::Off.ring_out(None), Samples::ZERO);
    // At 2x the rate a first-order half frame is a quarter of a base frame, a second-order
    // frame half of one; at 4x, an eighth and a quarter.
    assert_eq!(
        Antialiasing::FirstOrder.delay(Some(OversamplingFactor::X2)),
        Samples::new(0)
    );
    assert_eq!(
        Antialiasing::SecondOrder.delay(Some(OversamplingFactor::X2)),
        Samples::new(1)
    );
    assert_eq!(
        Antialiasing::SecondOrder.delay(Some(OversamplingFactor::X4)),
        Samples::new(0)
    );
    assert_eq!(
        Antialiasing::SecondOrder.ring_out(Some(OversamplingFactor::X2)),
        Samples::new(1)
    );
}

/// The kinds with a second antiderivative are `Softsign` and `HardClip`; an order past a kind's
/// highest is refused with the kind, the order and the highest named, `Off` and the highest are
/// taken, and a refusal leaves the bank as it was.
#[test]
fn an_order_past_the_kinds_highest_is_refused() {
    for kind in KINDS {
        let highest = match kind {
            ShapeKind::Softsign | ShapeKind::HardClip => Antialiasing::SecondOrder,
            _ => Antialiasing::FirstOrder,
        };
        assert_eq!(kind.max_antialiasing(), highest, "{kind:?}");
        let bank = |order| WaveshaperBank::<f32>::new(kind, RATE).antialiasing(order);
        for order in [Antialiasing::Off, Antialiasing::FirstOrder, highest] {
            assert_eq!(bank(order).unwrap().antialiasing_order(), order, "{kind:?}");
        }
        if highest == Antialiasing::FirstOrder {
            assert_eq!(
                bank(Antialiasing::SecondOrder).unwrap_err(),
                AntialiasingError {
                    kind,
                    order: Antialiasing::SecondOrder,
                    max: Antialiasing::FirstOrder,
                }
            );
        } else {
            assert!(bank(Antialiasing::SecondOrder).is_ok());
        }
    }
}

/// The history is per channel and a reset clears it: a first-order bank fed a block on one
/// channel and silence on the other leaves nothing in the second, and after a reset an input
/// of `0.5` reads half of it (the history is 0) where it read the full `0.5` before.
#[test]
fn the_history_is_per_channel_and_a_reset_clears_it() {
    let mut bank = antialiased(ShapeKind::HardClip, 1.0, Antialiasing::FirstOrder);
    let out = run(&mut bank, None, &[vec![0.5; 4], vec![0.0; 4]]);
    assert!(out[1].iter().all(|&y| y == 0.0));
    assert_eq!(out[0][3], 0.5);
    bank.reset();
    let out = run(&mut bank, None, &[vec![0.5], vec![0.0]]);
    assert_eq!(out[0][0], 0.25);
}

/// A NaN input is heard at its own frame and no later: the history takes 0 for it. An infinity
/// is held to the argument bound, so it is the curve's end value, finite, and the frames after it
/// are the NaN's (the history takes 0 for both).
#[test]
fn a_non_finite_input_is_heard_at_its_own_frame_only() {
    for order in [Antialiasing::FirstOrder, Antialiasing::SecondOrder] {
        let through = |bad: f32| {
            let mut bank = antialiased(ShapeKind::Softsign, 2.0, order);
            let x = [0.3, 0.3, bad, 0.3, 0.3, 0.3];
            run(&mut bank, None, &[x.to_vec()]).remove(0)
        };
        let nan = through(f32::NAN);
        for (i, y) in nan.iter().enumerate() {
            assert_eq!(!y.is_finite(), i == 2, "{order:?} NaN: frame {i}: {y}");
        }
        let inf = through(f32::INFINITY);
        assert!(inf.iter().all(|y| y.is_finite()), "{order:?} inf: {inf:?}");
        assert_eq!(inf[3..], nan[3..], "{order:?}");
    }
}

/// One block of `x` through an `f64` bank of `kind` at rest on `drive`, at `order`.
fn through_f64(kind: ShapeKind, drive: f64, order: Antialiasing, x: &[f64]) -> Vec<f64> {
    let mut bank = WaveshaperBank::<f64>::new(kind, RATE)
        .antialiasing(order)
        .expect("an order the kind has");
    bank.retarget(&WaveshaperControls {
        drive: Drive::new(drive),
    });
    bank.jump_to_target();
    let mut out = vec![0.0f64; x.len()];
    bank.process(
        None,
        &mut InputOutput::paired(
            Planes::new(&[x]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    out
}

/// An `f64` bank holds the drive and the signal in `f64`: on a 29-bit signal at a drive
/// of 1.7, each curve is its closed form in `f64`
/// (`tanh h x`, `atan(pi h x / 2) 2 / pi`, `h x / (1 + |h x|)`, `clamp(h x)` and
/// `round(h x) / h`), to 2^-45; an `f32` curve is off by about 1e-8.
#[test]
fn an_f64_bank_is_each_curve_in_f64() {
    use std::f64::consts::PI;
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(200);
    let (drive, crush) = (1.7f64, 4096.0f64);
    let h = drive;
    let closed: [(ShapeKind, f64, Box<dyn Fn(f64) -> f64>); 5] = [
        (ShapeKind::Tanh, drive, Box::new(move |x| (h * x).tanh())),
        (
            ShapeKind::Atan,
            drive,
            Box::new(move |x| (PI / 2.0 * h * x).atan() * (2.0 / PI)),
        ),
        (
            ShapeKind::Softsign,
            drive,
            Box::new(move |x| h * x / (1.0 + (h * x).abs())),
        ),
        (
            ShapeKind::HardClip,
            drive,
            Box::new(move |x| (h * x).clamp(-1.0, 1.0)),
        ),
        (
            ShapeKind::Crush,
            crush,
            Box::new(move |x| (crush * x).round() / crush),
        ),
    ];
    for (kind, drive, f) in closed {
        let want: Vec<f64> = x.iter().map(|&x| f(x)).collect();
        let got = through_f64(kind, drive, Antialiasing::Off, &x);
        assert_close_f64(&got, &want, 2.0f64.powi(-45));
    }
}

/// First-order ADAA in `f64` is the divided difference of the antiderivative,
/// `(F(x[n]) - F(x[n-1])) / (x[n] - x[n-1])`, with the history starting at 0: of `u^2 / 2`
/// for a hard clip inside its linear region and of `ln cosh u` for `tanh`, on a 29-bit
/// signal that alternates in sign so that no step is inside the threshold, to 2^-45.
#[test]
fn an_f64_first_order_adaa_is_the_divided_difference_of_the_antiderivative() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x: Vec<f64> = wide_signal(200)
        .iter()
        .enumerate()
        .map(|(n, v)| if n % 2 == 0 { *v } else { -2.0 * v })
        .collect();
    type Antiderivative = fn(f64) -> f64;
    let cases: [(ShapeKind, Antiderivative); 2] = [
        (ShapeKind::HardClip, |u| u * u / 2.0),
        (ShapeKind::Tanh, |u| u.cosh().ln()),
    ];
    for (kind, antiderivative) in cases {
        let mut want = Vec::new();
        let mut before = 0.0f64;
        for &now in &x {
            want.push((antiderivative(now) - antiderivative(before)) / (now - before));
            before = now;
        }
        let got = through_f64(kind, 1.0, Antialiasing::FirstOrder, &x);
        assert_close_f64(&got, &want, 2.0f64.powi(-45));
    }
}

/// A huge input at `f32` is held to the argument bound, so the antiderivatives' squares are
/// finite: an input of `1e30` (whose square overflows `f32`) at every order and kind gives a
/// finite output, the curve's end value to within the first-order mean of the held segment.
#[test]
fn an_f32_input_of_1e30_gives_a_finite_output_at_every_order() {
    for kind in KINDS {
        for order in orders_of(kind) {
            for sign in [1.0f32, -1.0] {
                let mut bank = antialiased(kind, 2.0, order);
                let x = vec![0.3, 1e30 * sign, 1e30 * sign, 0.3, 1e30 * sign, 0.3];
                let y = run(&mut bank, None, &[x]).remove(0);
                assert!(
                    y.iter().all(|y| y.is_finite()),
                    "{kind:?} {order:?} {sign}: {y:?}"
                );
            }
        }
    }
}

/// The triangle fold at chosen drives and inputs is the closed form: `x` times the drive
/// reflected into `-1..=1` with a period of 4, to 1e-6 (`f32`), and it is the theory closed
/// form, `2 / pi asin(sin(pi u / 2))`, to the same.
#[test]
fn the_triangle_fold_is_the_reflection_of_the_driven_input_at_chosen_points() {
    let exact = [
        // Inside -1..=1 the identity.
        (0.3, 1.0, 0.3),
        (-0.75, 1.0, -0.75),
        // 1 is the first peak; past it the signal comes back down: u = 1.5 is 0.5.
        (1.0, 1.0, 1.0),
        (1.5, 1.0, 0.5),
        (2.0, 1.0, 0.0),
        (2.5, 1.0, -0.5),
        // The trough is at 3, and the period closes at 4.
        (3.0, 1.0, -1.0),
        (3.5, 1.0, -0.5),
        (4.5, 1.0, 0.5),
        (5.5, 1.0, 0.5),
        // The drive scales the input first: 0.75 at 2 is 1.5, 1.25 at 3 is 3.75.
        (0.75, 2.0, 0.5),
        (-0.75, 2.0, -0.5),
        (1.25, 3.0, -0.25),
        (-0.8, 3.0, 0.4f32),
    ];
    for (x, drive, want) in exact {
        let got = ShapeKind::TriangleFold.apply::<f32>(x, Drive::new(drive));
        assert!(
            (f64::from(got) - f64::from(want)).abs() < 1e-6,
            "fold({x}, drive {drive}) = {got}, want {want}"
        );
        let theory = closed(ShapeKind::TriangleFold, x, drive);
        assert!(
            (f64::from(got) - theory).abs() < 1e-6,
            "fold({x}, drive {drive}) = {got}, theory {theory}"
        );
    }
    // Every drive and input stays in -1..=1.
    for drive in [0.5, 1.0, 7.3, 100.0] {
        for x in noise(5, 256, 5.0) {
            assert!(
                ShapeKind::TriangleFold
                    .apply::<f32>(x, Drive::new(drive))
                    .abs()
                    <= 1.0
            );
        }
    }
}

/// `F1` of the triangle fold, from its Fourier series: the fold is
/// `8 / pi^2 sum_{k odd} (-1)^((k-1)/2) sin(k pi u / 2) / k^2`, so its integral from 0 is
/// `16 / pi^3 sum_{k odd} (-1)^((k-1)/2) (1 - cos(k pi u / 2)) / k^3`. Over 20 000 terms the
/// tail is under `16 / pi^3 * 2 / (4 * 20000^2)`, 3e-10.
fn fold_f1_fourier(u: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let sum: f64 = (0..20_000)
        .map(|n| {
            let k = f64::from(2 * n + 1);
            let sign = if n % 2 == 0 { 1.0 } else { -1.0 };
            sign * (1.0 - (k * pi * u / 2.0).cos()) / (k * k * k)
        })
        .sum();
    16.0 / (pi * pi * pi) * sum
}

/// First-order ADAA of the triangle fold is the quotient of the antiderivative at both ends of
/// the segment: `(F1(u) - F1(u')) / (u - u')`, `u = drive x`, at a segment inside one linear
/// piece, one across a kink with a gap under 1, one across several kinks and periods, and one
/// at another drive. The antiderivative is the Fourier series' ([`fold_f1_fourier`]), so the
/// bound is its tail, 3e-10 over the gap (at least 0.15): 1e-8.
#[test]
fn first_order_fold_is_the_antiderivative_quotient_at_chosen_segments() {
    let cases = [
        // Inside one piece.
        (0.5, -0.25, 1.0),
        // Across the kink at 1, 0.15 wide.
        (0.9, 1.05, 1.0),
        // Across the kinks at 1 and 3 and the period's edge.
        (-2.3, 3.1, 1.0),
        // At drive 2.5 the segment is 0.25 to 1.25: across the kink at a gap of 1.
        (0.1, 0.5, 2.5),
    ];
    for (before, now, drive) in cases {
        let got = through_f64(
            ShapeKind::TriangleFold,
            drive,
            Antialiasing::FirstOrder,
            &[before, now],
        )[1];
        let (b, a) = (before * drive, now * drive);
        let want = (fold_f1_fourier(a) - fold_f1_fourier(b)) / (a - b);
        assert!(
            (got - want).abs() < 1e-8,
            "{before} to {now} at {drive}: {got} against {want}"
        );
    }
}

/// A segment far under the guard across a kink keeps its closed form at `f32`: from `0.999` to
/// `1.001` the fold is `1 - |u - 1|`, whose mean is `1 - ((1 - b)^2 + (a - 1)^2) / (2 (a -
/// b))`, to 4 eps of `f32` (a sum of a few terms of order 1, each rounded once); a plain
/// quotient of the antiderivative's values is off by `eps / gap`, 6e-5.
#[test]
fn first_order_fold_across_a_kink_at_a_tiny_gap_holds_to_a_few_epsilon() {
    let (b, a) = (0.999f32, 1.001f32);
    let mut bank = antialiased(ShapeKind::TriangleFold, 1.0, Antialiasing::FirstOrder);
    let y = run(&mut bank, None, &[vec![b, a]]).remove(0)[1];
    let (b, a) = (f64::from(b), f64::from(a));
    let want = 1.0 - ((1.0 - b).powi(2) + (a - 1.0).powi(2)) / (2.0 * (a - b));
    let eps = f64::from(f32::EPSILON);
    assert!(
        (f64::from(y) - want).abs() < 4.0 * eps,
        "{y} against {want}"
    );
}
