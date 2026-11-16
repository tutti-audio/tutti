use super::*;
use crate::testing::noise;

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
        let got = kind.apply(x, Drive(drive));
        assert_eq!(
            got.to_bits(),
            f32::to_bits(want),
            "{kind:?}({x}, drive {drive}) = {got}, want exactly {want}"
        );
    }

    // Off the midpoint the S-curve is a rational: smooth9(1/4) is
    // 12826 / 262144 exactly, so at hardness 1 a quarter step lands there.
    let quarter = ShapeKind::SoftCrush.apply(0.25, Drive(1.0));
    assert!((f64::from(quarter) - 12826.0 / 262144.0).abs() < 1e-7);

    let half_pi = std::f64::consts::FRAC_PI_2;
    let closed = [
        (ShapeKind::Tanh, 0.5f32, 2.0f32, 1.0f64.tanh()),
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
        let got = f64::from(kind.apply(x, Drive(drive)));
        assert!(
            (got - want).abs() < 1e-6,
            "{kind:?}({x}, drive {drive}) = {got}, want {want}"
        );
    }
}

const KINDS: [ShapeKind; 6] = [
    ShapeKind::Tanh,
    ShapeKind::Atan,
    ShapeKind::Softsign,
    ShapeKind::HardClip,
    ShapeKind::Crush,
    ShapeKind::SoftCrush,
];

/// The most an `f32` render may differ from the `f64` closed form: the soft staircase's
/// degree-nine polynomial loses a few `f32` ulps of its range.
const TOL: f64 = 1e-4;

/// The curve's closed form in `f64`, at `drive` (the staircases' levels floored at 1).
fn closed(kind: ShapeKind, x: f32, drive: f32) -> f64 {
    let (x, d) = (f64::from(x), f64::from(drive));
    let half_pi = std::f64::consts::FRAC_PI_2;
    match kind {
        ShapeKind::Tanh => (x * d).tanh(),
        ShapeKind::Atan => (x * d * half_pi).atan() / half_pi,
        ShapeKind::Softsign => x * d / (1.0 + (x * d).abs()),
        ShapeKind::HardClip => (x * d).clamp(-1.0, 1.0),
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

/// A bank of `kind` landed on `drive`.
fn landed(kind: ShapeKind, drive: f32) -> WaveshaperBank {
    let mut bank = WaveshaperBank::new(kind, RATE);
    bank.aim(&WaveshaperControls {
        drive: Drive(drive),
    });
    bank.settle();
    bank
}

/// `bank` over `channels` (each a plane), one block, with `lane` the drive per frame.
fn run(bank: &mut WaveshaperBank, lane: Option<&[f32]>, channels: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
    let mut out = vec![vec![0.0f32; channels[0].len()]; channels.len()];
    let planes = Planes::new(&refs).unwrap();
    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    bank.process(
        lane.map(Mono::new),
        planes,
        PlanesMut::new(&mut outs).unwrap(),
    );
    out
}

/// A block through a landed bank is the curve of each sample at the drive: the loop for
/// each kind picks the curve it names and reads its own channel at its own frame.
#[test]
fn a_held_block_is_the_curve_of_each_sample() {
    for kind in KINDS {
        let mut bank = landed(kind, 2.5);
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

/// With a lane that moves every frame, every channel's sample is the curve at that frame's
/// drive, across the chunk boundary.
#[test]
fn a_lane_is_read_per_frame_on_every_channel() {
    let lane: Vec<f32> = (0..150).map(|i| 1.0 + 0.125 * i as f32).collect();
    for kind in KINDS {
        let mut bank = landed(kind, 1.0);
        let ins: Vec<Vec<f32>> = (0..3).map(|c| noise(c + 11, 150, 1.0)).collect();
        for (c, (y, x)) in run(&mut bank, Some(&lane), &ins)
            .iter()
            .zip(&ins)
            .enumerate()
        {
            for (i, (&y, &x)) in y.iter().zip(x).enumerate() {
                let want = closed(kind, x, lane[i]);
                assert!(
                    (f64::from(y) - want).abs() < TOL,
                    "{kind:?} ch{c} frame {i}: {y} vs {want}"
                );
            }
        }
    }
}

/// A lane frame that is not finite reads the aimed drive, a finite one its own.
#[test]
fn a_non_finite_lane_frame_reads_the_aimed_drive() {
    let mut bank = landed(ShapeKind::HardClip, 2.0);
    let lane = [f32::NAN, 4.0, f32::INFINITY, f32::NEG_INFINITY];
    let out = run(&mut bank, Some(&lane), &[vec![0.25; 4]]);
    assert_eq!(out[0], [0.5, 1.0, 0.5, 0.5]);
}

/// A drive aimed past the held one glides over `GAIN_GLIDE`: frame `i` of a hard clip of a
/// constant is the line `1 + (3 - 1) * (i + 1) / n`, in whole blocks or in pieces, and the
/// block after it is on the target.
#[test]
fn a_drive_step_glides_linearly_over_the_glide() {
    let n = GAIN_GLIDE.to_samples_ceil(RATE).get();
    let x = 0.2f32;
    for block in [n + 40, 7] {
        let mut bank = landed(ShapeKind::HardClip, 1.0);
        bank.aim(&WaveshaperControls { drive: Drive(3.0) });
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
    let mut bank = landed(ShapeKind::HardClip, 1.0);
    let out = run(&mut bank, None, &[vec![4.0], vec![0.5]]);
    assert_eq!((out[0][0], out[1][0]), (1.0, 0.5));
}

/// `settle` lands the drive on its target at once: the block after is the held curve.
#[test]
fn settle_lands_on_the_target() {
    let mut bank = landed(ShapeKind::Softsign, 1.0);
    bank.aim(&WaveshaperControls { drive: Drive(3.0) });
    bank.settle();
    let out = run(&mut bank, None, &[vec![1.0]]);
    assert_eq!(out[0][0], 0.75);
}
