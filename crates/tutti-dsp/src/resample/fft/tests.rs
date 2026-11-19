use super::*;
use crate::Window;
use tutti_buffers::Float;
use tutti_units::Normalized;

/// [`sinc_design`] at unity gain, each tap crossed once into `F`.
fn sinc_filter<F: Float>(npoints: usize, f_cutoff: Normalized) -> Vec<F> {
    sinc_design(npoints, f_cutoff, 1.0)
        .into_iter()
        .map(F::from_design)
        .collect()
}

/// The cutoff fit lands on the values rubato documents for it: 0.894 at 128
/// taps and 0.947 at 256, each within 0.001. Worked by hand from the fit's
/// constants, `1 / (1 + k1/n + k2/n^2 + k3/n^3)` at n = 128 is
/// `1 / (1 + 0.10738 + 0.00743 + 0.00284) = 0.8947`, so a slip in any of the
/// three coefficients moves it off the documented figure.
#[test]
fn the_cutoff_fit_matches_its_documented_values() {
    assert!(
        (cutoff(128).get() - 0.894).abs() < 0.001,
        "{}",
        cutoff(128).get()
    );
    assert!(
        (cutoff(256).get() - 0.947).abs() < 0.001,
        "{}",
        cutoff(256).get()
    );
}

/// The window sets the filter's response: a 256-tap, 0.9-cutoff filter is
/// 46 dB down at 0.93 of Nyquist (the squared window's wide transition; an
/// unsquared one is already at -105 dB there) and below -130 dB from 0.96
/// to Nyquist (the four-term window's sidelobes; a three-term one is at
/// -113 dB). Read as the magnitude of the taps' DFT in f64.
#[test]
fn the_filter_has_the_squared_windows_transition_and_stopband() {
    let n = 256;
    let taps = sinc_filter::<f32>(n, Normalized::clamped(0.9));
    let gain_db = |rel: f64| {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &t) in taps.iter().enumerate() {
            let w = std::f64::consts::PI * rel * (i as f64 - (n / 2) as f64);
            re += t as f64 * w.cos();
            im += t as f64 * w.sin();
        }
        20.0 * re.hypot(im).log10()
    };
    let edge = gain_db(0.93);
    assert!((-60.0..-35.0).contains(&edge), "0.93 Nyquist: {edge:.1} dB");
    for rel in [0.96, 0.97, 0.98, 0.99, 1.0] {
        let db = gain_db(rel);
        assert!(db < -130.0, "{rel} Nyquist: {db:.1} dB");
    }
}

/// The filter sums to 1 (DC gain 1), is symmetric about tap `n / 2`, and
/// peaks there.
#[test]
fn the_sinc_has_unity_dc_gain_and_peaks_at_its_centre() {
    let taps = sinc_filter::<f32>(441, Normalized::clamped(0.9));
    let sum: f32 = taps.iter().sum();
    assert!((sum - 1.0).abs() < 1e-5, "sum {sum}");
    let peak = taps
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .unwrap()
        .0;
    assert_eq!(peak, 441 / 2);
}

/// The block sizes are the rate pair's repeating unit times a count, so the
/// ratio is exact: 44.1 -> 48 k is 147 -> 160 per unit, and 1024 in 2
/// sub-chunks asks for 512 -> 4 units = 588 -> 640.
#[test]
fn block_sizes_are_whole_units_of_the_rate_pair() {
    let r = FftResampler::<f32>::new(44_100, 48_000, 1024, 2, 1).unwrap();
    assert_eq!((r.fft_size_in, r.fft_size_out), (588, 640));
    assert_eq!(r.output_delay(), 320);
    let r = FftResampler::<f32>::new(48_000, 44_100, 1024, 2, 1).unwrap();
    assert_eq!((r.fft_size_in, r.fft_size_out), (640, 588));
}

