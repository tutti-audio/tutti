use super::*;

const ALL: [CosineWindow; 5] = [
    CosineWindow::HANN,
    CosineWindow::HAMMING,
    CosineWindow::BLACKMAN,
    CosineWindow::BLACKMAN_HARRIS,
    CosineWindow::RECTANGULAR,
];

#[test]
fn periodic_hann_starts_at_zero_and_peaks_at_the_centre() {
    let w = CosineWindow::HANN.coefficients(8);
    assert_eq!(w.len(), 8);
    assert!(w[0].abs() < 1e-15, "first point is zero");
    assert!((w[4] - 1.0).abs() < 1e-15, "centre reaches unity");
    // Periodic: the last point is NOT zero, which is what distinguishes it
    // from the symmetric form and what makes it tile.
    assert!(w[7] > 0.0);
}

/// **The COLA rule, checked in both directions.**
///
/// Flat at the derived overlap *and* rippling at half of it. The second half
/// is what makes the rule tight rather than merely safe: a `cola_overlap`
/// that returned 16 for everything would pass the first assertion and
/// needlessly refuse grids that reconstruct perfectly well. The ripple is in `f64`, so flat is
/// at rounding, `1e-12`.
#[test]
fn the_cola_rule_is_tight() {
    for w in ALL {
        let overlap = w.cola_overlap();
        let flat = cola_ripple(&w, 64, overlap);
        assert!(
            flat < 1e-12,
            "{:?}-term window is not flat at its own {overlap}x: {flat:.2e}",
            w.terms()
        );

        if overlap > 1 {
            let looser = cola_ripple(&w, 64, overlap / 2);
            assert!(
                looser > 1e-3,
                "{:?}-term window claims it needs {overlap}x, but {}x is \
                 already flat ({looser:.2e}) — the rule is too conservative",
                w.terms(),
                overlap / 2
            );
        }
    }
}

/// **`coherent_gain` is `a₀`, and `a₀` is the measured mean.**
///
/// Two sizes, because the claim is also that it does not depend on `n` —
/// which is the entire justification for it being a closed form that takes
/// no length.
#[test]
fn coherent_gain_is_the_measured_mean_at_any_size() {
    for w in ALL {
        for size in [64usize, 1024] {
            let measured = w.coefficients(size).iter().sum::<f64>() / size as f64;
            let claimed = w.coherent_gain();
            assert!(
                (measured - claimed).abs() < 1e-14,
                "{:?}-term at n={size}: claims {claimed}, measures {measured}",
                w.terms()
            );
        }
    }
}

/// The trait's *default* implementations must agree with the closed forms
/// that override them.
///
/// Without this the overrides could drift from the maths they claim to
/// shortcut, and nothing would notice — the defaults are the only
/// independent statement of what the two functionals mean.
#[test]
fn the_closed_forms_agree_with_numeric_integration() {
    /// A window with no closed forms, so the trait defaults run.
    struct Numeric(CosineWindow);
    impl Window for Numeric {
        fn at(&self, t: f64) -> f64 {
            self.0.at(t)
        }
    }

    for w in ALL {
        let numeric = Numeric(w);
        assert!(
            (numeric.coherent_gain() - w.coherent_gain()).abs() < 1e-12,
            "{:?}-term: integrated {} vs closed form {}",
            w.terms(),
            numeric.coherent_gain(),
            w.coherent_gain()
        );
        assert_eq!(
            numeric.cola_overlap(),
            w.cola_overlap(),
            "{:?}-term: searched overlap disagrees with the derived one",
            w.terms()
        );
    }
}

/// Every window peaks at or below unity and never goes negative — one that
/// overshot would amplify the frame it was meant to taper.
///
/// Blackman-Harris is the one that could plausibly dip: it has the most
/// subtracting terms.
#[test]
fn no_window_overshoots_or_goes_negative() {
    for w in ALL {
        for &v in w.coefficients(128).iter() {
            assert!(
                (-1e-15..=1.0 + 1e-15).contains(&v),
                "{:?}-term produced {v}, outside [0, 1]",
                w.terms()
            );
        }
    }
}

/// `centred` is `at` shifted half a period, in `f64`: the same window,
/// peak at 0, zero at `±0.5` for the zero-ended ones.
#[test]
fn centred_is_the_periodic_window_shifted_half_a_period() {
    for w in [
        CosineWindow::HANN,
        CosineWindow::HAMMING,
        CosineWindow::BLACKMAN,
    ] {
        for i in 0..=40 {
            let x = f64::from(i) / 40.0 - 0.5;
            let periodic = w.at(x + 0.5);
            assert!((w.centred(x) - periodic).abs() < 1e-14, "{w:?} at {x}");
        }
    }
    assert!(CosineWindow::BLACKMAN.centred(0.5).abs() < 1e-15);
    assert!(CosineWindow::BLACKMAN.centred(-0.5).abs() < 1e-15);
    assert!((CosineWindow::BLACKMAN.centred(0.0) - 1.0).abs() < 1e-15);
}

