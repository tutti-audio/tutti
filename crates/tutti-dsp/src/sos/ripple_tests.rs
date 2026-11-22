//! The Chebyshev and elliptic designs against their closed forms, read through
//! [`Lti::frequency_response`]: the edges at exactly the ripple or attenuation, the extrema
//! and zeros at the analytic frequencies mapped through the prewarp.

use core::f64::consts::PI;

use super::jacobi::{Modulus, cd, complete_k, incomplete_k, sn_cn_dn};
use super::*;

const RATE: SampleRate = SampleRate::SR_48K;
const FC: f64 = 1_000.0;
const ORDERS: [u8; 8] = [1, 2, 3, 4, 5, 8, 11, 16];
const RESPONSES: [CutResponse; 2] = [CutResponse::LowPass, CutResponse::HighPass];

fn db_at(sos: &Sos, f: f64) -> f64 {
    let mut h = [Complex64::new(0.0, 0.0)];
    sos.frequency_response(0, &[Hz::new(f)], &mut h);
    20.0 * libm::log10(h[0].norm())
}

/// The physical frequency of the prototype frequency `omega` (the edge is 1) for a design
/// at `FC`: the low-pass `tan(w / 2) = g omega`, the high-pass `tan(w / 2) = g / omega`.
fn mapped(response: CutResponse, omega: f64) -> f64 {
    let g = libm::tan(PI * FC / RATE.get());
    let t = match response {
        CutResponse::LowPass => g * omega,
        _ => g / omega,
    };
    libm::atan(t) * RATE.get() / PI
}

fn order(n: u8) -> ButterworthOrder {
    ButterworthOrder::of(n)
}

fn ripple(db: f64) -> PassbandRipple {
    PassbandRipple::new(Db::new(db)).unwrap()
}

fn tolerances(ripple_db: f64, attenuation_db: f64) -> EllipticTolerances {
    EllipticTolerances::new(ripple(ripple_db), attenuation(attenuation_db)).unwrap()
}

fn attenuation(db: f64) -> StopbandAttenuation {
    StopbandAttenuation::new(Db::new(db)).unwrap()
}

/// The gain of the DC (low-pass) or Nyquist (high-pass) end, the prototype's `omega = 0`.
fn pass_end_db(sos: &Sos, response: CutResponse) -> f64 {
    match response {
        CutResponse::LowPass => db_at(sos, 0.0),
        _ => db_at(sos, RATE.get() / 2.0),
    }
}

/// The gain at the passband edge is `-ripple` dB, to 1e-9 dB, at every order, in both
/// responses.
#[test]
fn chebyshev_i_gain_at_the_passband_edge_is_exactly_the_ripple() {
    for response in RESPONSES {
        for n in ORDERS {
            for r in [0.1, 1.0, 3.0] {
                let sos = ChebyshevI::new(response, order(n), ripple(r))
                    .design(Hz::new(FC), RATE)
                    .unwrap();
                assert_eq!(sos.sections().len(), usize::from(n).div_ceil(2));
                assert!(sos.spectral_radius() < 1.0, "{response:?} {n} {r}");
                let got = db_at(&sos, FC);
                assert!((got + r).abs() < 1e-9, "{response:?} {n} {r}: {got}");
            }
        }
    }
}

/// The Chebyshev polynomial of degree N is `+-1` at `w = cos(k pi / N)` (the gain is `-ripple` dB) and `0` at `cos((2k - 1)
/// pi / 2N)` (the gain is 0 dB): the passband's N extrema alternate between the two. DC is 0
/// dB for an odd order and `-ripple` dB for an even one.
#[test]
fn chebyshev_i_passband_extrema_alternate_between_0_db_and_minus_ripple() {
    for response in RESPONSES {
        for n in ORDERS {
            let r = 1.5;
            let sos = ChebyshevI::new(response, order(n), ripple(r))
                .design(Hz::new(FC), RATE)
                .unwrap();
            let order = usize::from(n);
            for k in 0..=order / 2 {
                let omega = libm::cos(PI * k as f64 / order as f64);
                let got = db_at(&sos, mapped(response, omega));
                assert!((got + r).abs() < 1e-8, "min {response:?} {n} k {k}: {got}");
            }
            for k in 1..=order.div_ceil(2) {
                let omega = libm::cos(PI * (2 * k - 1) as f64 / (2 * order) as f64);
                let got = db_at(&sos, mapped(response, omega));
                assert!(got.abs() < 1e-8, "max {response:?} {n} k {k}: {got}");
            }
            let end = pass_end_db(&sos, response);
            let want = if order % 2 == 1 { 0.0 } else { -r };
            assert!((end - want).abs() < 1e-8, "{response:?} {n}: DC {end}");
        }
    }
}