/// The block cap admits every real pair and refuses a runaway one. The
/// pairs are the common rates both ways, the coprime ones at the top of the
/// range (192000 <-> 44101, 44100 <-> 48001), and the largest preset-sized
/// block (8000 -> 192000 at a 32768-frame chunk in one piece, 786432
/// frames); 1 -> 192000 would plan an FFT of about 98 M points and is an
/// error, not an 800 MB allocation.
#[test]
fn the_block_cap_admits_real_pairs_and_refuses_a_runaway_one() {
    let rates = [
        8_000, 11_025, 22_050, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 44_101, 48_001,
    ];
    let ok = |a: usize, b: usize, chunk: usize, sub: usize| {
        let r = FftResampler::<f32>::new(a, b, chunk, sub, 1);
        assert!(r.is_ok(), "{a} -> {b} at {chunk}/{sub}: {:?}", r.err());
    };
    for &a in &rates {
        for &b in &rates {
            ok(a, b, 1024, 2);
        }
    }
    for (a, b) in [(8_000, 192_000), (192_000, 44_101), (44_101, 192_000)] {
        for (chunk, sub) in [(512, 1), (4096, 8), (32_768, 32), (32_768, 1)] {
            ok(a, b, chunk, sub);
        }
    }
    let err = FftResampler::<f32>::new(1, 192_000, 1024, 2, 1)
        .err()
        .unwrap();
    assert!(err.to_string().contains("block"), "{err}");
}

/// The block size is computed with checked arithmetic. Two units of
/// `2^(w - 1)` frames, `w` the pointer width, make `2^w`, which wraps to 0 and
/// would slip under the cap if the product were wrapping.
#[test]
fn an_overflowing_block_size_is_an_error() {
    let half = usize::MAX / 2 + 1;
    assert!(FftResampler::<f32>::new(1, half, 2, 1, 1).is_err());
}

/// A zero rate is refused, not divided by.
#[test]
fn a_zero_rate_is_an_error() {
    assert!(FftResampler::<f32>::new(0, 48_000, 1024, 2, 1).is_err());
    assert!(FftResampler::<f32>::new(44_100, 0, 1024, 2, 1).is_err());
}

/// The magnitude of `taps`' DFT in dB at `rel` times Nyquist, tap `n / 2`
/// taken as the time origin so a symmetric filter has a real response.
fn response_db(taps: &[f32], rel: f64) -> f64 {
    let mid = (taps.len() / 2) as f64;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &t) in taps.iter().enumerate() {
        let w = std::f64::consts::PI * rel * (i as f64 - mid);
        re += f64::from(t) * w.cos();
        im += f64::from(t) * w.sin();
    }
    20.0 * re.hypot(im).log10()
}

