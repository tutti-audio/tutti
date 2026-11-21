//! The zero-latency convolver ([`Convolver::zero_latency`]): its output at frame `n` is the
//! full convolution at frame `n`, computed here by the definition.

use super::*;

/// The head partition the tests use: with IRs of a few partitions up to several thousand
/// taps, the tail is cut into levels too.
const HEAD: usize = 32;

/// `y[n] = sum_k h[k] x[n - k]` for `n < x.len()`, in `f64` (the definition, O(N * L)).
fn definition(ir: &[f32], x: &[f32]) -> Vec<f64> {
    (0..x.len())
        .map(|n| {
            (0..ir.len().min(n + 1))
                .map(|k| f64::from(ir[k]) * f64::from(x[n - k]))
                .sum()
        })
        .collect()
}

/// The IR lengths that sit on and around the head's end and every level's first tap.
fn lengths() -> Vec<usize> {
    vec![1, HEAD - 1, HEAD, HEAD + 1, 3 * HEAD, 9 * HEAD + 5, 3000]
}

/// The largest difference between `got` and `want`.
fn worst(got: &[f32], want: &[f64]) -> f64 {
    got.iter()
        .zip(want)
        .map(|(&g, &w)| (f64::from(g) - w).abs())
        .fold(0.0, f64::max)
}

/// The round-off bound of the tail's FFT path (`tolerance`, over the largest transform) plus
/// the head's: `HEAD` products summed in `f32` are off by at most `HEAD * eps * sum |h_k x_k|`,
/// and by Cauchy-Schwarz `sum |h_k x_k| <= |h|_2 |x|_2`.
fn bound(ir: &[f32], x: &[f32]) -> f64 {
    let norm = |v: &[f32]| v.iter().map(|a| a * a).sum::<f32>().sqrt();
    let fft = f64::from(tolerance(ir, x, 2 * MAX_PARTITION));
    let head = f64::from(f32::EPSILON) * HEAD as f64 * f64::from(norm(ir) * norm(x));
    fft + head
}

/// An impulse gives the IR from frame 0, at every IR length.
#[test]
fn an_impulse_gives_the_ir_from_frame_zero() {
    for len in lengths() {
        let ir = flat_ir(len);
        let mut x = vec![0.0f32; len + 2 * HEAD];
        x[0] = 1.0;
        let mut c = Convolver::zero_latency(Mono::new(&ir), Samples::new(HEAD));
        let got = run_blocks(&mut c, &x, &[HEAD]);
        let mut want = vec![0.0f64; x.len()];
        for (w, &h) in want.iter_mut().zip(&ir) {
            *w = f64::from(h);
        }
        let err = worst(&got, &want);
        assert!(
            err <= bound(&ir, &x),
            "len {len}: the impulse response is off by {err}"
        );
    }
}

/// A noise input gives the definition's convolution at the same frame, at block sizes
/// one, one under, at, one over the head and past it, through the whole tail.
#[test]
fn noise_gives_the_direct_convolution_at_every_block_size() {
    for len in lengths() {
        let ir = flat_ir(len);
        let mut x = noise(1500);
        x.extend(std::iter::repeat_n(0.0, len));
        let want = definition(&ir, &x);
        for block in [1, HEAD - 1, HEAD, HEAD + 1, 4 * HEAD + 3] {
            let mut c = Convolver::zero_latency(Mono::new(&ir), Samples::new(HEAD));
            let got = run_blocks(&mut c, &x, &[block]);
            let err = worst(&got, &want);
            assert!(
                err <= bound(&ir, &x),
                "len {len}, block {block}: off the definition by {err}"
            );
        }
    }
}

/// The same in `f64`, where a head or a level that is wrong by one tap is thousands of
/// times past the round-off.
#[test]
fn an_f64_zero_latency_convolver_matches_the_definition() {
    let ir: Vec<f64> = flat_ir(1200).iter().map(|&v| f64::from(v)).collect();
    let x: Vec<f64> = noise(2000).iter().map(|&v| f64::from(v)).collect();
    let mut c = Convolver::<f64>::zero_latency(Mono::new(&ir), Samples::new(HEAD));
    let mut got = vec![0.0f64; x.len()];
    for (i, o) in x.chunks(37).zip(got.chunks_mut(37)) {
        c.process(Mono::new(i), MonoMut::new(o));
    }
    let norm = |v: &[f64]| v.iter().map(|a| a * a).sum::<f64>().sqrt();
    let bound = 2.0 * f64::EPSILON * ((2 * MAX_PARTITION) as f64).log2() * norm(&ir) * norm(&x);
    for (n, &g) in got.iter().enumerate() {
        let want: f64 = (0..ir.len().min(n + 1)).map(|k| ir[k] * x[n - k]).sum();
        assert!((g - want).abs() <= bound, "frame {n}: {g} against {want}");
    }
}

