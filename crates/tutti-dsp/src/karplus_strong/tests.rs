use super::*;
use crate::Complex64;
use crate::testing::lock_in;
use core::f64::consts::TAU;

const LN_1000: f64 = 3.0 * core::f64::consts::LN_10;

/// A string of `frequency` and `decay` at `rate`, lowest 50 Hz, f64.
fn string(frequency: f64, decay: f64, rate: f64) -> KarplusStrong<f64> {
    let mut s = KarplusStrong::new(Hz::new(50.0), SampleRate::of(rate));
    s.retarget(&KarplusStrongControls {
        frequency: Hz::new(frequency),
        decay: Seconds::new(decay),
    });
    s
}

/// The impulse response of `s`, `len` frames.
fn impulse(s: &mut KarplusStrong<f64>, len: usize) -> Vec<f64> {
    (0..len)
        .map(|n| s.tick(if n == 0 { 1.0 } else { 0.0 }))
        .collect()
}

/// The all-pass `(eta + z^-1) / (1 + eta z^-1)`'s phase delay at `w`, in frames:
/// `arg H = -w + 2 atan2(eta sin w, 1 + eta cos w)`.
fn all_pass_phase_delay(eta: f64, w: f64) -> f64 {
    1.0 - 2.0 / w * (eta * w.sin()).atan2(1.0 + eta * w.cos())
}

/// The all-pass's impulse response from `H(z) = (eta + z^-1) / (1 + eta z^-1)`: `eta` at 0
/// and `(1 - eta^2) (-eta)^(k-1)` at `k`.
fn all_pass_tap(eta: f64, k: usize) -> f64 {
    match k {
        0 => eta,
        k => (1.0 - eta * eta) * (-eta).powi(k as i32 - 1),
    }
}

/// The all-pass's `eta`: minus the coefficient `g` the string holds.
fn eta_of(s: &KarplusStrong<f64>) -> f64 {
    -s.tuning().1.get()
}

/// The decay per loop of `period` frames at `rate` that falls 60 dB in `t60` seconds.
fn rho(period: f64, rate: f64, t60: f64) -> f64 {
    (-LN_1000 * period / (rate * t60)).exp()
}

/// The loop's closed form at the complex frequency `s`, `z = e^s`:
/// `rho A(z) (1 + z^-1) / 2 z^-N`.
fn loop_gain(s: Complex64, n: usize, eta: f64, rho: f64) -> Complex64 {
    let zi = (-s).exp();
    let ap = (zi + eta) / (zi * eta + 1.0);
    ap * (zi + 1.0) * 0.5 * (-s * n as f64).exp() * rho
}

/// The frequency `w` that maximises `|sum x[n] hann[n] e^(-j w n)|` in `[lo, hi]`, by golden
/// section on a unimodal peak.
fn peak(x: &[f64], lo: f64, hi: f64) -> f64 {
    let len = x.len() as f64;
    let mag = |w: f64| {
        let (re, im) = x.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &v)| {
            let h = 0.5 - 0.5 * (TAU * n as f64 / len).cos();
            (
                re + v * h * (w * n as f64).cos(),
                im - v * h * (w * n as f64).sin(),
            )
        });
        re.hypot(im)
    };
    let phi = (5.0f64.sqrt() - 1.0) / 2.0;
    let (mut a, mut b) = (lo, hi);
    for _ in 0..80 {
        let (c, d) = (b - phi * (b - a), a + phi * (b - a));
        if mag(c) > mag(d) {
            b = d;
        } else {
            a = c;
        }
    }
    (a + b) / 2.0
}