/// The resampler's filter against its design, at `n = 256` taps and cutoff
/// 0.5, read as the DFT of the taps in `f64`:
///
/// - the passband (up to 0.4 of Nyquist) is flat within 0.01 dB;
/// - the cutoff is the half-amplitude point, -6.02 dB within 0.5 dB, which is
///   where a windowed sinc crosses its ideal edge;
/// - beyond the main lobe of the squared four-term window (it convolves two
///   four-bin lobes, so it ends `2 * 4 = 8` bins out, `2 * 8 / n` of Nyquist)
///   the response is at the floor of the taps' `f32` narrowing. The window's
///   own highest sidelobe is twice Blackman-Harris's published -92 dB, -184 dB,
///   far under that floor, so the floor is what is left: each tap is off by at
///   most `2^-24` of itself, so the response is off by at most
///   `2^-24 * sum|t|` (the taps sum to 1), which the test computes from the
///   taps and bounds the stopband by;
/// - the transition is the ideal step convolved with the window's main lobe: at
///   0.5, 1 and 2 bins either side of the cutoff the amplitude is 0.5 minus or
///   plus the window spectrum's integral from the cutoff, over its integral
///   across the lobe (the test integrates the window's DFT), within 0.002.
#[test]
fn the_filter_passes_to_its_cutoff_and_stops_at_the_taps_precision_floor() {
    let n = 256usize;
    let fc = 0.5f64;
    let taps = sinc_filter::<f32>(n, Normalized::clamped(fc));

    for k in 0..=40 {
        let rel = f64::from(k) / 40.0 * (fc - 0.1);
        let db = response_db(&taps, rel);
        assert!(db.abs() < 0.01, "{rel:.3} Nyquist: {db:.4} dB");
    }
    let edge = response_db(&taps, fc);
    assert!((edge + 6.02).abs() < 0.5, "cutoff: {edge:.2} dB");

    let sidelobe_db = 2.0 * -92.0;
    let abs_sum: f64 = taps.iter().map(|&t| f64::from(t).abs()).sum();
    let floor_db = 20.0 * (2.0f64.powi(-24) * abs_sum).log10();
    let bound = floor_db.max(sidelobe_db);
    let lobe = 2.0 * 8.0 / n as f64;
    let mut worst = f64::MIN;
    let mut rel = fc + lobe;
    while rel <= 1.0 {
        worst = worst.max(response_db(&taps, rel));
        rel += 0.002;
    }
    assert!(
        worst < bound,
        "stopband worst {worst:.1} dB over {:.3}..1 Nyquist, bound {bound:.1} dB",
        fc + lobe
    );

    // The transition is the ideal step convolved with the window's main lobe,
    // so at `d` bins from the cutoff the amplitude is `0.5 +/- int_0^d A / int A`,
    // `A(b) = sum_i w_i cos(2 pi b (i - n/2) / n)` the window's spectrum, and
    // the integral over the whole lobe (+/- 8 bins) normalises it.
    let w = CosineWindow::BLACKMAN_HARRIS_SQUARED.coefficients(n);
    let spectrum = |b: f64| -> f64 {
        w.iter()
            .enumerate()
            .map(|(i, &v)| {
                let t = std::f64::consts::TAU * b * (i as f64 - (n / 2) as f64) / n as f64;
                v * t.cos()
            })
            .sum()
    };
    let integral = |from: f64, to: f64| -> f64 {
        let steps = 4000;
        let h = (to - from) / f64::from(steps);
        (0..steps)
            .map(|k| spectrum(from + (f64::from(k) + 0.5) * h) * h)
            .sum()
    };
    let total = integral(-8.0, 8.0);
    for offset in [0.5f64, 1.0, 2.0] {
        let rel = offset * 2.0 / n as f64;
        let step = integral(0.0, offset) / total;
        for (at, want) in [(fc + rel, 0.5 - step), (fc - rel, 0.5 + step)] {
            let got = 10.0f64.powf(response_db(&taps, at) / 20.0);
            assert!(
                (got - want).abs() < 0.002,
                "{offset} bins from the cutoff ({at:.4}): amplitude {got:.4}, lobe integral {want:.4}"
            );
        }
    }
}

/// At `f64` a constant input is a constant output of the same level once the
/// filter has filled: the low-pass has unity gain at DC, read in the
/// passband to far below `f32` precision.
#[test]
fn an_f64_resampler_passes_dc_at_unity_to_f64_precision() {
    let mut r = FftResampler::<f64>::new(44_100, 48_000, 1024, 2, 1).unwrap();
    let input = vec![vec![1.0f64; r.input_frames_next()]];
    let mut out = vec![vec![0.0f64; r.output_frames_max()]];
    let mut produced = 0;
    for _ in 0..6 {
        produced = r
            .process_into_buffer(
                Planes::from_vecs(&input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap();
    }
    assert!(produced > 0);
    for (i, &v) in out[0][..produced].iter().enumerate() {
        assert!((v - 1.0).abs() < 1e-10, "frame {i}: {v}");
    }
}

/// At `f32` a constant input is a constant output of the same level once the filter has
/// filled: the low-pass sums to 1 in the design (`f64`), so the passband gain is 1 up to the
/// `f32` rounding of the chain. A transform of `N = 2 * 640` points is `log2 N = 10.3` butterfly
/// stages, each adding a relative error of half an ulp (`2^-24`) in the worst case, forward and
/// inverse, and the filter product and the overlap add one more each: the bound is
/// `(2 * 11 + 2) * 2^-24`.
#[test]
fn an_f32_resampler_passes_dc_at_unity_to_the_transforms_rounding() {
    let mut r = FftResampler::<f32>::new(44_100, 48_000, 1024, 2, 1).unwrap();
    let input = vec![vec![1.0f32; r.input_frames_next()]];
    let mut out = vec![vec![0.0f32; r.output_frames_max()]];
    let mut produced = 0;
    for _ in 0..6 {
        produced = r
            .process_into_buffer(
                Planes::from_vecs(&input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap();
    }
    assert!(produced > 0);
    let bound = 24.0 * 2f64.powi(-24);
    let worst = out[0][..produced]
        .iter()
        .map(|&v| (f64::from(v) - 1.0).abs())
        .fold(0.0, f64::max);
    assert!(worst < bound, "worst {worst:e} vs bound {bound:e}");
}