/// The latency is zero in this mode and the head partition in the default one.
#[test]
fn latency_is_zero_and_the_default_is_the_block() {
    for len in lengths() {
        let ir = flat_ir(len);
        let zero = Convolver::<f32>::zero_latency(Mono::new(&ir), Samples::new(HEAD));
        let default = Convolver::<f32>::new(Mono::new(&ir), Samples::new(HEAD));
        assert_eq!(zero.latency(), Samples::ZERO, "len {len}");
        assert_eq!(default.latency(), Samples::new(HEAD), "len {len}");
        assert_eq!(zero.ir_length(), Samples::new(len), "len {len}");
    }
}

/// The default mode keeps no head: its spectra cover every tap of the IR.
#[test]
fn the_default_mode_has_no_head() {
    let ir = flat_ir(500);
    let c = Convolver::<f32>::new(Mono::new(&ir), Samples::new(HEAD));
    assert!(c.spectra().head.is_none());
    let covered: usize = c.spectra().shape().iter().map(|&(part, n)| part * n).sum();
    assert!(covered >= ir.len());
}

/// A clone and a fresh convolver read the one set of head taps, in the same `Arc` as the
/// spectra, and the head holds exactly the IR's first `HEAD` taps.
#[test]
fn clones_and_fresh_convolvers_share_the_head_taps() {
    let ir = flat_ir(700);
    let c = Convolver::<f32>::zero_latency(Mono::new(&ir), Samples::new(HEAD));
    let (clone, fresh) = (c.clone(), c.fresh());
    assert!(Arc::ptr_eq(c.spectra(), clone.spectra()));
    assert!(Arc::ptr_eq(c.spectra(), fresh.spectra()));
    assert_eq!(Arc::strong_count(c.spectra()), 3);
    let taps = c.spectra().head.as_ref().expect("a head");
    assert_eq!(taps.len(), HEAD);
    // Reversed: the last stored tap is `h[0]`.
    assert_eq!(taps.reversed.last(), ir.first());
    assert_eq!(taps.reversed.first(), Some(&ir[HEAD - 1]));
    // The levels start at tap `HEAD`: their first segment is the transform of `ir[HEAD..]`.
    let covered: usize = c.spectra().shape().iter().map(|&(part, n)| part * n).sum();
    assert!(covered >= ir.len() - HEAD && covered < ir.len() - HEAD + HEAD * MAX_TAIL_RATIO);
}

/// After `reset` the convolver answers an impulse as a fresh one does: from frame 0, with
/// nothing of the earlier input in the head's history.
#[test]
fn reset_clears_the_head_history() {
    let ir = flat_ir(300);
    let mut c = Convolver::<f32>::zero_latency(Mono::new(&ir), Samples::new(HEAD));
    run_blocks(&mut c, &noise(500), &[HEAD + 3]);
    c.reset();
    let mut x = vec![0.0f32; 400];
    x[0] = 1.0;
    let got = run_blocks(&mut c, &x, &[41]);
    let mut want = vec![0.0f64; x.len()];
    for (w, &h) in want.iter_mut().zip(&ir) {
        *w = f64::from(h);
    }
    assert!(worst(&got, &want) <= bound(&ir, &x));
}

/// An empty IR is silence, at zero latency.
#[test]
fn an_empty_ir_is_silence_at_zero_latency() {
    let mut c = Convolver::<f32>::zero_latency(Mono::new(&[] as &[f32]), Samples::new(HEAD));
    assert_eq!(c.latency(), Samples::ZERO);
    assert!(
        run_blocks(&mut c, &noise(100), &[7])
            .iter()
            .all(|&v| v == 0.0)
    );
}
