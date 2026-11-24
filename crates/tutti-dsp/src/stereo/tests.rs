use super::*;
use tutti_buffers::{BlockSide, InputOutput};
use tutti_buffers::{Planes, PlanesMut};

fn convert<S: Float>(dir: MidSide, l: &[S], r: &[S]) -> (Vec<S>, Vec<S>) {
    let input = vec![l.to_vec(), r.to_vec()];
    let mut out = vec![vec![S::ZERO; l.len()]; 2];
    dir.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let s = out.pop().unwrap();
    (out.pop().unwrap(), s)
}

/// Encoding is the half sum and the half difference: `(1, 0.5)` gives `(0.75, 0.25)`,
/// opposite signs `(1, -1)` give mid 0 and side 1, equal channels have no side.
#[test]
fn encode_is_the_half_sum_and_half_difference() {
    let (m, s) = convert(
        MidSide::Encode,
        &[1.0f64, 1.0, 0.25, 0.0],
        &[0.5, -1.0, 0.25, 0.0],
    );
    assert_eq!(m, [0.75, 0.0, 0.25, 0.0]);
    assert_eq!(s, [0.25, 1.0, 0.0, 0.0]);
}

/// Decoding is `L = M + S`, `R = M - S`.
#[test]
fn decode_is_the_sum_and_difference() {
    let (l, r) = convert(MidSide::Decode, &[0.75f64, 0.0], &[0.25, 1.0]);
    assert_eq!(l, [1.0, 1.0]);
    assert_eq!(r, [0.5, -1.0]);
}

fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
        })
        .collect()
}

/// The round trip is exact in `f64` where the sums are exact (values on a 2^-20 grid, the
/// edge values 0, +-1 and opposite signs among them), and within `EPSILON * max(|L|, |R|)`
/// for any input, in `f64` and in `f32`.
#[test]
fn decode_of_encode_is_exact_on_a_grid_and_within_epsilon_otherwise() {
    let grid = |v: f64| (v * 1_048_576.0).round() / 1_048_576.0;
    let mut l: Vec<f64> = noise(4096, 1).into_iter().map(grid).collect();
    let mut r: Vec<f64> = noise(4096, 2).into_iter().map(grid).collect();
    for (a, b) in [
        (1.0, 1.0),
        (1.0, -1.0),
        (-1.0, 1.0),
        (0.0, 0.0),
        (1.0, 0.0),
        (0.0, -1.0),
    ] {
        l.push(a);
        r.push(b);
    }
    let (m, s) = convert(MidSide::Encode, &l, &r);
    let (l2, r2) = convert(MidSide::Decode, &m, &s);
    assert_eq!((&l2, &r2), (&l, &r));

    let (l, r) = (noise(4096, 3), noise(4096, 4));
    let (m, s) = convert(MidSide::Encode, &l, &r);
    let (l2, r2) = convert(MidSide::Decode, &m, &s);
    for i in 0..l.len() {
        let bound = f64::EPSILON * l[i].abs().max(r[i].abs());
        assert!((l2[i] - l[i]).abs() <= bound && (r2[i] - r[i]).abs() <= bound);
    }

    let (l, r): (Vec<f32>, Vec<f32>) = (
        noise(4096, 5).into_iter().map(|x| x as f32).collect(),
        noise(4096, 6).into_iter().map(|x| x as f32).collect(),
    );
    let (m, s) = convert(MidSide::Encode, &l, &r);
    let (l2, r2) = convert(MidSide::Decode, &m, &s);
    for i in 0..l.len() {
        let bound = f32::EPSILON * l[i].abs().max(r[i].abs());
        assert!(
            (l2[i] - l[i]).abs() <= bound && (r2[i] - r[i]).abs() <= bound,
            "frame {i}: {} {} against {} {}",
            l2[i],
            r2[i],
            l[i],
            r[i]
        );
    }
}

/// A view with one channel is refused, and the output is left as it was.
#[test]
fn a_mono_view_is_refused_untouched() {
    let input = vec![vec![1.0f32; 4]];
    let mut out = vec![vec![7.0f32; 4]];
    let refused = MidSide::Encode.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
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
    assert_eq!(out[0], [7.0; 4]);
}

