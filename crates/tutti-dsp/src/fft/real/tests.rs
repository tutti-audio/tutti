use super::*;
use crate::Complex32;

fn frame(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7 + 3) % 11) as f32 * 0.25 - 1.0)
        .collect()
}

/// The textbook DFT of bin `k`, in f64.
fn dft(frame: &[f32], k: usize) -> (f64, f64) {
    let n = frame.len();
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (t, &x) in frame.iter().enumerate() {
        let angle = -2.0 * PI * (k * t) as f64 / n as f64;
        re += x as f64 * angle.cos();
        im += x as f64 * angle.sin();
    }
    (re, im)
}

/// The transform pair against the textbook DFT, so the passes are checked
/// against an analytic figure and not against another FFT. Every size up
/// to 64, even and odd: the even ones include those that are not a
/// multiple of four (an odd bin count, a lone centre bin).
#[test]
fn the_transform_is_the_dft_for_every_size() {
    for n in 1..=64usize {
        let frame = frame(n);
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        for (k, bin) in bins.iter().enumerate() {
            let (re, im) = dft(&frame, k);
            assert!(
                (bin.re as f64 - re).abs() < 1e-4 && (bin.im as f64 - im).abs() < 1e-4,
                "size {n} bin {k}: {bin} != {re} + {im}i"
            );
        }
        assert_eq!(bins[0].im, 0.0, "size {n}: DC is real");
        if n % 2 == 0 {
            assert_eq!(bins[n / 2].im, 0.0, "size {n}: Nyquist is real");
        }

        let mut back = vec![0.0f32; n];
        fft.inverse(&mut bins, MonoMut::new(&mut back), &mut scratch);
        for (i, (&got, &want)) in back.iter().zip(&frame).enumerate() {
            assert!(
                (got / n as f32 - want).abs() < 1e-5,
                "size {n} sample {i}: {got} / {n} != {want}"
            );
        }
    }
}

/// Odd sizes the analysis kernels run (7, 255, 301) and a prime large
/// enough to leave the small butterflies, against the DFT.
#[test]
fn larger_odd_sizes_match_the_dft() {
    for n in [127usize, 255, 301] {
        let frame: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() + 0.1).collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        for (k, bin) in bins.iter().enumerate() {
            let (re, im) = dft(&frame, k);
            assert!(
                (bin.re as f64 - re).abs() < 2e-3 && (bin.im as f64 - im).abs() < 2e-3,
                "size {n} bin {k}: {bin} != {re} + {im}i"
            );
        }
    }
}

/// A non-zero imaginary DC or Nyquist is not part of a real signal: the
/// inverse must neither panic nor let those parts reach the signal. Any
/// non-zero value does this; NaN is the worst of them. Even and odd.
#[test]
fn the_inverse_ignores_dc_and_nyquist_imaginary_parts() {
    for n in [64usize, 63] {
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut tone = |im_dc: f32, im_ny: f32| {
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            bins[0] = Complex32::new(1.0, im_dc);
            bins[3] = Complex32::new(2.0, -1.0);
            if n % 2 == 0 {
                bins[n / 2] = Complex32::new(0.5, im_ny);
            }
            let mut out = vec![0.0f32; n];
            fft.inverse(&mut bins, MonoMut::new(&mut out), &mut scratch);
            out
        };
        let clean = tone(0.0, 0.0);
        assert_eq!(clean, tone(0.25, -0.75), "size {n}");
        // The DC bin alone contributes 1.0 to every sample's mean.
        let mean = clean.iter().sum::<f32>() / n as f32;
        assert!((mean - 1.0).abs() < 1e-5, "size {n}: {mean}");
        assert!(tone(f32::NAN, f32::INFINITY).iter().all(|x| x.is_finite()));
    }
}

/// The scratch is shared by both directions, so it must cover the larger
/// of the two plans' needs, at every size.
///
/// Not covered: rustfft 6.4's scalar out-of-place scratch length
/// is 0 at every size, so sizing it from one direction alone still passes.
/// This is a cheap guard for a future rustfft whose scratch is nonzero or
/// differs by direction; it cannot fail today.
#[test]
fn the_scratch_covers_both_directions() {
    // Under miri the powers of two stop at 2^10: planning a 32k transform
    // there takes tens of seconds and adds no new scratch rule.
    let top = if cfg!(miri) { 10 } else { 15 };
    for n in (1..=33usize).chain((6..=top).map(|p| 1usize << p)) {
        let fft = RealFft::<f32>::new(n);
        assert!(fft.scratch_len() >= fft.forward_scratch_len(), "size {n}");
        assert!(fft.scratch_len() >= fft.inverse_scratch_len(), "size {n}");
        assert_eq!(fft.new_scratch().len(), fft.scratch_len());
    }
}