/// `RECTANGULAR` is exactly the identity, so a caller holding pre-windowed
/// samples can say so rather than passing a window that almost does nothing.
#[test]
fn rectangular_is_the_identity() {
    assert_eq!(CosineWindow::RECTANGULAR.coefficients(5), vec![1.0; 5]);
    assert_eq!(CosineWindow::RECTANGULAR.coherent_gain(), 1.0);
}

#[test]
fn degenerate_sizes_do_not_divide_by_zero() {
    for w in ALL {
        assert!(w.coefficients(0).is_empty(), "{:?}-term at 0", w.terms());
        assert_eq!(w.coefficients(1).len(), 1, "{:?}-term at 1", w.terms());
        assert!(
            w.coefficients(1)[0].is_finite(),
            "{:?}-term at size 1 is not finite",
            w.terms()
        );
    }
}

/// Equality is by coefficients, not by slice address — otherwise two
/// identical windows built at different call sites would compare unequal,
/// and `StftGeometry`'s `PartialEq` would report a grid as changed when
/// nothing about it had.
#[test]
fn windows_compare_by_their_coefficients() {
    assert_eq!(CosineWindow::new(&[0.5, 0.5]), CosineWindow::HANN);
    assert_ne!(CosineWindow::HANN, CosineWindow::HAMMING);
}

/// Blackman-Harris squared is the square of the four-term window at every
/// phase, from the published coefficients 0.35875, 0.48829, 0.14128, 0.01168:
/// at `n = 64` the peak (`x = 32`) is `(a + b + c + d)² = 1`, the start is
/// `(a - b + c - d)² ≈ 3.6e-9`, and a quarter in, where the cosines at `π/2`
/// leave `(a - c)²`, it is `0.21747² = 0.047293`.
#[test]
fn blackman_harris_squared_is_the_square_of_blackman_harris() {
    let n = 64;
    let w = CosineWindow::BLACKMAN_HARRIS_SQUARED.coefficients(n);
    assert_eq!(CosineWindow::BLACKMAN_HARRIS_SQUARED.terms(), 7);
    assert!((w[n / 2] - 1.0).abs() < 1e-14, "peak {}", w[n / 2]);
    assert!(w[0] < 1e-8, "start {}", w[0]);
    assert!((w[n / 4] - 0.047_293).abs() < 1e-6, "quarter {}", w[n / 4]);
    for (x, &got) in w.iter().enumerate() {
        let t = std::f64::consts::TAU * x as f64 / n as f64;
        let bh =
            0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos();
        assert!((got - bh * bh).abs() < 1e-14, "tap {x}");
    }
}

/// `I0(0) = 1`; `I0(1)` and `I0(5)` are Abramowitz & Stegun table 9.8.
#[test]
fn bessel_i0_matches_known_values() {
    assert_eq!(bessel_i0(0.0), 1.0);
    assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-14);
    assert!((bessel_i0(5.0) - 27.239_871_823_604_442).abs() < 1e-11);
}

/// `I0(0) = 1` makes `beta = 0` the constant 1, the rectangular window.
#[test]
fn kaiser_at_beta_zero_is_rectangular() {
    let k = KaiserWindow::new(0.0);
    for x in [-0.5, -0.25, 0.0, 0.1, 0.5] {
        assert_eq!(k.centred(x), 1.0);
    }
}

/// `w(0) = 1` at the centre and `w(±1/2) = 1 / I0(beta)`, with `I0(5)` from A&S table 9.8.
#[test]
fn kaiser_peaks_at_one_in_the_middle_and_ends_at_one_over_i0_of_beta() {
    let k = KaiserWindow::new(5.0);
    assert_eq!(k.centred(0.0), 1.0);
    assert!((k.centred(0.5) - 1.0 / 27.239_871_823_604_442).abs() < 1e-14);
    assert!((k.centred(-0.5) - 1.0 / 27.239_871_823_604_442).abs() < 1e-14);
}

/// The squares of the `f32` Hann coefficients overlap-add at 4x to the constant `3/2`.
/// `w^2 = 3/8 - cos(t)/2 + cos(2t)/8`, and four quarter-period shifts cancel both cosines.
/// Each `f32` coefficient is off its design by at most half an ulp, `2^-25` for a value in
/// `[0.5, 1]` and less below, so a square is off by at most `2 * 2^-25` (`w <= 1`) and the
/// four overlapping squares by `4 * 2^-24 = 2^-22`. The sum is taken in `f64` over the
/// widened coefficients, so the accumulation adds nothing.
#[test]
fn f32_hann_squares_overlap_add_to_three_halves_within_the_rounding_of_the_taps() {
    let size = 2048;
    let hop = size / 4;
    let taps: Vec<f32> = crate::StftGeometry::new(
        tutti_units::SampleRate::SR_44K1,
        crate::WindowLength::new(tutti_units::Samples::new(size)).unwrap(),
        crate::HopSize::new(tutti_units::Samples::new(hop)).unwrap(),
    )
    .window_coefficients();
    let bound = 2f64.powi(-22);
    let mut worst = 0.0f64;
    for i in 0..hop {
        let sum: f64 = (0..4)
            .map(|k| f64::from(taps[i + k * hop]))
            .map(|w| w * w)
            .sum();
        worst = worst.max((sum - 1.5).abs());
    }
    assert!(worst < bound, "worst {worst:e} vs bound {bound:e}");
}
