use core::f64::consts::{FRAC_1_SQRT_2, PI, SQRT_2};

use super::*;
use tutti_units::Q;

const RATE: SampleRate = SampleRate::SR_48K;
const NYQUIST: f64 = 24_000.0;
const CENTERS: [f64; 5] = [60.0, 440.0, 1_000.0, 6_000.0, 19_000.0];
const QS: [f64; 5] = [0.3, FRAC_1_SQRT_2, 1.0, 4.0, 20.0];
const GAINS: [f64; 6] = [-24.0, -12.0, -3.0, 3.0, 12.0, 24.0];
const PROBES: [f64; 8] = [
    20.0, 90.0, 400.0, 1_000.0, 3_100.0, 9_000.0, 17_000.0, 23_000.0,
];

fn response(sos: &Sos, f: f64) -> Complex64 {
    let mut h = [Complex64::new(0.0, 0.0)];
    sos.frequency_response(0, &[Hz::new(f)], &mut h);
    h[0]
}

fn design(filter: Cookbook, center: f64) -> Sos {
    filter.design(Hz::new(center), RATE).unwrap()
}

/// `|H|` of the design at `f`.
fn magnitude(filter: Cookbook, center: f64, f: f64) -> f64 {
    response(&design(filter, center), f).norm()
}

fn amplitude(db: f64) -> f64 {
    Db::new(db).to_amplitude().get()
}

/// The cookbook's low-pass is `Q` at the center (`-3.0103 dB` at `Q = 1/sqrt(2)`), 1 at DC
/// and 0 at Nyquist; the high-pass is the mirror.
#[test]
fn low_pass_and_high_pass_are_q_at_the_center_and_unity_and_zero_at_the_ends() {
    for fc in CENTERS {
        for q in QS {
            let q_ = Q::new(q);
            let lp = design(Cookbook::LowPass { q: q_ }, fc);
            let hp = design(Cookbook::HighPass { q: q_ }, fc);
            assert!(
                (response(&lp, fc).norm() - q).abs() < 1e-9 * q,
                "lp {fc} {q}"
            );
            assert!(
                (response(&hp, fc).norm() - q).abs() < 1e-9 * q,
                "hp {fc} {q}"
            );
            assert!((response(&lp, 0.0).norm() - 1.0).abs() < 1e-9);
            assert!(response(&lp, NYQUIST).norm() < 1e-9);
            assert!(response(&hp, 0.0).norm() < 1e-9);
            assert!((response(&hp, NYQUIST).norm() - 1.0).abs() < 1e-9);
        }
    }
    let db = 20.0
        * magnitude(
            Cookbook::LowPass {
                q: Q::new(FRAC_1_SQRT_2),
            },
            1_000.0,
            1_000.0,
        )
        .log10();
    assert!((db + 3.0103).abs() < 1e-4, "{db}");
}

/// The constant-peak band-pass is exactly 1 at the center and 0 at DC and Nyquist.
#[test]
fn band_pass_peaks_at_0_db_at_the_center() {
    for fc in CENTERS {
        for q in QS {
            let bp = design(Cookbook::BandPass { q: Q::new(q) }, fc);
            assert!((response(&bp, fc).norm() - 1.0).abs() < 1e-9, "{fc} {q}");
            assert!(response(&bp, 0.0).norm() < 1e-9);
            assert!(response(&bp, NYQUIST).norm() < 1e-9);
        }
    }
}

/// The notch is a zero at the center and 1 at DC and Nyquist.
#[test]
fn notch_is_zero_at_the_center_and_unity_at_the_ends() {
    for fc in CENTERS {
        for q in QS {
            let n = design(Cookbook::Notch { q: Q::new(q) }, fc);
            assert!(response(&n, fc).norm() < 1e-9, "{fc} {q}");
            assert!((response(&n, 0.0).norm() - 1.0).abs() < 1e-9);
            assert!((response(&n, NYQUIST).norm() - 1.0).abs() < 1e-9);
        }
    }
}