fn widen(width: f64, settle: bool, l: &[f64], r: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let mut bank = WidthBank::<f64>::new(SampleRate::SR_48K);
    bank.retarget(&WidthControls {
        width: StereoWidth::new(width),
    });
    if settle {
        bank.jump_to_target();
    }
    let input = vec![l.to_vec(), r.to_vec()];
    let mut out = vec![vec![0.0f64; l.len()]; 2];
    bank.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let r = out.pop().unwrap();
    (out.pop().unwrap(), r)
}

/// A settled width is `L = M + w S`, `R = M - w S`: 0 gives `L = R = (L + R) / 2`, 1
/// gives the input, 2 gives `M + 2 S` (so `(1, 0.5)` becomes `(1.25, 0.25)`).
#[test]
fn a_settled_width_is_mid_plus_width_times_side() {
    let (l, r) = ([1.0, 0.5, -0.25, 0.0], [0.5, 1.0, 0.25, 0.0]);
    let (a, b) = widen(0.0, true, &l, &r);
    assert_eq!(a, b);
    assert_eq!(a, [0.75, 0.75, 0.0, 0.0]);
    let (a, b) = widen(1.0, true, &l, &r);
    assert_eq!((a, b), (l.to_vec(), r.to_vec()));
    let (a, b) = widen(2.0, true, &l, &r);
    assert_eq!(a, [1.25, 0.25, -0.5, 0.0]);
    assert_eq!(b, [0.25, 1.25, 0.5, 0.0]);
}

/// The width ramps linearly over `GAIN_RAMP` and lands on the target exactly: frame `i`
/// of a side-only input reads `w(i + 1)` of the line from 1 to 0, and the frames past
/// the ramp read 0.
#[test]
fn the_width_ramps_linearly_and_lands_exactly() {
    let n = GAIN_RAMP.to_samples_ceil(SampleRate::SR_48K).get();
    let l = vec![1.0f64; n + 50];
    let r = vec![-1.0f64; n + 50];
    let (a, b) = widen(0.0, false, &l, &r);
    for i in 0..n {
        let want = 1.0 - (i + 1) as f64 / n as f64;
        assert!((a[i] - want).abs() < 1e-12, "frame {i}: {}", a[i]);
        assert_eq!(a[i], -b[i]);
    }
    assert!(a[n..].iter().all(|&y| y == 0.0));
}

/// A width that is not finite leaves the `retarget` as it was, and a negative one reads as 0.
#[test]
fn a_bad_width_is_refused_or_floored() {
    let (l, r) = ([1.0], [0.0]);
    assert_eq!(widen(f64::NAN, true, &l, &r).0, [1.0]);
    assert_eq!(widen(-3.0, true, &l, &r).0, [0.5]);
}

/// A view with one channel is refused, and the output is left as it was.
#[test]
fn the_width_bank_refuses_a_mono_view_untouched() {
    let input = vec![vec![1.0f32; 4]];
    let mut out = vec![vec![7.0f32; 4]];
    let refused = WidthBank::<f32>::new(SampleRate::SR_48K).process(
        &mut InputOutput::paired(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut out).unwrap(),
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
    assert_eq!(out[0], [7.0; 4]);
}

/// An `f64` bank at width 1 returns a 29-bit signal exactly, and at another width `w` is `M = (L + R) / 2` and `S = w (L - R) / 2` as `L' = M + S`,
/// `R' = M - S` in `f64`, to 2^-45.
#[test]
fn an_f64_bank_is_the_identity_at_unity_and_the_mid_side_matrix_at_other_widths() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let l = wide_signal(200);
    let r: Vec<f64> = l.iter().rev().map(|v| v * 0.75).collect();
    let (a, b) = widen(1.0, true, &l, &r);
    assert_close(&a, &l, 0.0);
    assert_close(&b, &r, 0.0);
    let w = 0.3;
    let (a, b) = widen(w, true, &l, &r);
    let mid: Vec<f64> = l.iter().zip(&r).map(|(l, r)| (l + r) / 2.0).collect();
    let side: Vec<f64> = l.iter().zip(&r).map(|(l, r)| w * (l - r) / 2.0).collect();
    let (want_l, want_r): (Vec<f64>, Vec<f64>) =
        mid.iter().zip(&side).map(|(m, s)| (m + s, m - s)).unzip();
    assert_close(&a, &want_l, 2.0f64.powi(-45));
    assert_close(&b, &want_r, 2.0f64.powi(-45));
}
