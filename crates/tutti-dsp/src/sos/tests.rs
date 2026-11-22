use core::f64::consts::PI;

use super::*;
use crate::TAIL_FLOOR;
use tutti_buffers::InputOutput;

const RATE: SampleRate = SampleRate::SR_48K;
const ORDERS: [u8; 9] = [1, 2, 3, 4, 5, 7, 8, 12, 16];
const FREQS: [f64; 8] = [30.0, 120.0, 400.0, 1000.0, 2500.0, 6000.0, 12000.0, 20000.0];

fn gain_at(sos: &Sos, f: f64) -> Complex64 {
    let mut h = [Complex64::new(0.0, 0.0)];
    sos.frequency_response(0, &[Hz::new(f)], &mut h);
    h[0]
}

/// The analog Butterworth magnitude at the bilinear image of `f`: `tan(w/2) / g` is the
/// frequency in units of the prewarped cutoff.
fn butterworth_mag(response: CutResponse, order: u8, fc: f64, f: f64) -> f64 {
    let t = libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get());
    let ratio = match response {
        CutResponse::LowPass => t,
        _ => 1.0 / t,
    };
    1.0 / (1.0 + libm::pow(ratio, 2.0 * f64::from(order))).sqrt()
}

/// Every design answers `1 / sqrt(1 + (w / wc)^2N)` at the prewarped frequency, to 1e-8 of
/// the figure, in both responses and at orders 1 to 16.
#[test]
fn the_butterworth_magnitude_is_the_analog_one_at_the_prewarped_frequency() {
    for response in [CutResponse::LowPass, CutResponse::HighPass] {
        for order in ORDERS {
            for fc in [100.0f64, 1000.0, 8000.0] {
                let sos = Butterworth::new(response, ButterworthOrder::of(order))
                    .design(Hz::new(fc), RATE)
                    .unwrap();
                assert_eq!(sos.sections().len(), usize::from(order).div_ceil(2));
                for f in FREQS {
                    let want = butterworth_mag(response, order, fc, f);
                    let got = gain_at(&sos, f).norm();
                    assert!(
                        (got - want).abs() <= 1e-8 * want + 1e-10,
                        "{response:?} {order} fc {fc} f {f}: {got} vs {want}"
                    );
                }
            }
        }
    }
}

/// The poles of every section are the analog prototype's, `exp(j pi (2m + N + 1) / 2N)`
/// scaled by the prewarped cutoff (`g p` low-pass, `g / p` high-pass) and taken through
/// `z = (1 + s) / (1 - s)`.
#[test]
fn the_section_poles_are_the_bilinear_image_of_the_prototype_poles() {
    for response in [CutResponse::LowPass, CutResponse::HighPass] {
        for order in ORDERS {
            let g = libm::tan(PI * 700.0 / RATE.get());
            let mut want: Vec<Complex64> = (0..order)
                .map(|m| {
                    let angle = PI * f64::from(2 * m + order + 1) / f64::from(2 * order);
                    let p = Complex64::from_polar(1.0, angle);
                    let s = match response {
                        CutResponse::LowPass => p * g,
                        _ => Complex64::new(g, 0.0) / p,
                    };
                    (1.0 + s) / (1.0 - s)
                })
                .collect();
            let sos = Butterworth::new(response, ButterworthOrder::of(order))
                .design(Hz::new(700.0), RATE)
                .unwrap();
            let mut got: Vec<Complex64> = sos.sections().iter().flat_map(Biquad::poles).collect();
            if order % 2 == 1 {
                // The first-order section's second pole is the origin: not the filter's.
                let at = got.iter().position(|p| p.norm() < 1e-12).unwrap();
                got.swap_remove(at);
            }
            assert_eq!(got.len(), want.len());
            for p in got {
                let at = want
                    .iter()
                    .position(|q| (p - q).norm() < 1e-9)
                    .unwrap_or_else(|| panic!("{response:?} {order}: stray pole {p}"));
                want.swap_remove(at);
            }
        }
    }
}