/// The all-pass has unit magnitude everywhere and a phase of `-pi` at the center: `H = -1`.
#[test]
fn all_pass_is_unity_everywhere_with_phase_minus_pi_at_the_center() {
    for fc in CENTERS {
        for q in QS {
            let ap = design(Cookbook::AllPass { q: Q::new(q) }, fc);
            for f in PROBES {
                assert!((response(&ap, f).norm() - 1.0).abs() < 1e-9, "{fc} {q} {f}");
            }
            let h = response(&ap, fc);
            assert!(
                (h.re + 1.0).abs() < 1e-9 && h.im.abs() < 1e-9,
                "{fc} {q}: {h}"
            );
            // Below the center the phase is above -pi, above it the phase has wound past -pi.
            let below = response(&ap, fc * 0.9).arg();
            assert!(below < 0.0 && below > -PI, "{fc} {q}: {below}");
        }
    }
}

/// The peaking bell is `A^2` (the gain) at the center and 1 at DC and Nyquist.
#[test]
fn peaking_is_the_gain_at_the_center_and_unity_at_the_ends() {
    for fc in CENTERS {
        for q in QS {
            for db in GAINS {
                let bell = design(
                    Cookbook::Peaking {
                        q: Q::new(q),
                        gain: Db::new(db),
                    },
                    fc,
                );
                let want = amplitude(db);
                assert!(
                    (response(&bell, fc).norm() - want).abs() < 1e-9 * want,
                    "{fc} {q} {db}"
                );
                assert!((response(&bell, 0.0).norm() - 1.0).abs() < 1e-9);
                assert!((response(&bell, NYQUIST).norm() - 1.0).abs() < 1e-9);
            }
        }
    }
}

/// The low shelf is the gain at DC and 1 at Nyquist; the high shelf is the mirror.
#[test]
fn shelves_are_the_gain_at_their_end_and_unity_at_the_other() {
    for fc in CENTERS {
        for q in QS {
            for db in GAINS {
                let want = amplitude(db);
                let low = design(
                    Cookbook::LowShelf {
                        q: Q::new(q),
                        gain: Db::new(db),
                    },
                    fc,
                );
                let high = design(
                    Cookbook::HighShelf {
                        q: Q::new(q),
                        gain: Db::new(db),
                    },
                    fc,
                );
                assert!(
                    (response(&low, 0.0).norm() - want).abs() < 1e-9 * want,
                    "low {fc} {q} {db}"
                );
                assert!((response(&low, NYQUIST).norm() - 1.0).abs() < 1e-9);
                assert!(
                    (response(&high, NYQUIST).norm() - want).abs() < 1e-9 * want,
                    "high {fc} {q} {db}"
                );
                assert!((response(&high, 0.0).norm() - 1.0).abs() < 1e-9);
            }
        }
    }
}

/// At 0 dB every gain design is exactly a wire: numerator equals denominator.
#[test]
fn the_gain_designs_at_zero_db_are_wires() {
    let q = Q::new(2.0);
    let gain = Db::UNITY;
    for filter in [
        Cookbook::Peaking { q, gain },
        Cookbook::LowShelf { q, gain },
        Cookbook::HighShelf { q, gain },
    ] {
        let b = design(filter, 3_000.0).sections()[0];
        assert_eq!((b.b0, b.b1, b.b2), (1.0, b.a1, b.a2), "{filter:?}");
    }
}

/// A center outside `(0, Nyquist)`, a quality factor that is not above 0 and a gain past the
/// limit fail at the design with their own error.
#[test]
fn a_design_refuses_a_center_a_q_and_a_gain_out_of_range() {
    let q = Q::new(1.0);
    for center in [0.0, -1.0, NYQUIST, 30_000.0, f64::NAN] {
        let r = Cookbook::LowPass { q }.design(Hz::new(center), RATE);
        assert!(
            matches!(r, Err(SosError::CutoffOutOfRange { .. })),
            "{center}"
        );
    }
    for q in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let r = Cookbook::Notch { q: Q::new(q) }.design(Hz::new(1_000.0), RATE);
        assert!(matches!(r, Err(SosError::QOutOfRange { .. })), "{q}");
    }
    let r = Cookbook::Peaking {
        q,
        gain: Db::new(121.0),
    }
    .design(Hz::new(1_000.0), RATE);
    assert!(matches!(r, Err(SosError::GainOutOfRange { .. })));
}

