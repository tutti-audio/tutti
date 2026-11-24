use std::f64::consts::PI;

use super::*;

const RATE: f64 = 48_000.0;

fn rate() -> SampleRate {
    SampleRate::of(RATE)
}

fn hz(v: f64) -> Hz {
    Hz::new(v)
}

fn taps(fir: &Fir) -> Vec<f64> {
    fir.taps().collect()
}

/// The ideal gain at `f` of a response: 1 in a passband, 0 in a stopband,
/// `None` in a transition band.
fn ideal(response: FirResponse, transition: f64, f: f64) -> Option<f64> {
    let half = transition / 2.0;
    let side = |edge: f64, pass_below: bool| -> Option<f64> {
        if (f - edge).abs() < half {
            None
        } else {
            Some(if (f < edge) == pass_below { 1.0 } else { 0.0 })
        }
    };
    match response {
        FirResponse::LowPass { cutoff } => side(cutoff.get(), true),
        FirResponse::HighPass { cutoff } => side(cutoff.get(), false),
        FirResponse::HalfBand => side(RATE / 4.0, true),
        FirResponse::BandPass { low, high } => {
            Some(side(low.get(), false)? * side(high.get(), true)?)
        }
        FirResponse::BandStop { low, high } => {
            Some(1.0 - side(low.get(), false)? * side(high.get(), true)?)
        }
    }
}

fn responses() -> [FirResponse; 5] {
    [
        FirResponse::LowPass {
            cutoff: hz(6_000.0),
        },
        FirResponse::HighPass {
            cutoff: hz(6_000.0),
        },
        FirResponse::BandPass {
            low: hz(5_000.0),
            high: hz(11_000.0),
        },
        FirResponse::BandStop {
            low: hz(5_000.0),
            high: hz(11_000.0),
        },
        FirResponse::HalfBand,
    ]
}

/// Published Kaiser (1974) formulas, written out from the text: the tests
/// compute their expected figures here, not through the implementation.
fn beta_formula(a: f64) -> f64 {
    if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    }
}

#[test]
fn kaiser_beta_matches_the_published_formula() {
    for a in [20.0f64, 21.0, 40.0, 50.0, 60.0, 80.0] {
        let got = kaiser_beta(Db::new(a));
        let want = beta_formula(a);
        assert!((got - want).abs() < 1e-12, "A = {a}: {got} vs {want}");
    }
    assert_eq!(kaiser_beta(Db::new(20.0)), 0.0);
}

#[test]
fn kaiser_order_matches_the_published_formula_at_a_transition_of_005_pi() {
    let dw = 0.05 * PI;
    for a in [20.0f64, 21.0, 40.0, 50.0, 60.0, 80.0] {
        let want = ((a - 7.95) / (2.285 * dw) + 1.0).ceil() as usize;
        assert_eq!(kaiser_order(a, dw), want, "A = {a}");
        assert_eq!(odd_up(want) % 2, 1);
        assert!(odd_up(want) - want <= 1);
    }
}

/// scipy's documented example `kaiserord(65, 24/500)`: a width of 24/500 of
/// Nyquist is 0.048π rad/sample.
#[test]
fn kaiser_order_and_beta_reproduce_the_scipy_example() {
    assert_eq!(kaiser_order(65.0, 0.048 * PI), 167);
    assert!((kaiser_beta(Db::new(65.0)) - 6.20426).abs() < 1e-5);
}

#[test]
fn the_length_is_odd_and_follows_the_estimate() {
    let fir = FirDesign::new(
        FirResponse::LowPass {
            cutoff: hz(6_000.0),
        },
        hz(1_000.0),
    )
    .attenuation(Db::new(65.0))
    .design(rate())
    .unwrap();
    let estimate = ((65.0 - 7.95) / (2.285 * 2.0 * PI * 1_000.0 / RATE) + 1.0).ceil() as usize;
    let want = estimate | 1;
    assert_eq!(fir.len(), Samples::new(want));
    assert_eq!(fir.len().get() % 2, 1);
    assert_eq!(fir.delay(), Samples::new((want - 1) / 2));
}

