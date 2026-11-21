use super::*;
use tutti_buffers::InputOutput;
use tutti_buffers::{Planes, PlanesMut};

fn run(crusher: &mut BitCrusher<f64>, input: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; input[0].len()]; input.len()];
    crusher
        .process(
            &mut InputOutput::paired(
                Planes::from_vecs(input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    out
}

/// The whole stream through `crusher` in blocks of `block` frames.
fn run_blocks(crusher: &mut BitCrusher<f64>, input: &[Vec<f64>], block: usize) -> Vec<Vec<f64>> {
    let mut out = vec![Vec::new(); input.len()];
    for start in (0..input[0].len()).step_by(block) {
        let end = (start + block).min(input[0].len());
        let chunk: Vec<Vec<f64>> = input.iter().map(|c| c[start..end].to_vec()).collect();
        for (o, r) in out.iter_mut().zip(run(crusher, &chunk)) {
            o.extend(r);
        }
    }
    out
}

fn crusher(bits: f64, hold: f64) -> BitCrusher<f64> {
    BitCrusher::<f64>::new(
        ChannelLayout::MONO,
        BitCrusherControls {
            bits: BitDepth::new(bits),
            hold: HoldFactor::new(hold),
        },
    )
}

/// Multiples of 2^-10 spread over `-0.5..0.5`, each exact at 32 bits (a multiple of
/// 2^-31), distinct from their neighbours.
fn exact_input(frames: usize) -> Vec<f64> {
    (0..frames)
        .map(|k| ((k * 37) % 1000) as f64 / 1024.0 - 0.5)
        .collect()
}

/// `round(x q) / q` at whole depths `b`, `q = 2^(b-1)`: 1, 4, 8 and 16 bits are `q` of 1, 8,
/// 128 and 32768, ties to even, a value past `+-1` held at the end.
#[test]
fn the_output_is_round_x_q_over_q_at_whole_depths() {
    let x: Vec<f64> = (-1300..=1300).map(|i| f64::from(i) / 1000.0).collect();
    for (bits, q) in [(1.0, 1.0), (4.0, 8.0), (8.0, 128.0), (16.0, 32_768.0)] {
        let out = run(&mut crusher(bits, 1.0), std::slice::from_ref(&x));
        for (&x, &y) in x.iter().zip(&out[0]) {
            assert!(
                (y - x.clamp(-1.0, 1.0)).abs() <= 0.5 / q + 1e-12,
                "{bits} bits at {x}"
            );
        }
    }
}

/// Literal figures at each whole depth, `round(x q) / q` worked by hand: 1 bit (`q = 1`)
/// has the levels -1, 0 and 1; 4 bits (`q = 8`) steps of 0.125; 8 bits (`q = 128`) of
/// 1/128; 16 bits (`q = 32768`) of 2^-15. A value past `+-1` is held at the end level.
#[test]
fn the_output_matches_hand_worked_figures() {
    let cases: [(f64, &[(f64, f64)]); 4] = [
        (1.0, &[(0.4, 0.0), (0.6, 1.0), (-0.6, -1.0), (1.3, 1.0)]),
        (
            4.0,
            &[
                (0.3, 0.25),
                (0.4, 0.375),
                (0.9, 0.875),
                (-0.55, -0.5),
                (1.3, 1.0),
                (-1.3, -1.0),
            ],
        ),
        (
            8.0,
            &[(0.5, 0.5), (0.3, 38.0 / 128.0), (-0.7, -90.0 / 128.0)],
        ),
        (16.0, &[(0.3, 9830.0 / 32768.0), (-0.1, -3277.0 / 32768.0)]),
    ];
    for (bits, rows) in cases {
        let x: Vec<f64> = rows.iter().map(|r| r.0).collect();
        let out = run(&mut crusher(bits, 1.0), &[x]);
        for (&(x, want), &y) in rows.iter().zip(&out[0]) {
            assert_eq!(y, want, "{bits} bits at {x}");
        }
    }
}

/// Rounding is half to even: at 4 bits (`q = 8`) `0.0625 q = 0.5` goes to 0, `0.1875 q = 1.5`
/// to 2 and `0.3125 q = 2.5` to 2, where half away from zero would give 1, 2 and 3.
#[test]
fn ties_round_to_even() {
    let x = vec![0.0625, 0.1875, 0.3125, -0.0625, -0.1875, -0.3125];
    let out = run(&mut crusher(4.0, 1.0), &[x]);
    assert_eq!(out[0], [0.0, 0.25, 0.25, 0.0, -0.25, -0.25]);
}

/// A fractional depth has the real `q = 2^(b-1)`: at 3.5 bits `q = 2^2.5`, so 0.5 is
/// `round(2.828) / q = 3 / q`.
#[test]
fn a_fractional_depth_has_a_real_q() {
    let q = 5.656_854_249_492_381;
    let out = run(&mut crusher(3.5, 1.0), &[vec![0.5, -0.5, 0.2, 0.9]]);
    assert!((out[0][0] - 3.0 / q).abs() < 1e-15);
    assert!((out[0][1] + 3.0 / q).abs() < 1e-15);
    assert!((out[0][2] - 1.0 / q).abs() < 1e-15);
    assert!((out[0][3] - 5.0 / q).abs() < 1e-15);
}

/// The distinct levels of a ramp over `-1..=1`, spaced finer than a step. A whole depth
/// `b` has the integers `j` in `-q..=q`: `2q + 1 = 2^b + 1` levels. At 3.5 bits (`q = 5.657`)
/// the `j` in `-5..=5` are inside `+-1`, and `+-6` round past it and clamp to the ends `+-1`:
/// 11 + 2 = 13 levels.
#[test]
fn a_full_ramp_has_two_to_the_bits_plus_one_levels() {
    let ramp: Vec<f64> = (0..=4096).map(|i| -1.0 + f64::from(i) / 2048.0).collect();
    let distinct = |bits: f64| {
        let mut out = run(&mut crusher(bits, 1.0), std::slice::from_ref(&ramp)).remove(0);
        out.sort_by(f64::total_cmp);
        out.dedup();
        out.len()
    };
    for bits in 1..=6u32 {
        assert_eq!(
            distinct(f64::from(bits)),
            (1usize << bits) + 1,
            "{bits} bits"
        );
    }
    assert_eq!(distinct(3.5), 13);
}

/// 32 bits keeps every sample within `0.5 / q = 2^-32` of the input, and exactly where the
/// `f32` input is a multiple of `2^-31` (every magnitude from `2^-8` up).
#[test]
fn thirty_two_bits_is_the_identity_within_the_step() {
    let x: Vec<f32> = (0..4000).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
    let mut c = BitCrusher::<f32>::new(
        ChannelLayout::MONO,
        BitCrusherControls {
            bits: BitDepth::MAX,
            hold: HoldFactor::NONE,
        },
    );
    let mut out = vec![vec![0.0f32; x.len()]];
    c.process(
        &mut InputOutput::paired(
            Planes::from_vecs(std::slice::from_ref(&x)).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut small = 0;
    for (&x, &y) in x.iter().zip(&out[0]) {
        assert!((f64::from(y) - f64::from(x)).abs() <= 2f64.powi(-32), "{x}");
        if x.abs() >= 2f32.powi(-8) {
            assert_eq!(y, x);
        } else {
            small += 1;
        }
    }
    assert!(small > 0, "the input reaches the sub-2^-8 range");
}

/// A depth outside `1..=32` reads at the nearest end and one that is not finite as 8 bits.
#[test]
fn a_depth_is_read_within_its_range() {
    let x = 0.3;
    assert_eq!(quantize(x, BitDepth::new(0.2)), quantize(x, BitDepth::MIN));
    assert_eq!(quantize(x, BitDepth::new(99.0)), quantize(x, BitDepth::MAX));
    assert_eq!(
        quantize(x, BitDepth::new(f64::NAN)),
        quantize(x, DEFAULT_BIT_DEPTH)
    );
    assert_eq!(
        quantize(x, BitDepth::new(f64::INFINITY)),
        quantize(x, DEFAULT_BIT_DEPTH)
    );
    assert_eq!(quantize(f64::INFINITY, DEFAULT_BIT_DEPTH), 1.0);
    assert!(quantize(f64::NAN, DEFAULT_BIT_DEPTH).is_nan());
}

/// No hold is the identity of the hold stage: at 32 bits a multiple of `2^-10` passes exactly.
#[test]
fn a_hold_of_one_holds_nothing() {
    let x = exact_input(300);
    let out = run(&mut crusher(32.0, 1.0), std::slice::from_ref(&x));
    assert_eq!(out[0], x);
}

/// Hold `N` repeats input frame `floor(k / N) N` for `N` frames: output `k` is input
/// `(k / N) * N`, at 32 bits where the quantizer leaves the inputs alone.
#[test]
fn each_value_is_held_for_n_frames() {
    let x = exact_input(300);
    for n in [2usize, 4, 5, 64] {
        let out = run(&mut crusher(32.0, n as f64), std::slice::from_ref(&x));
        for (k, &y) in out[0].iter().enumerate() {
            assert_eq!(y, x[(k / n) * n], "hold {n}, frame {k}");
        }
    }
}

/// The count of frames since a capture continues across blocks: blocks of 1, 7 and 64 give
/// the per-frame formula, at holds that no block is a multiple of.
#[test]
fn the_hold_counter_is_continuous_across_blocks() {
    let x = exact_input(300);
    for n in [4usize, 5, 9] {
        for block in [1usize, 7, 64] {
            let out = run_blocks(
                &mut crusher(32.0, n as f64),
                std::slice::from_ref(&x),
                block,
            );
            for (k, &y) in out[0].iter().enumerate() {
                assert_eq!(y, x[(k / n) * n], "hold {n}, block {block}, frame {k}");
            }
        }
    }
}

/// Every channel captures on the same frames, from a hold that is not a multiple of a block.
#[test]
fn every_channel_holds_on_the_same_frames() {
    let left = exact_input(100);
    let right: Vec<f64> = left.iter().rev().copied().collect();
    let mut c = BitCrusher::<f64>::new(
        ChannelLayout::STEREO,
        BitCrusherControls {
            bits: BitDepth::MAX,
            hold: HoldFactor::new(6.0),
        },
    );
    let out = run_blocks(&mut c, &[left.clone(), right.clone()], 7);
    for k in 0..100 {
        assert_eq!(out[0][k], left[(k / 6) * 6], "left {k}");
        assert_eq!(out[1][k], right[(k / 6) * 6], "right {k}");
    }
}

/// The factor is read rounded to the nearest whole, ties to even: 2.4 holds 2 frames, 2.6
/// holds 3, 2.5 holds 2, 3.5 holds 4, and a factor of 1 or less, or not finite, holds 1.
#[test]
fn the_hold_factor_is_read_rounded() {
    for (factor, frames) in [
        (1.0, 1),
        (1.49, 1),
        (1.5, 2),
        (2.4, 2),
        (2.5, 2),
        (2.6, 3),
        (3.5, 4),
        (64.0, 64),
        (0.2, 1),
        (f64::NAN, 1),
        (f64::INFINITY, usize::MAX),
        (f64::MAX, f64::MAX as usize),
    ] {
        assert_eq!(hold_frames(HoldFactor::new(factor)), frames, "{factor}");
    }
    let x = exact_input(60);
    for (factor, n) in [(2.4f64, 2usize), (2.6, 3)] {
        let out = run(&mut crusher(32.0, factor), std::slice::from_ref(&x));
        for (k, &y) in out[0].iter().enumerate() {
            assert_eq!(y, x[(k / n) * n], "factor {factor}, frame {k}");
        }
    }
}

/// The held value is the input, quantized each frame after it is held: a depth that moves
/// mid-hold requantizes the same held input (0.3 is 0.25 at 4 bits and 0 at 1 bit).
#[test]
fn the_held_input_is_quantized_after_the_hold() {
    let mut c = crusher(4.0, 4.0);
    let first = run(&mut c, &[vec![0.3, 0.9]]);
    assert_eq!(first[0], [0.25, 0.25]);
    c.retarget(&BitCrusherControls {
        bits: BitDepth::MIN,
        hold: HoldFactor::new(4.0),
    });
    let second = run(&mut c, &[vec![0.9, 0.9, 0.9]]);
    assert_eq!(
        second[0],
        [0.0, 0.0, 1.0],
        "the held 0.3 at 1 bit, then the capture of 0.9"
    );
}

/// A hold that grows mid-hold captures nothing until the new count of frames has passed
/// since the last capture: held at 2, then 6 after 3 frames, the next capture is at frame 6.
#[test]
fn a_longer_hold_waits_for_the_new_count() {
    let mut c = crusher(32.0, 2.0);
    let x: Vec<f64> = (0..12).map(|k| f64::from(k) / 16.0).collect();
    let mut out = run(&mut c, &[x[..3].to_vec()]).remove(0);
    // Captures at frames 0 and 2.
    assert_eq!(out, [0.0, 0.0, 2.0 / 16.0]);
    c.retarget(&BitCrusherControls {
        bits: BitDepth::MAX,
        hold: HoldFactor::new(6.0),
    });
    out.extend(run(&mut c, &[x[3..].to_vec()]).remove(0));
    for (k, &y) in out.iter().enumerate().skip(2).take(4) {
        assert_eq!(y, 2.0 / 16.0, "frame {k} is still the capture at 2");
    }
    assert_eq!(
        out[8],
        8.0 / 16.0,
        "the next capture is six frames after frame 2"
    );
}

/// A hold that shrinks below the frames already held captures on the next frame, and a
/// reset makes the next frame capture.
#[test]
fn a_shorter_hold_and_a_reset_capture_at_once() {
    let mut c = crusher(32.0, 8.0);
    assert_eq!(run(&mut c, &[vec![0.5, 0.25, 0.125, 0.0625]])[0], [0.5; 4]);
    c.retarget(&BitCrusherControls {
        bits: BitDepth::MAX,
        hold: HoldFactor::new(2.0),
    });
    assert_eq!(
        run(&mut c, &[vec![0.125, 0.25, 0.375]])[0],
        [0.125, 0.125, 0.375]
    );
    c.reset();
    assert_eq!(run(&mut c, &[vec![0.75, 0.5]])[0], [0.75, 0.75]);
}

/// At 32 bits (`q = 2^31`) a 29-bit signal, which `f32` cannot hold, passes an `f64` crusher
/// exactly: `x q` is an integer and `round(x q) / q` returns `x`.
#[test]
fn an_f64_crusher_at_thirty_two_bits_passes_a_twenty_nine_bit_signal_exactly() {
    use tutti_fixtures::signal::{assert_close_f64, wide_signal};
    let x = wide_signal(300);
    assert!(
        x.iter().any(|&v| v != f64::from(v as f32)),
        "the signal needs more than f32 holds"
    );
    let y = run(&mut crusher(32.0, 1.0), std::slice::from_ref(&x)).remove(0);
    assert_close_f64(&y, &x, 0.0);
}