/// The gain at the stopband edge is `-attenuation` dB and the stopband never rises above it;
/// the passband end is 0 dB.
#[test]
fn chebyshev_ii_gain_at_the_stopband_edge_is_exactly_the_attenuation() {
    for response in RESPONSES {
        for n in ORDERS {
            for a in [20.0, 40.0, 80.0] {
                let sos = ChebyshevII::new(response, order(n), attenuation(a))
                    .design(Hz::new(FC), RATE)
                    .unwrap();
                let got = db_at(&sos, FC);
                assert!((got + a).abs() < 1e-8, "{response:?} {n} {a}: {got}");
                assert!(pass_end_db(&sos, response).abs() < 1e-9, "{response:?} {n}");
                for step in 0..200 {
                    let omega = 1.0 + 0.05 * f64::from(step);
                    let outside = db_at(&sos, mapped(response, omega));
                    assert!(
                        outside <= -a + 1e-8,
                        "{response:?} {n} w {omega}: {outside}"
                    );
                }
            }
        }
    }
}

/// The zeros are at `sec((2k - 1) pi / 2N)` times the stopband edge: the gain there is a
/// null (below -250 dB; the zero is on the unit circle to rounding).
#[test]
fn chebyshev_ii_zeros_are_at_the_mapped_secants() {
    for response in RESPONSES {
        for n in ORDERS {
            let sos = ChebyshevII::new(response, order(n), attenuation(50.0))
                .design(Hz::new(FC), RATE)
                .unwrap();
            let order = usize::from(n);
            for k in 1..=order / 2 {
                let omega = 1.0 / libm::cos(PI * (2 * k - 1) as f64 / (2 * order) as f64);
                let got = db_at(&sos, mapped(response, omega));
                assert!(got < -250.0, "{response:?} {n} k {k}: {got} dB");
            }
        }
    }
}

/// The passband edge is `-ripple` dB, the stopband edge the design reports is `-attenuation`
/// dB, and every point past it is at or under it.
#[test]
fn elliptic_edges_are_the_ripple_and_the_attenuation() {
    for response in RESPONSES {
        for n in ORDERS {
            for (r, a) in [(0.5, 40.0), (1.0, 60.0), (0.1, 90.0)] {
                let design = Elliptic::new(response, order(n), tolerances(r, a));
                let sos = design.design(Hz::new(FC), RATE).unwrap();
                assert_eq!(sos.sections().len(), usize::from(n).div_ceil(2));
                assert!(sos.spectral_radius() < 1.0, "{response:?} {n} {r} {a}");
                let passband = db_at(&sos, FC);
                assert!((passband + r).abs() < 1e-8, "{response:?} {n}: {passband}");
                let edge = design.stopband_edge(Hz::new(FC), RATE).unwrap();
                let stopband = db_at(&sos, edge.get());
                assert!(
                    (stopband + a).abs() < 1e-7,
                    "{response:?} {n} {r} {a}: {stopband}"
                );
                let omega = design.selectivity().get().recip();
                for step in 0..400 {
                    let outside = db_at(
                        &sos,
                        mapped(response, omega * (1.0 + 0.05 * f64::from(step))),
                    );
                    assert!(
                        outside <= -a + 1e-7,
                        "{response:?} {n} step {step}: {outside}"
                    );
                }
            }
        }
    }
}

/// The passband's maxima are 0 dB, at `cd((2j - 1) K / N, k)`; its minima `-ripple` dB, at
/// `cd(2 j K / N, k)`; nothing in it exceeds 0 dB. DC is 0 dB for an odd order and `-ripple`
/// dB for an even one.
#[test]
fn elliptic_passband_maximum_is_0_db_and_its_minima_are_the_ripple() {
    for response in RESPONSES {
        for n in ORDERS {
            let (r, a) = (0.8, 55.0);
            let design = Elliptic::new(response, order(n), tolerances(r, a));
            let sos = design.design(Hz::new(FC), RATE).unwrap();
            let m = Modulus::new(design.selectivity().get());
            let order = usize::from(n);
            for j in 1..=order.div_ceil(2) {
                let omega = cd((2 * j - 1) as f64 / order as f64 * complete_k(m), m);
                let got = db_at(&sos, mapped(response, omega));
                assert!(got.abs() < 1e-7, "max {response:?} {n} j {j}: {got}");
            }
            for j in 0..=order / 2 {
                let omega = cd(2.0 * j as f64 / order as f64 * complete_k(m), m);
                let got = db_at(&sos, mapped(response, omega));
                assert!((got + r).abs() < 1e-7, "min {response:?} {n} j {j}: {got}");
            }
            for step in 0..=500 {
                let got = db_at(&sos, mapped(response, f64::from(step) / 500.0));
                assert!(
                    got < 1e-7 && got > -r - 1e-7,
                    "{response:?} {n} {step}: {got}"
                );
            }
            let end = pass_end_db(&sos, response);
            let want = if order % 2 == 1 { 0.0 } else { -r };
            assert!((end - want).abs() < 1e-7, "{response:?} {n}: DC {end}");
        }
    }
}