/// A Linkwitz-Riley design of order `2N` is the Butterworth magnitude squared (`-6 dB` at
/// the cutoff, exactly `1/2`) and its sections the Butterworth ones twice.
#[test]
fn linkwitz_riley_is_butterworth_squared() {
    for order in [2u8, 4, 6, 8, 12, 16] {
        let lr = LinkwitzRiley::new(CutResponse::LowPass, ButterworthOrder::of(order / 2))
            .design(Hz::new(1000.0), RATE)
            .unwrap();
        let bw = Butterworth::new(CutResponse::LowPass, ButterworthOrder::of(order / 2))
            .design(Hz::new(1000.0), RATE)
            .unwrap();
        assert_eq!(lr.sections().len(), 2 * bw.sections().len());
        for f in FREQS {
            let want = butterworth_mag(CutResponse::LowPass, order / 2, 1000.0, f).powi(2);
            assert!((gain_at(&lr, f).norm() - want).abs() <= 1e-8 * want + 1e-10);
        }
        assert!(
            (gain_at(&lr, 1000.0).norm() - 0.5).abs() < 1e-9,
            "LR{order}"
        );
    }
}

/// A low-pass and high-pass of one Linkwitz-Riley order have unit magnitude summed when the
/// order is a multiple of 4, and their difference does when it is not.
#[test]
fn the_linkwitz_riley_pair_sums_to_an_allpass() {
    for order in [2u8, 4, 6, 8, 10, 12, 16] {
        let at = |response| {
            LinkwitzRiley::new(response, ButterworthOrder::of(order / 2))
                .design(Hz::new(300.0), RATE)
                .unwrap()
        };
        let (lp, hp) = (at(CutResponse::LowPass), at(CutResponse::HighPass));
        for f in FREQS {
            let sign = if order % 4 == 0 { 1.0 } else { -1.0 };
            let sum = gain_at(&lp, f) + sign * gain_at(&hp, f);
            assert!(
                (sum.norm() - 1.0).abs() < 1e-8,
                "LR{order} at {f}: {}",
                sum.norm()
            );
        }
    }
}

/// A cutoff the rate refuses fails at the design, in both families.
#[test]
fn a_design_refuses_a_cutoff_the_rate_cannot_take() {
    for cutoff in [0.0, -5.0, 24000.0, 30000.0, f64::NAN] {
        let order = ButterworthOrder::of(4);
        let b = Butterworth::new(CutResponse::LowPass, order).design(Hz::new(cutoff), RATE);
        assert!(
            matches!(b, Err(SosError::CutoffOutOfRange { .. })),
            "{cutoff}"
        );
        let l = LinkwitzRiley::new(CutResponse::HighPass, order).design(Hz::new(cutoff), RATE);
        assert!(
            matches!(l, Err(SosError::CutoffOutOfRange { .. })),
            "{cutoff}"
        );
    }
    // The same cutoff at a faster rate is inside.
    let fast = SampleRate::SR_96K;
    assert!(
        Butterworth::new(CutResponse::LowPass, ButterworthOrder::of(4))
            .design(Hz::new(30000.0), fast)
            .is_ok()
    );
}

/// `ButterworthOrder` keeps the range-checked shape over `1..=16`.
#[test]
fn a_butterworth_order_keeps_the_range_checked_shape() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: ButterworthOrder::new,
        get: ButterworthOrder::get,
        of: Some(ButterworthOrder::of),
        valid: &[1, 8, 16],
        invalid: &[0, 17, 255],
    });
}

/// The slowest designs, LR96 at 10 Hz, ring for fewer frames than the run follows (`2^24`)
/// at the rates a node runs at, in both responses.
#[test]
fn the_slowest_design_has_a_finite_tail_at_the_fastest_rate() {
    for rate in [RATE, SampleRate::SR_192K] {
        for response in [CutResponse::LowPass, CutResponse::HighPass] {
            let sos = LinkwitzRiley::new(response, ButterworthOrder::of(8))
                .design(Hz::new(10.0), rate)
                .unwrap();
            assert!(
                matches!(sos.tail(TAIL_FLOOR), DecayTime::Finite(_)),
                "{response:?} {rate:?}"
            );
        }
    }
}

