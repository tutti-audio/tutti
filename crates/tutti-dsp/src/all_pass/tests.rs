use super::*;
use core::f64::consts::TAU;
use core::num::NonZeroUsize;

fn lattice(delay: usize, gain: f64, len: usize) -> Vec<f64> {
    let mut ap = AllPassFilter::<f64>::new(NonZeroUsize::new(delay).unwrap(), Feedback::new(gain));
    let mut h = vec![0.0; len];
    h[0] = 1.0;
    ap.process(&mut h);
    h
}

/// The lattice all-pass has `|H| = 1` at every frequency: its impulse response, cut where
/// `g^k` is below 1e-30, has a DTFT of unit magnitude at frequencies on and between the
/// multiples of `fs / D`.
#[test]
fn the_lattice_all_pass_has_unit_magnitude_at_every_frequency() {
    let (d, g) = (7usize, 0.7);
    let h = lattice(d, g, 200 * d);
    for w in [
        0.0,
        0.013,
        0.4,
        TAU / 7.0,
        1.0,
        2.0,
        3.0,
        core::f64::consts::PI,
    ] {
        let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
            let a = w * n as f64;
            (re + x * a.cos(), im - x * a.sin())
        });
        assert!(
            (re.hypot(im) - 1.0).abs() < 1e-12,
            "w = {w}: {}",
            re.hypot(im)
        );
    }
}

/// The lattice all-pass's impulse response is `-g` at frame 0 and `(1 - g^2) g^(k-1)` at
/// `k D`, from `H(z) = (z^-D - g) / (1 - g z^-D)`.
#[test]
fn the_lattice_impulse_response_is_minus_g_then_one_minus_g_squared_times_powers() {
    let (d, g) = (6usize, 0.6);
    let h = lattice(d, g, 8 * d + 1);
    assert_eq!(h[0], -g);
    for k in 1..=8usize {
        let want = (1.0 - g * g) * g.powi(k as i32 - 1);
        assert!((h[k * d] - want).abs() < 1e-15, "frame {}", k * d);
    }
    assert!((0..h.len()).filter(|n| n % d != 0).all(|n| h[n] == 0.0));
}

/// At DC the group delay of the lattice is `D (1 + g) / (1 - g)` frames: the response's phase
/// slope, `-d arg H / d w`, by a symmetric difference of the DTFT's phase around 0.
#[test]
fn the_lattice_group_delay_at_dc_is_d_times_one_plus_g_over_one_minus_g() {
    let (d, g) = (5usize, 0.5);
    let h = lattice(d, g, 200 * d);
    let phase = |w: f64| {
        let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
            (re + x * (w * n as f64).cos(), im - x * (w * n as f64).sin())
        });
        im.atan2(re)
    };
    let eps = 1e-4;
    let delay = -(phase(eps) - phase(-eps)) / (2.0 * eps);
    let want = d as f64 * (1.0 + g) / (1.0 - g);
    // The central difference errs by `tau'' eps^2 / 6`, 3e-6 here.
    assert!((delay - want).abs() < 1e-5, "{delay} vs {want}");
}

/// The delay is the frames asked for: one frame is one frame, and there is no zero to
/// refuse because the argument is a `NonZeroUsize`.
#[test]
fn a_one_frame_delay_is_one_frame_and_the_response_has_its_closed_form() {
    let ap = AllPassFilter::<f64>::new(NonZeroUsize::MIN, Feedback::new(0.5));
    assert_eq!(ap.delay(), Samples::new(1));
    // Lattice `(z^-1 - g) / (1 - g z^-1)` at `g = 1/2`: `h[0] = -g`, `h[n] = (1 - g^2) g^(n-1)`.
    let h = lattice(1, 0.5, 4);
    let want = [-0.5, 0.75, 0.375, 0.1875];
    for (n, (got, want)) in h.iter().zip(want).enumerate() {
        assert!((got - want).abs() < 1e-15, "h[{n}] = {got}, want {want}");
    }
}