/// The zeros are at `1 / (k cd((2i - 1) K / N, k))` times the passband edge: nulls.
#[test]
fn elliptic_zeros_are_at_the_mapped_cd_frequencies() {
    for response in RESPONSES {
        for n in ORDERS {
            let design = Elliptic::new(response, order(n), tolerances(1.0, 60.0));
            let sos = design.design(Hz::new(FC), RATE).unwrap();
            let m = Modulus::new(design.selectivity().get());
            let order = usize::from(n);
            for i in 1..=order / 2 {
                let zeta = cd((2 * i - 1) as f64 / order as f64 * complete_k(m), m);
                let got = db_at(&sos, mapped(response, 1.0 / (m.k() * zeta)));
                assert!(got < -250.0, "{response:?} {n} i {i}: {got} dB");
            }
        }
    }
}

/// The degree equation `N = K(k) K'(k1) / (K'(k) K(k1))` holds for the selectivity of every
/// order, with the complete integrals from the mean (independent of the theta series the
/// modulus came from).
#[test]
fn elliptic_selectivity_solves_the_degree_equation() {
    for n in ORDERS {
        for (r, a) in [(0.5, 40.0), (1.0, 60.0), (0.1, 90.0), (3.0, 40.0)] {
            let design = Elliptic::new(CutResponse::LowPass, order(n), tolerances(r, a));
            let m = Modulus::new(design.selectivity().get());
            let eps = |db: f64| libm::sqrt(libm::pow(10.0, db / 10.0) - 1.0);
            let k1 = Modulus::new(eps(r) / eps(a));
            let got = complete_k(m) * complete_k(k1.complement())
                / (complete_k(m.complement()) * complete_k(k1));
            assert!(
                (got - f64::from(n)).abs() < 1e-9 * f64::from(n),
                "{n} {r} {a}: {got}"
            );
        }
    }
}

/// A larger order is steeper: the stopband edge moves in toward the passband edge.
#[test]
fn elliptic_stopband_edge_narrows_with_the_order() {
    let edge = |n| {
        Elliptic::new(CutResponse::LowPass, order(n), tolerances(1.0, 60.0))
            .stopband_edge(Hz::new(FC), RATE)
            .unwrap()
            .get()
    };
    assert!(edge(3) > edge(5) && edge(5) > edge(8) && edge(8) > FC);
}

/// `sn(K, k) = 1`, `cn(K, k) = 0`, `cd(0, k) = 1`, `cd(K, k) = 0`, and the derivative
/// `d sn / du = cn dn` along the real axis (a central difference of 1e-5).
#[test]
fn jacobi_functions_hold_their_special_values_and_derivative() {
    for k in [0.0, 0.1, 0.5, 0.9, 0.999] {
        let m = Modulus::new(k);
        let big_k = complete_k(m);
        let (sn, cn, _) = sn_cn_dn(big_k, m);
        assert!(
            (sn - 1.0).abs() < 1e-12 && cn.abs() < 1e-7,
            "k {k}: {sn} {cn}"
        );
        assert!((cd(0.0, m) - 1.0).abs() < 1e-15, "k {k}");
        assert!(cd(big_k, m).abs() < 1e-7, "k {k}");
        let h = 1e-5;
        for step in 1..20 {
            let u = f64::from(step) * big_k / 10.0;
            let slope = (sn_cn_dn(u + h, m).0 - sn_cn_dn(u - h, m).0) / (2.0 * h);
            let (_, cn, dn) = sn_cn_dn(u, m);
            assert!(
                (slope - cn * dn).abs() < 1e-8,
                "k {k} step {step}: {slope} {}",
                cn * dn
            );
        }
    }
}