/// With the loop of `P = N + 1/2 + d` frames, `d` the all-pass's phase delay at the fundamental,
/// the output's spectral peak sits at `w0 = 2 pi / P`. The peak of the Hann-windowed response
/// is found to 1e-10 and read against `fs / (N + 1/2 + d(f0))` with `d` from the all-pass's
/// closed-form phase delay. The pole sits off the unit circle by the per-frame decay `e =
/// -ln(rho cos(w0 / 2)) / D`, `D` the loop's group delay `N + 1/2 + (1 - eta^2) / (1 + 2 eta
/// cos w0 + eta^2)`, which moves its frequency down by `e tan(w0 / 2) / (2 D)` (analyticity of
/// the loop's log-gain: the gain falls `tan(w0 / 2) / 2` a radian); the tolerance is that shift
/// plus `1e-7` of `w0` for the harmonics' leak (Hann sidelobes 30 bins away and more). Dropping the half
/// frame of the average or the all-pass's delay moves the peak 0.5 to 1.5 frames in 20 to 100:
/// 1% or more.
#[test]
fn the_fundamental_is_the_rate_over_the_loop_of_n_plus_a_half_plus_the_all_pass_delay() {
    for (f0, rate) in [(1000.0, 48_000.0), (440.0, 44_100.0), (200.0, 48_000.0)] {
        let mut s = string(f0, 0.5, rate);
        let h = impulse(&mut s, 4096);
        let (n, _) = s.tuning();
        let eta = eta_of(&s);
        let w0 = TAU * f0 / rate;
        let period = rate / f0;
        let d = all_pass_phase_delay(eta, w0);
        let want = TAU / (n.get() as f64 + 0.5 + d);
        assert!(
            (want - w0).abs() < 1e-12 * w0,
            "the all-pass delay at f0 closes the loop to 2 pi: {want} against {w0}"
        );
        let group =
            n.get() as f64 + 0.5 + (1.0 - eta * eta) / (1.0 + 2.0 * eta * w0.cos() + eta * eta);
        let decay = -(rho(period, rate, 0.5) * (w0 / 2.0).cos()).ln() / group;
        let shift = decay * (w0 / 2.0).tan() / (2.0 * group);
        let got = peak(&h, 0.975 * w0, 1.025 * w0);
        assert!(
            (got - want).abs() < shift + 1e-7 * w0,
            "{f0} Hz: {got} against {want}, the pole's shift {shift}"
        );
    }
}

/// The fundamental's amplitude falls per period by `|p|^P`, `p` the loop's pole: the root of
/// `1 = rho A(z) avg(z) z^-N` near `e^(j w0)`, found by the secant method on the closed form.
/// A Hann-windowed lock-in at `w0` over 16 periods reads the fundamental alone (the other poles
/// are 16 bins away and more, under `1e-5` of it), so the ratio of two windows `P` frames apart
/// is `|p|^P` to that leak. To first order that is `rho cos(w0 / 2)`, the decay factor times the average's
/// `|H(f0)|`: the test asserts both, the second within the first-order bound
/// `|ln(rho cos(w0/2))| * 0.02` (the group delay of the loop is within 2% of its period).
#[test]
fn the_fundamental_falls_per_period_by_the_decay_times_the_averages_gain() {
    check_fall(0.5);
}

/// The decay is not capped: a 5 s decay at 1 kHz is `rho = exp(-ln(1000) 0.001 / 5) = 0.99862`
/// per period, above the comb's 0.99 bound, and the fundamental falls by the same closed form as
/// at 0.5 s. A cap at 0.99 reads the string's loop gain as 0.99 and the fall as 1.2% lower.
#[test]
fn a_five_second_decay_at_a_kilohertz_falls_per_period_by_the_closed_form() {
    assert!(rho(48.0, 48_000.0, 5.0) > 0.99);
    check_fall(5.0);
}

