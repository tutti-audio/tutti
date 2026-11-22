use super::*;
use tutti_dsp::{BinCount, FrameCount};

/// A key's own profile, as the chroma a perfect rendition would have.
fn profile_chroma(tonic: usize, mode: Mode, profile: KeyProfile) -> [f64; CLASSES] {
    let r = profile.ratings(mode);
    core::array::from_fn(|i| r[(i + CLASSES - tonic) % CLASSES])
}

fn rotate(v: &[f64; CLASSES], by: usize) -> [f64; CLASSES] {
    core::array::from_fn(|i| v[(i + CLASSES - by) % CLASSES])
}

/// Each of the 24 profiles, rendered exactly, names its own key with
/// confidence 1 (the correlation is 1 and the runner-up's is below it).
#[test]
fn a_profile_names_its_own_key_with_full_confidence() {
    for profile in [KeyProfile::Krumhansl, KeyProfile::Temperley] {
        for mode in [Mode::Major, Mode::Minor] {
            for tonic in 0..CLASSES {
                let est =
                    estimate_key_from_mean(profile, &profile_chroma(tonic, mode, profile)).unwrap();
                assert_eq!(
                    est.key,
                    key_at(tonic + if mode == Mode::Minor { CLASSES } else { 0 })
                );
                assert!((est.confidence.get() - 1.0).abs() < 1e-12);
                assert_ne!(est.runner_up, est.key);
            }
        }
    }
}

/// Rotating the chroma by `k` rotates the tonic by `k` and keeps the mode,
/// the confidence and the runner-up's mode; the sums run in the same order
/// over rotated terms, so the scores agree to f64 rounding (1e-9 is
/// millions of ulps of twelve terms of order one).
#[test]
fn rotating_the_chroma_rotates_the_key() {
    let base: [f64; CLASSES] = [0.9, 0.1, 0.5, 0.0, 0.6, 0.4, 0.1, 0.8, 0.0, 0.3, 0.1, 0.4];
    let first = estimate_key_from_mean(KeyProfile::Krumhansl, &base).unwrap();
    for k in 0..CLASSES {
        let est = estimate_key_from_mean(KeyProfile::Krumhansl, &rotate(&base, k)).unwrap();
        let tonic = (usize::from(first.key.tonic.semitones_above_c()) + k) % CLASSES;
        assert_eq!(est.key.tonic.semitones_above_c() as usize, tonic);
        assert_eq!(est.key.mode, first.key.mode);
        assert_eq!(est.runner_up.mode, first.runner_up.mode);
        assert!((est.confidence.get() - first.confidence.get()).abs() < 1e-12);
    }
}

/// The confidence is `(r1 - r2) / (1 - r2)` of the two best Pearson
/// correlations, here recomputed independently in f64.
#[test]
fn the_confidence_is_the_documented_margin() {
    let x: [f64; CLASSES] = [0.9, 0.1, 0.5, 0.0, 0.6, 0.4, 0.1, 0.8, 0.0, 0.3, 0.1, 0.4];
    let est = estimate_key_from_mean(KeyProfile::Temperley, &x).unwrap();
    let pearson = |tonic: usize, mode: Mode| {
        let p = KeyProfile::Temperley.ratings(mode);
        let q: Vec<f64> = (0..CLASSES)
            .map(|i| p[(i + CLASSES - tonic) % CLASSES])
            .collect();
        let a: Vec<f64> = x.to_vec();
        let (ma, mq) = (a.iter().sum::<f64>() / 12.0, q.iter().sum::<f64>() / 12.0);
        let cov: f64 = a.iter().zip(&q).map(|(a, q)| (a - ma) * (q - mq)).sum();
        let va: f64 = a.iter().map(|a| (a - ma).powi(2)).sum();
        let vq: f64 = q.iter().map(|q| (q - mq).powi(2)).sum();
        cov / (va * vq).sqrt()
    };
    let mut all: Vec<(f64, usize)> = (0..24)
        .map(|i| {
            (
                pearson(i % 12, if i < 12 { Mode::Major } else { Mode::Minor }),
                i,
            )
        })
        .collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    let want = (all[0].0 - all[1].0) / (1.0 - all[1].0);
    assert_eq!(est.key, key_at(all[0].1));
    assert_eq!(est.runner_up, key_at(all[1].1));
    assert!((est.confidence.get() - want).abs() < 1e-12);
}