/// At `k = 0` the functions are `sin`, `cos` and 1, and `K = pi / 2`.
#[test]
fn jacobi_functions_at_zero_modulus_are_the_circular_ones() {
    let m = Modulus::new(0.0);
    assert!((complete_k(m) - PI / 2.0).abs() < 1e-15);
    for step in 0..40 {
        let u = 0.2 * f64::from(step);
        let (sn, cn, dn) = sn_cn_dn(u, m);
        assert!((sn - libm::sin(u)).abs() < 1e-14, "u {u}");
        assert!((cn - libm::cos(u)).abs() < 1e-14, "u {u}");
        assert!((dn - 1.0).abs() < 1e-15, "u {u}");
        assert!((cd(u, m) - libm::cos(u)).abs() < 1e-14, "u {u}");
    }
}

/// `K(k = 1/2) = 1.6857503548` (Abramowitz and Stegun, Table 17.1, parameter `m = k^2 =
/// 0.25`, the integral `K = int_0^(pi/2) dt / sqrt(1 - k^2 sin^2 t)`), and
/// `K(1 / sqrt 2) = Gamma(1/4)^2 / (4 sqrt pi) = 1.8540746773`.
#[test]
fn complete_k_matches_the_tabulated_values() {
    assert!((complete_k(Modulus::new(0.5)) - 1.685_750_354_8).abs() < 1e-10);
    assert!(
        (complete_k(Modulus::new(core::f64::consts::FRAC_1_SQRT_2)) - 1.854_074_677_3).abs()
            < 1e-10
    );
}

/// `F(phi, k)` inverts `sn`: `sn(F(phi, k), k) = sin(phi)`, and `F(pi / 2, k) = K(k)`,
/// beyond a half turn too.
#[test]
fn incomplete_k_is_the_inverse_of_sn() {
    for k in [0.0, 0.3, 0.8, 0.99] {
        let m = Modulus::new(k);
        assert!(
            (incomplete_k(PI / 2.0, m) - complete_k(m)).abs() < 1e-12,
            "k {k}"
        );
        for step in 1..20 {
            let phi = f64::from(step) * PI / 40.0;
            let (sn, _, _) = sn_cn_dn(incomplete_k(phi, m), m);
            assert!((sn - libm::sin(phi)).abs() < 1e-12, "k {k} phi {phi}");
        }
    }
}

/// The modulus of the nome `q = exp(-pi K' / K)` round-trips through `K` and `K'`.
#[test]
fn the_modulus_of_a_nome_has_that_nome() {
    for q in [1e-6, 0.01, 0.2, 0.6, 0.9] {
        let m = Modulus::from_nome(q);
        let back = libm::exp(-PI * complete_k(m.complement()) / complete_k(m));
        assert!((back - q).abs() < 1e-12 * q.max(1e-3), "q {q}: {back}");
    }
}

/// A cutoff outside `(0, Nyquist)`, an elliptic attenuation not above its ripple, a ripple or
/// attenuation outside its range and an order outside `1..=16` are each refused by name.
#[test]
fn bad_settings_are_refused_by_name() {
    let low = CutResponse::LowPass;
    for cutoff in [0.0, -1.0, 24_000.0, 30_000.0, f64::NAN] {
        let hz = Hz::new(cutoff);
        assert!(matches!(
            ChebyshevI::new(low, order(4), ripple(1.0)).design(hz, RATE),
            Err(SosError::CutoffOutOfRange { .. })
        ));
        assert!(matches!(
            ChebyshevII::new(low, order(4), attenuation(40.0)).design(hz, RATE),
            Err(SosError::CutoffOutOfRange { .. })
        ));
        assert!(matches!(
            Elliptic::new(low, order(4), tolerances(1.0, 40.0)).design(hz, RATE),
            Err(SosError::CutoffOutOfRange { .. })
        ));
    }
    for (r, a) in [(1.0, 1.0), (2.0, 1.0)] {
        assert_eq!(EllipticTolerances::new(ripple(r), attenuation(a)), None);
    }
    for bad in [0.0, -1.0, 20.5, f64::NAN, f64::INFINITY] {
        assert_eq!(PassbandRipple::new(Db::new(bad)), None, "{bad}");
        assert!(PassbandRipple::try_from(Db::new(bad)).is_err(), "{bad}");
    }
    for bad in [0.0, -1.0, 150.5, f64::NAN, f64::INFINITY] {
        assert_eq!(StopbandAttenuation::new(Db::new(bad)), None, "{bad}");
        assert!(
            StopbandAttenuation::try_from(Db::new(bad)).is_err(),
            "{bad}"
        );
    }
    assert_eq!(ButterworthOrder::new(0), None);
    assert_eq!(ButterworthOrder::new(17), None);
}