fn check_fall(t60: f64) {
    let (f0, rate) = (1000.0, 48_000.0);
    let mut s = string(f0, t60, rate);
    let h = impulse(&mut s, 48 * 60);
    let (n, _) = s.tuning();
    let eta = eta_of(&s);
    let r = rho(48.0, rate, t60);
    let w0 = TAU * f0 / rate;
    let target = |z: Complex64| loop_gain(z, n.get(), eta, r) - 1.0;
    let (mut a, mut b) = (Complex64::new(-1e-4, w0), Complex64::new(-2e-4, w0));
    for _ in 0..60 {
        let (fa, fb) = (target(a), target(b));
        if (fb - fa).norm() == 0.0 {
            break;
        }
        let c = b - fb * (b - a) / (fb - fa);
        (a, b) = (b, c);
    }
    let pole_per_period = (b.re * 48.0).exp();
    let amp = |from: usize| {
        let window: Vec<f64> = (0..48 * 16)
            .map(|i| h[from + i] * (0.5 - 0.5 * (TAU * i as f64 / (48.0 * 16.0)).cos()))
            .collect();
        let (re, im) = lock_in(&window, f0, rate);
        re.hypot(im)
    };
    let measured = amp(48 * 3) / amp(48 * 2);
    assert!(
        (measured - pole_per_period).abs() < 1e-5,
        "{measured} against the pole's {pole_per_period}"
    );
    let first_order = r * (w0 / 2.0).cos();
    assert!(
        (measured - first_order).abs() < first_order.ln().abs() * 0.02,
        "{measured} against rho cos(w0/2) = {first_order}"
    );
}

/// The first echoes are the average's `[1/2, 1/2]` through the all-pass `A` at `N`: `h[0] = 1` and
/// `h[N + k] = rho / 2 (a[k] + a[k - 1])` with `a` the all-pass's impulse response, and
/// nothing between. `rho = exp(-ln(1000) P / (fs T60))` with `P = fs / f0`.
#[test]
fn the_first_echo_is_the_decay_times_the_averages_taps_through_the_all_pass() {
    for t60 in [0.5, 5.0] {
        check_echo(t60);
    }
}

fn check_echo(t60: f64) {
    let (f0, rate) = (1000.0, 48_000.0);
    let mut s = string(f0, t60, rate);
    let h = impulse(&mut s, 200);
    let (n, _) = s.tuning();
    let (n, eta) = (n.get(), eta_of(&s));
    let r = rho(48.0, rate, t60);
    assert_eq!(h[0], 1.0);
    for (i, &y) in h.iter().enumerate().take(n).skip(1) {
        assert_eq!(y, 0.0, "frame {i} is before the first echo");
    }
    for k in 0..8 {
        let before = if k == 0 {
            0.0
        } else {
            all_pass_tap(eta, k - 1)
        };
        let want = r / 2.0 * (all_pass_tap(eta, k) + before);
        assert!(
            (h[n + k] - want).abs() < 1e-15,
            "echo tap {k}: {} against {want}",
            h[n + k]
        );
    }
}

/// A frequency above `fs / 6` is held at it: the loop is `6` frames, `N = 4` whole frames, and
/// one not finite reads as the lowest.
#[test]
fn a_frequency_past_a_sixth_of_the_rate_and_a_non_finite_one_are_held() {
    let high = string(20_000.0, 1.0, 48_000.0);
    let at_limit = string(8_000.0, 1.0, 48_000.0);
    assert_eq!(high.tuning(), at_limit.tuning());
    assert_eq!(high.tuning().0.get(), 4);
    let nan = string(f64::NAN, 1.0, 48_000.0);
    let lowest = string(50.0, 1.0, 48_000.0);
    assert_eq!(nan.tuning(), lowest.tuning());
}

/// The tail covers the ring-out: no frame past it is above `TAIL_FLOOR` (-60 dB) of the
/// excitation.
#[test]
fn the_declared_tail_covers_the_ring_out() {
    let (f0, rate) = (400.0, 48_000.0);
    let controls = KarplusStrongControls {
        frequency: Hz::new(f0),
        decay: Seconds::new(0.3),
    };
    let mut s = string(f0, 0.3, rate);
    let Tail::Finite(tail) = s.tail(&controls) else {
        panic!("a finite tail")
    };
    let h = impulse(&mut s, tail.get() + 20_000);
    let last = h.iter().rposition(|y| y.abs() > 1e-3).unwrap();
    assert!(
        last < tail.get(),
        "rings to {last}, declared {}",
        tail.get()
    );
    assert!(
        last > tail.get() / 20,
        "the declared tail is not absurdly long"
    );
}