/// A diatonic scale is close to its relative minor, so its confidence is
/// lower than a profile's exact one; mixing in a second key lowers it more.
#[test]
fn a_blurred_chroma_is_less_certain() {
    let pure = profile_chroma(0, Mode::Major, KeyProfile::Krumhansl);
    let other = profile_chroma(7, Mode::Major, KeyProfile::Krumhansl);
    let blend: [f64; CLASSES] = core::array::from_fn(|i| pure[i] + other[i]);
    let c = |x| {
        estimate_key_from_mean(KeyProfile::Krumhansl, x)
            .unwrap()
            .confidence
    };
    assert!(c(&blend) < c(&pure));
    let scale = [1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0];
    assert!(c(&scale) < Confidence::new(0.9));
}

/// Silence, a flat chroma and no frames are `NoTonalContent`, never a key;
/// a chroma barely off flat is a key.
#[test]
fn silence_and_flat_chroma_are_errors() {
    let bad = |x: [f64; CLASSES]| estimate_key_from_mean(KeyProfile::Krumhansl, &x).unwrap_err();
    assert_eq!(bad([0.0; CLASSES]), KeyEstimateError::NoTonalContent);
    assert_eq!(bad([0.3; CLASSES]), KeyEstimateError::NoTonalContent);
    assert_eq!(bad([1e-30; CLASSES]), KeyEstimateError::NoTonalContent);
    // One ulp of spread on 0.3 is rounding, not a key.
    let mut ulp = [0.3f64; CLASSES];
    ulp[0] = 0.3 + 0.3 * f64::EPSILON;
    assert_eq!(bad(ulp), KeyEstimateError::NoTonalContent);
    let mut tilted = [0.5f64; CLASSES];
    tilted[0] = 0.51;
    assert!(estimate_key_from_mean(KeyProfile::Krumhansl, &tilted).is_ok());
    let none = Grid::try_from((Vec::new(), FrameCount(0), BinCount(CLASSES))).unwrap();
    assert_eq!(
        estimate_key(KeyProfile::Krumhansl, &none).unwrap_err(),
        KeyEstimateError::NoTonalContent
    );
}

/// NaN, infinity, a negative value and a wrong width are refused.
#[test]
fn bad_chroma_is_refused() {
    let mut x = [0.5f64; CLASSES];
    x[3] = f64::NAN;
    let e = |x: &[f64; CLASSES]| estimate_key_from_mean(KeyProfile::Krumhansl, x).unwrap_err();
    assert_eq!(e(&x), KeyEstimateError::NonFiniteInput);
    x[3] = f64::INFINITY;
    assert_eq!(e(&x), KeyEstimateError::NonFiniteInput);
    x[3] = -0.1;
    assert_eq!(e(&x), KeyEstimateError::NegativeInput);
    let wide = Grid::try_from((vec![0.5; 13], FrameCount(1), BinCount(13))).unwrap();
    assert_eq!(
        estimate_key(KeyProfile::Krumhansl, &wide),
        Err(KeyEstimateError::NotTwelveClasses { columns: 13 })
    );
}

/// The grid form averages its frames: a loud frame of one key and a quiet
/// one of another read as the mean, and the order of frames is irrelevant.
#[test]
fn a_grid_is_read_as_its_mean() {
    let a = profile_chroma(0, Mode::Major, KeyProfile::Krumhansl);
    let b = profile_chroma(5, Mode::Minor, KeyProfile::Krumhansl);
    let rows = |first: &[f64; 12], second: &[f64; 12]| {
        let data = [first.as_slice(), second.as_slice(), second.as_slice()].concat();
        Grid::try_from((data, FrameCount(3), BinCount(CLASSES))).unwrap()
    };
    let est = estimate_key(KeyProfile::Krumhansl, &rows(&a, &b)).unwrap();
    assert_eq!(
        est.key,
        Key {
            tonic: PitchClass::F,
            mode: Mode::Minor
        }
    );
    let swapped = estimate_key(KeyProfile::Krumhansl, &rows(&b, &a)).unwrap();
    assert_eq!(
        swapped.key,
        Key {
            tonic: PitchClass::C,
            mode: Mode::Major
        }
    );
    assert_eq!(est.key.to_string(), "F minor");
}

/// A meter of a C major triad reads C major, in any chunking.
#[test]
fn the_key_meter_reads_c_major_from_a_c_major_triad() {
    let rate = SampleRate::of(22_050.0);
    let x = crate::test_rng::tones(22_050.0, &[261.626, 329.628, 391.995], 44_100);
    let cfg = KeyConfig::new(KeyProfile::Krumhansl);
    let est = crate::measure::chunk_independent(&cfg, rate, &x).unwrap();
    assert_eq!(
        est.key,
        Key {
            tonic: PitchClass::C,
            mode: Mode::Major
        }
    );
}