/// The octave bands' `Q` is `sqrt(2)` (`sinh(ln(2) / 2) = 1 / (2 sqrt(2))`) and the third-octave
/// bands' is `1 / (2^(1/6) - 2^(-1/6))`.
#[test]
fn band_q_follows_the_spacing() {
    assert!((GraphicBands::Octave.q().get() - SQRT_2).abs() < 1e-12);
    let third = 1.0 / (2f64.powf(1.0 / 6.0) - 2f64.powf(-1.0 / 6.0));
    assert!((GraphicBands::ThirdOctave.q().get() - third).abs() < 1e-12);
}

/// The centers rise, count 10 and 31, and sit within 2% of the exact base-2 series about
/// 1 kHz that ISO 266 rounds; the octave centers are every third third-octave one.
#[test]
fn the_centers_are_the_iso_266_series() {
    let third = GraphicBands::ThirdOctave.centers();
    let octave = GraphicBands::Octave.centers();
    assert_eq!((octave.len(), third.len()), (10, 31));
    assert_eq!(GraphicBands::Octave.count(), 10);
    assert_eq!(GraphicBands::MAX_COUNT, 31);
    for (i, f) in third.iter().enumerate() {
        let exact = 1_000.0 * 2f64.powf((i as f64 - 17.0) / 3.0);
        assert!(
            (f.get() / exact - 1.0).abs() < 0.02,
            "{i}: {f:?} vs {exact}"
        );
    }
    assert!(third.windows(2).all(|w| w[0] < w[1]));
    for (k, f) in octave.iter().enumerate() {
        assert_eq!(*f, third[2 + 3 * k]);
    }
}

fn gains_with(band: usize, db: f64) -> [Db; GraphicBands::MAX_COUNT] {
    let mut gains = [Db::UNITY; GraphicBands::MAX_COUNT];
    gains[band] = Db::new(db);
    gains
}

/// With every other band at 0 dB (a wire), a band's response at its center is its gain.
#[test]
fn each_band_is_its_gain_at_its_center_with_the_others_flat() {
    for rate in [RATE, SampleRate::SR_44K1] {
        for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
            for (i, center) in bands.centers().iter().enumerate() {
                for db in [-12.0, 6.0, 12.0] {
                    let sos = bands.design(&gains_with(i, db), rate).unwrap();
                    assert_eq!(sos.sections().len(), bands.count());
                    let mut h = [Complex64::new(0.0, 0.0)];
                    sos.frequency_response(0, &[*center], &mut h);
                    let want = amplitude(db);
                    assert!(
                        (h[0].norm() - want).abs() < 1e-9 * want,
                        "{bands:?} band {i} {db} dB at {rate:?}: {}",
                        h[0].norm()
                    );
                }
            }
        }
    }
}

/// All gains at 0 dB is a flat response: unit magnitude at every probe.
#[test]
fn zero_gains_are_flat() {
    for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
        let sos = bands
            .design(&[Db::UNITY; GraphicBands::MAX_COUNT], RATE)
            .unwrap();
        for f in PROBES
            .into_iter()
            .chain(bands.centers().iter().map(|c| c.get()))
        {
            assert!(
                (response(&sos, f).norm() - 1.0).abs() < 1e-9,
                "{bands:?} {f}"
            );
        }
    }
}

/// A band whose center is not below Nyquist is a wire whatever its gain, and the section
/// count is still the band count.
#[test]
fn a_band_at_or_above_nyquist_is_a_wire() {
    let rate = SampleRate::of(32_000.0);
    let bands = GraphicBands::ThirdOctave;
    let top = bands.count() - 1;
    let sos = bands.design(&gains_with(top, 24.0), rate).unwrap();
    assert_eq!(sos.sections().len(), bands.count());
    for f in PROBES {
        let mut h = [Complex64::new(0.0, 0.0)];
        sos.frequency_response(0, &[Hz::new(f.min(15_000.0))], &mut h);
        assert!((h[0].norm() - 1.0).abs() < 1e-9, "{f}");
    }
}

/// A gain past the limit in a read band fails the design; one in a band the spacing does not
/// have is not read.
#[test]
fn the_design_reads_only_the_bands_it_has() {
    let bad = Db::new(200.0);
    let r = GraphicBands::Octave.design(&gains_with(3, 200.0), RATE);
    assert!(matches!(r, Err(SosError::GainOutOfRange { gain }) if gain == bad));
    assert!(
        GraphicBands::Octave
            .design(&gains_with(20, 200.0), RATE)
            .is_ok()
    );
}