/// A section's state-variable form has the response of its coefficients, second order and
/// first order alike.
#[test]
fn the_state_variable_form_has_the_biquads_response() {
    let second = Biquad {
        b0: 0.2,
        b1: 0.1,
        b2: -0.05,
        a1: -1.2,
        a2: 0.5,
    };
    let first = Biquad {
        b0: 0.3,
        b1: 0.2,
        b2: 0.0,
        a1: -0.4,
        a2: 0.0,
    };
    for b in [second, first] {
        let svf = svf_form(&b);
        for w in [0.05, 0.4, 1.3, 2.9] {
            assert!(
                (svf.response(w) - b.response(w)).norm() < 1e-9,
                "{b:?} at {w}"
            );
        }
    }
}

/// An impulse response through `sos`'s sections in transposed direct form II, in `f64`:
/// a realization the tail's own never uses.
fn impulse(sos: &Sos, frames: usize) -> Vec<f64> {
    let mut state = vec![[0.0f64; 2]; sos.sections().len()];
    (0..frames)
        .map(|n| {
            let mut x = if n == 0 { 1.0 } else { 0.0 };
            for (b, z) in sos.sections().iter().zip(&mut state) {
                let y = b.b0 * x + z[0];
                z[0] = b.b1 * x - b.a1 * y + z[1];
                z[1] = b.b2 * x - b.a2 * y;
                x = y;
            }
            x
        })
        .collect()
}

/// The frames until `h` last carries `TAIL_FLOOR` of its peak.
fn ring_of(h: &[f64]) -> usize {
    let peak = h.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let eps = TAIL_FLOOR.to_amplitude().get();
    h.iter().rposition(|v| v.abs() >= eps * peak).unwrap() + 1
}

fn frames_of(sos: &Sos) -> usize {
    match sos.tail(TAIL_FLOOR) {
        DecayTime::Finite(s) => (s.get() * RATE.get()).round() as usize,
        DecayTime::Unbounded => panic!("unbounded"),
    }
}

/// Two equal one-pole sections ring as `(n + 1) p^n`: the declared tail is the frame that
/// closed form last carries the floor of its peak, and one section alone, the repeated
/// pole's `p^n`, rings far shorter.
#[test]
fn a_repeated_pole_rings_as_n_times_r_to_the_n() {
    let p = 0.999f64;
    let section = Biquad {
        b0: 1.0 - p,
        b1: 0.0,
        b2: 0.0,
        a1: -p,
        a2: 0.0,
    };
    let twice = Sos::new(vec![section, section], RATE);
    let h: Vec<f64> = (0..60_000)
        .map(|n| (1.0 - p).powi(2) * (n as f64 + 1.0) * p.powi(n))
        .collect();
    assert_eq!(frames_of(&twice), ring_of(&h));
    let once = frames_of(&Sos::new(vec![section], RATE));
    assert!(
        once * 5 / 4 < frames_of(&twice),
        "{once} vs {}",
        frames_of(&twice)
    );
}

/// LR96 (Butterworth 8 twice, sixteen poles, eight sections) at a low cutoff: the declared
/// tail covers the last frame of an independent `f64` impulse response above the floor and
/// is at most 25% over it, and the longest tail of its sections alone, the figure of a
/// max over sections, is under four fifths of it (the repeated slowest pole adds `n r^n`).
#[test]
fn the_tail_of_a_long_cascade_is_that_of_all_its_poles_together() {
    for response in [CutResponse::LowPass, CutResponse::HighPass] {
        let sos = LinkwitzRiley::new(response, ButterworthOrder::of(8))
            .design(Hz::new(40.0), RATE)
            .unwrap();
        let ring = ring_of(&impulse(&sos, 1 << 21));
        let declared = frames_of(&sos);
        assert!(
            ring <= declared && declared * 4 <= ring * 5,
            "{response:?}: {declared} vs {ring}"
        );
        // A high-pass's peak is its first frames, so a section alone (its own, smaller
        // peak) can ring longer relative to it than the cascade does: the comparison is
        // the low-pass's, whose peak is the slow response.
        if response == CutResponse::LowPass {
            let longest_section = sos
                .sections()
                .iter()
                .map(|b| frames_of(&Sos::new(vec![*b], RATE)))
                .max()
                .unwrap();
            assert!(
                longest_section * 5 < declared * 4,
                "{longest_section} vs {declared}"
            );
        }
    }
}