/// Passband within the Kaiser ripple `δ = 10^(-A/20)` and stopband at or below
/// `-A` dB, each with a stated margin, outside the transition bands, on a
/// dense grid. Kaiser's estimate is approximate: the margin is 1.5x in the
/// passband and 3 dB in the stopband.
#[test]
fn every_response_meets_its_ripple_and_attenuation() {
    for a in [40.0f64, 60.0, 80.0] {
        let delta = Db::new(-a).to_amplitude().get();
        for response in responses() {
            let transition = 1_000.0f64;
            let fir = FirDesign::new(response, hz(transition))
                .attenuation(Db::new(a))
                .design(rate())
                .unwrap();
            let h = taps(&fir);
            let grid = 4_096;
            for i in 0..=grid {
                let omega = PI * f64::from(i) / f64::from(grid);
                let f = omega * RATE / (2.0 * PI);
                let Some(want) = ideal(response, transition, f) else {
                    continue;
                };
                let got = tutti_fixtures::signal::dtft_magnitude(&h, omega);
                if want == 1.0 {
                    assert!(
                        (got - 1.0).abs() <= 1.5 * delta,
                        "{response:?} A={a} f={f}: {got}"
                    );
                } else {
                    let limit = 10f64.powf((-a + 3.0) / 20.0);
                    assert!(got <= limit, "{response:?} A={a} f={f}: {got} > {limit}");
                }
            }
        }
    }
}

#[test]
fn the_taps_are_symmetric_bit_for_bit() {
    for response in responses() {
        let fir = FirDesign::new(response, hz(800.0)).design(rate()).unwrap();
        let h = taps(&fir);
        for n in 0..h.len() {
            assert_eq!(
                h[n].to_bits(),
                h[h.len() - 1 - n].to_bits(),
                "{response:?} tap {n}"
            );
        }
    }
}

#[test]
fn the_half_band_has_exact_zeros_and_a_centre_of_one_half() {
    for transition in [500.0f64, 2_000.0, 6_000.0] {
        let fir = FirDesign::new(FirResponse::HalfBand, hz(transition))
            .attenuation(Db::new(70.0))
            .design(rate())
            .unwrap();
        let h = taps(&fir);
        let m = fir.delay().get();
        assert_eq!(fir.len().get() % 4, 3);
        assert_eq!(h[m], 0.5);
        for k in (2..=m).step_by(2) {
            assert_eq!(h[m - k], 0.0);
            assert_eq!(h[m + k], 0.0);
        }
        assert!(h[m - 1] != 0.0);
        // H(ω) + H(π − ω) = 1 follows from the zeros and the centre.
        let sum: f64 = h.iter().sum();
        assert!((sum - 1.0).abs() < 4.0 * f64::EPSILON * h.len() as f64);
    }
}

#[test]
fn the_low_pass_has_unit_dc_gain_and_the_high_pass_unit_nyquist_gain() {
    let low = FirDesign::new(
        FirResponse::LowPass {
            cutoff: hz(3_000.0),
        },
        hz(600.0),
    )
    .design(rate())
    .unwrap();
    let h = taps(&low);
    let dc: f64 = h.iter().sum();
    assert!(
        (dc - 1.0).abs() <= 4.0 * f64::EPSILON * h.len() as f64,
        "{dc}"
    );

    let high = FirDesign::new(
        FirResponse::HighPass {
            cutoff: hz(3_000.0),
        },
        hz(600.0),
    )
    .design(rate())
    .unwrap();
    let h = taps(&high);
    let nyquist: f64 = h
        .iter()
        .enumerate()
        .map(|(n, t)| if n % 2 == 0 { *t } else { -*t })
        .sum();
    assert!(
        (nyquist - 1.0).abs() <= 4.0 * f64::EPSILON * h.len() as f64,
        "{nyquist}"
    );
    // And it rejects DC.
    assert!(h.iter().sum::<f64>().abs() < 1e-3);
}

#[test]
fn a_cutoff_outside_the_open_band_is_refused() {
    for cutoff in [0.0, -10.0, 24_000.0, 30_000.0, f64::NAN] {
        let err = FirDesign::new(FirResponse::LowPass { cutoff: hz(cutoff) }, hz(100.0))
            .design(rate())
            .unwrap_err();
        assert!(
            matches!(err, FirError::CutoffOutOfRange { .. }),
            "{cutoff}: {err:?}"
        );
    }
    let err = FirDesign::new(
        FirResponse::BandPass {
            low: hz(1_000.0),
            high: hz(30_000.0),
        },
        hz(100.0),
    )
    .design(rate())
    .unwrap_err();
    assert!(matches!(err, FirError::CutoffOutOfRange { .. }));
}