/// A unit impulse at sample 0 has a flat spectrum: every bin is exactly
/// 1 + 0i, and the inverse of it returns `n` times the impulse.
#[test]
fn an_impulse_has_a_flat_spectrum() {
    for n in [8usize, 63, 255, 1024] {
        let mut frame = vec![0.0f32; n];
        frame[0] = 1.0;
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        for (k, bin) in bins.iter().enumerate() {
            assert!(
                (bin.re - 1.0).abs() < 1e-6 && bin.im.abs() < 1e-6,
                "size {n} bin {k}: {bin}"
            );
        }
        let mut back = vec![0.0f32; n];
        fft.inverse(&mut bins, MonoMut::new(&mut back), &mut scratch);
        assert!((back[0] / n as f32 - 1.0).abs() < 1e-6, "size {n}");
        assert!(back[1..].iter().all(|x| (x / n as f32).abs() < 1e-6));
    }
}

/// A cosine centred on bin `k` puts `n / 2` in bin `k` and nothing in any
/// other bin, at the sizes the analysis kernels run.
#[test]
fn a_bin_centred_cosine_lands_in_its_bin() {
    for (n, k) in [(1024usize, 37usize), (256, 5), (255, 20)] {
        let frame: Vec<f32> = (0..n)
            .map(|t| (2.0 * PI * (k * t) as f64 / n as f64).cos() as f32)
            .collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        let tol = 1e-3 * n as f32 / 2.0;
        for (b, bin) in bins.iter().enumerate() {
            let want = if b == k { n as f32 / 2.0 } else { 0.0 };
            assert!(
                (bin.re - want).abs() < tol && bin.im.abs() < tol,
                "size {n} bin {b}: {bin}, want {want}"
            );
        }
    }
}

/// Parseval: the energy of the frame is the energy of its spectrum, with
/// the interior bins counted twice (their conjugates are not stored).
#[test]
fn the_transform_preserves_energy() {
    for n in [64usize, 255, 1024] {
        let frame: Vec<f32> = (0..n)
            .map(|i| ((i * 7 + 3) % 11) as f32 * 0.25 - 1.0)
            .collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        let time: f64 = frame.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        let last = bins.len() - 1;
        let freq: f64 = bins
            .iter()
            .enumerate()
            .map(|(k, b)| {
                let e = f64::from(b.re).powi(2) + f64::from(b.im).powi(2);
                if k == 0 || (n % 2 == 0 && k == last) {
                    e
                } else {
                    2.0 * e
                }
            })
            .sum::<f64>()
            / n as f64;
        assert!(
            (time - freq).abs() < 1e-4 * time,
            "size {n}: {time} vs {freq}"
        );
    }
}

/// At `f64` a bin-centred cosine of amplitude 1 puts `size / 2` in bin `k`
/// and zero elsewhere, to far below `f32` precision, for an even and an odd
/// size.
#[test]
fn an_f64_cosine_lands_in_its_bin_at_f64_precision() {
    for n in [64usize, 63] {
        let k = 5;
        let frame: Vec<f64> = (0..n)
            .map(|t| (2.0 * PI * (k * t) as f64 / n as f64).cos())
            .collect();
        let fft = RealFft::<f64>::new(n);
        let mut scratch = fft.new_scratch();
        let mut bins = vec![Complex::new(0.0, 0.0); fft.bins()];
        fft.forward(Mono::new(&frame), &mut bins, &mut scratch);
        for (b, bin) in bins.iter().enumerate() {
            let want = if b == k { n as f64 / 2.0 } else { 0.0 };
            assert!(
                (bin.re - want).abs() < 1e-11 && bin.im.abs() < 1e-11,
                "size {n} bin {b}: {bin} != {want}"
            );
        }
        let mut back = vec![0.0f64; n];
        fft.inverse(&mut bins, MonoMut::new(&mut back), &mut scratch);
        for (i, (&got, &want)) in back.iter().zip(&frame).enumerate() {
            assert!((got / n as f64 - want).abs() < 1e-12, "size {n} sample {i}");
        }
    }
}