/// A cascade that is not inside the unit circle never settles, and the l1 bound is above
/// the measured l1 norm of a stable one.
#[test]
fn the_norms_bound_the_response_and_an_unstable_cascade_is_unbounded() {
    let sos = Butterworth::new(CutResponse::LowPass, ButterworthOrder::of(6))
        .design(Hz::new(500.0), RATE)
        .unwrap();
    assert!(sos.is_stable() && sos.spectral_radius() < 1.0);
    let l1: f64 = impulse(&sos, 1 << 16).iter().map(|v| v.abs()).sum();
    assert!(sos.l1_norm() >= l1, "{} vs {l1}", sos.l1_norm());
    let hot = Sos::new(
        vec![Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: -2.0,
            a2: 1.01,
        }],
        RATE,
    );
    assert_eq!(hot.tail(TAIL_FLOOR), DecayTime::Unbounded);
}

/// A 5 Hz Butterworth high-pass at 192 kHz, the cascade's range extreme (an `SvfCoefficients`
/// per section, so the SVF's case), run in `f32` through the bank.
///
/// At the cutoff `W = 1` and the second-order high-pass `H = -W^2 / (1 - W^2 + j sqrt 2 W)` is
/// `j / sqrt 2`. Noise: the one section has the SVF's `k = sqrt 2`, `g = tan(pi fc / fs) =
/// 8.2e-5`, so `tau = 1 / (g k) = 8 643` frames and the output's deviation has standard
/// deviation `2^-24 / sqrt 12 * sqrt((k^2 + 1) tau) = 2.8e-6`. The test measures `N = 192 000`
/// frames, whose expected maximum is `sqrt(2 ln N) = 4.9` standard deviations; the bound is 1.5
/// times that, 7.4 of them, for the non-Gaussian rounding tails and the correlated state noise.
#[test]
fn a_five_hz_high_pass_at_192_khz_holds_its_gain_and_noise_in_f32() {
    use crate::testing::{sine_fit, steady_deviation};
    use tutti_buffers::ChannelLayout;
    use tutti_buffers::{Planes, PlanesMut};
    const FS: f64 = 192_000.0;
    const FC: f64 = 5.0;
    let whole = FS as usize;
    let w = 2.0 * PI * FC / FS;
    let sos = Butterworth::new(CutResponse::HighPass, ButterworthOrder::of(2))
        .design(Hz::new(FC), SampleRate::of(FS))
        .unwrap();
    let mut ramp = crate::Ramp::new(SosCoefficients::<f32>::new(&sos).unwrap());
    let input: Vec<f32> = (0..2 * whole)
        .map(|n| libm::sin(w * n as f64) as f32)
        .collect();
    let mut out = vec![0.0f32; input.len()];
    SosBank::<f32>::new(ChannelLayout::MONO)
        .process(
            &mut ramp,
            &mut InputOutput::paired(
                Planes::new(&[&input[..]]).unwrap(),
                PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let y: Vec<f64> = out.iter().map(|&v| f64::from(v)).collect();
    let h = Complex64::new(0.0, core::f64::consts::FRAC_1_SQRT_2);
    let tau = 1.0 / (libm::tan(PI * FC / FS) * core::f64::consts::SQRT_2);
    let sigma = 2f64.powi(-24) / 12f64.sqrt() * (3.0 * tau).sqrt();
    let bound = 1.5 * (2.0 * (whole as f64).ln()).sqrt() * sigma;
    let fit = sine_fit(&y, w, whole);
    assert!((fit - h).norm() < bound, "the fit {fit} against {h}");
    let worst = steady_deviation(&y, w, whole, h);
    assert!(worst < bound, "deviation {worst:e} over {bound:e}");
}