#[test]
fn unordered_band_edges_are_refused() {
    for (low, high) in [(5_000.0, 5_000.0), (6_000.0, 5_000.0)] {
        let err = FirDesign::new(
            FirResponse::BandStop {
                low: hz(low),
                high: hz(high),
            },
            hz(100.0),
        )
        .design(rate())
        .unwrap_err();
        assert!(matches!(err, FirError::CutoffsUnordered { .. }), "{err:?}");
    }
}

#[test]
fn a_transition_that_does_not_fit_is_refused_with_the_widest_that_does() {
    let design = |response, t| FirDesign::new(response, hz(t)).design(rate());
    // Low edge 1 kHz: the band reaches 0 at 2 kHz wide.
    let err = design(
        FirResponse::LowPass {
            cutoff: hz(1_000.0),
        },
        2_100.0,
    )
    .unwrap_err();
    assert_eq!(
        err,
        FirError::TransitionTooWide {
            transition: hz(2_100.0),
            widest: hz(2_000.0)
        }
    );
    // Near Nyquist: 23 kHz leaves 1 kHz, so 2 kHz at most.
    let err = design(
        FirResponse::HighPass {
            cutoff: hz(23_000.0),
        },
        2_500.0,
    )
    .unwrap_err();
    assert!(matches!(err, FirError::TransitionTooWide { widest, .. } if widest == hz(2_000.0)));
    // Two edges 1 kHz apart: bands may not overlap.
    let band = FirResponse::BandPass {
        low: hz(5_000.0),
        high: hz(6_000.0),
    };
    assert!(design(band, 1_000.0).is_ok());
    let err = design(band, 1_001.0).unwrap_err();
    assert!(matches!(err, FirError::TransitionTooWide { widest, .. } if widest == hz(1_000.0)));
    // The half-band's edge is rate / 4.
    assert!(design(FirResponse::HalfBand, 24_000.0).is_ok());
    assert!(matches!(
        design(FirResponse::HalfBand, 24_100.0).unwrap_err(),
        FirError::TransitionTooWide { .. }
    ));
}

#[test]
fn a_non_positive_transition_or_attenuation_is_refused() {
    let low = FirResponse::LowPass {
        cutoff: hz(6_000.0),
    };
    for t in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        let err = FirDesign::new(low, hz(t)).design(rate()).unwrap_err();
        assert!(matches!(err, FirError::NonPositiveTransition(_)), "{err:?}");
    }
    for a in [0.0, -60.0, f64::NAN] {
        let err = FirDesign::new(low, hz(500.0))
            .attenuation(Db::new(a))
            .design(rate())
            .unwrap_err();
        assert!(
            matches!(err, FirError::NonPositiveAttenuation(_)),
            "{err:?}"
        );
    }
}

#[test]
fn a_filter_beyond_the_maximum_length_is_refused() {
    let err = FirDesign::new(
        FirResponse::LowPass {
            cutoff: hz(6_000.0),
        },
        hz(0.001),
    )
    .attenuation(Db::new(100.0))
    .design(rate())
    .unwrap_err();
    assert!(
        matches!(err, FirError::TooLong { max: MAX_LEN, .. }),
        "{err:?}"
    );
}

/// An attenuation past what f64 taps resolve (300 dB) is refused by name; 300 dB itself
/// designs.
#[test]
fn an_attenuation_past_the_maximum_is_refused() {
    let lp = |a| {
        FirDesign::new(
            FirResponse::LowPass {
                cutoff: Hz::new(4_000.0),
            },
            Hz::new(8_000.0),
        )
        .attenuation(Db::new(a))
        .design(SampleRate::SR_48K)
    };
    assert!(matches!(
        lp(10_000.0),
        Err(FirError::AttenuationTooHigh { .. })
    ));
    assert!(lp(300.0).is_ok());
}
