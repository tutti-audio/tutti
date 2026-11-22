//! Every range-checked setting keeps the range contract: `new` refuses
//! and never clamps, `of` panics, `TryFrom` agrees and carries the refused raw.
//!
//! The crate has no serde, so the check runs on the `try_from` shape written
//! here for the tests alone.

use serde::{Deserialize, Serialize};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_units::{Amplitude, Cents, Confidence, Db, Samples, Seconds};

use crate::{
    Alternatives, BandCount, BandsPerOctave, FiniteLevel, HarmonicCount, HarmonicDecay, NoteSpan,
    OnsetGap, OnsetSensitivity, OnsetThreshold, PeakDecay, PeakFloor, PitchHop, PitchSplit,
    PriorSpread, PyinSwitch, PyinThresholdMean, PyinTransitionWidth, SelfTransition, Sharpness,
    SpectrumFloor, SpectrumTime, Tightness, TuningOffset, YinThreshold,
};

macro_rules! serde_through_try_from {
    ($($ty:ty: $raw:ty),* $(,)?) => {$(
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.get().serialize(s)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::try_from(<$raw>::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    )*};
}

serde_through_try_from! {
    HarmonicCount: usize,
    HarmonicDecay: f64,
    PeakFloor: Db,
    TuningOffset: Cents,
    PriorSpread: f64,
    Tightness: f64,
    SpectrumTime: Seconds,
    SpectrumFloor: Db,
    PeakDecay: Db,
    BandsPerOctave: usize,
    BandCount: usize,
    NoteSpan: Seconds,
    PitchSplit: Cents,
    SelfTransition: f64,
    Sharpness: f64,
    OnsetThreshold: Amplitude,
    OnsetSensitivity: Amplitude,
    OnsetGap: Seconds,
    FiniteLevel: Db,
    PitchHop: Samples,
    Alternatives: usize,
    YinThreshold: Confidence,
    PyinThresholdMean: Confidence,
    PyinSwitch: Confidence,
    PyinTransitionWidth: Cents,
}

const NAN: f64 = f64::NAN;
const INF: f64 = f64::INFINITY;
const NEG_INF: f64 = f64::NEG_INFINITY;

fn db(raws: &[f64]) -> Vec<Db> {
    raws.iter().map(|&r| Db::new(r)).collect()
}

fn cents(raws: &[f64]) -> Vec<Cents> {
    raws.iter().map(|&r| Cents::new(r)).collect()
}

fn confidences(raws: &[f64]) -> Vec<Confidence> {
    raws.iter().map(|&r| Confidence::new(r)).collect()
}

fn amplitudes(raws: &[f64]) -> Vec<Amplitude> {
    raws.iter().map(|&r| Amplitude::new(r)).collect()
}

fn seconds(raws: &[f64]) -> Vec<Seconds> {
    raws.iter().map(|&r| Seconds::new(r)).collect()
}

/// `1..=16` partials.
#[test]
fn harmonics_keep_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: HarmonicCount::new,
        get: HarmonicCount::get,
        of: Some(HarmonicCount::of),
        valid: &[1, 4, 16],
        invalid: &[0, 17, usize::MAX],
    });
}

/// `(0, 1]`.
#[test]
fn harmonic_decay_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: HarmonicDecay::new,
        get: HarmonicDecay::get,
        of: Some(HarmonicDecay::of),
        valid: &[f64::MIN_POSITIVE, 0.6, 1.0],
        invalid: &[0.0, -0.1, 1.000_000_1, NAN, INF, NEG_INF],
    });
}

/// `-120..=0` dB.
#[test]
fn peak_floor_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PeakFloor::new,
        get: PeakFloor::get,
        of: Some(PeakFloor::of),
        valid: &db(&[-120.0, -40.0, 0.0]),
        invalid: &db(&[1.0, -121.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `-50..=50` cents.
#[test]
fn tuning_offset_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: TuningOffset::new,
        get: TuningOffset::get,
        of: Some(TuningOffset::of),
        valid: &cents(&[-50.0, 0.0, 12.0, 50.0]),
        invalid: &cents(&[51.0, -51.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// Above zero and finite.
#[test]
fn prior_spread_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PriorSpread::new,
        get: PriorSpread::get,
        of: Some(PriorSpread::of),
        valid: &[f64::MIN_POSITIVE, 1.0, f64::MAX],
        invalid: &[0.0, -1.0, NAN, INF, NEG_INF],
    });
}

/// Zero or more, finite.
#[test]
fn tightness_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Tightness::new,
        get: Tightness::get,
        of: Some(Tightness::of),
        valid: &[0.0, 100.0, f64::MAX],
        invalid: &[-1.0, -f64::MIN_POSITIVE, NAN, INF, NEG_INF],
    });
}

/// `0..=3600` s.
#[test]
fn spectrum_time_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SpectrumTime::new,
        get: SpectrumTime::get,
        of: Some(SpectrumTime::of),
        valid: &seconds(&[0.0, 1.0, 3600.0]),
        invalid: &seconds(&[-1.0, 3601.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `-300..=0` dB.
#[test]
fn spectrum_floor_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SpectrumFloor::new,
        get: SpectrumFloor::get,
        of: Some(SpectrumFloor::of),
        valid: &db(&[-300.0, -96.0, 0.0]),
        invalid: &db(&[1.0, -301.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `0..=10000` dB per second.
#[test]
fn peak_decay_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PeakDecay::new,
        get: PeakDecay::get,
        of: Some(PeakDecay::of),
        valid: &db(&[0.0, 20.0, 10_000.0]),
        invalid: &db(&[-1.0, 10_001.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `1..=48` bands.
#[test]
fn bands_per_octave_keep_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: BandsPerOctave::new,
        get: BandsPerOctave::get,
        of: Some(BandsPerOctave::of),
        valid: &[1, 3, 48],
        invalid: &[0, 49, usize::MAX],
    });
}

/// `1..=4096` bands.
#[test]
fn band_count_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: BandCount::new,
        get: BandCount::get,
        of: Some(BandCount::of),
        valid: &[1, 24, 4096],
        invalid: &[0, 4097, usize::MAX],
    });
}

/// `0..=60` s.
#[test]
fn note_span_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: NoteSpan::new,
        get: NoteSpan::get,
        of: Some(NoteSpan::of),
        valid: &seconds(&[0.0, 0.06, 60.0]),
        invalid: &seconds(&[-1.0, 61.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// `(0, 1200]` cents.
#[test]
fn pitch_split_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PitchSplit::new,
        get: PitchSplit::get,
        of: Some(PitchSplit::of),
        valid: &cents(&[f64::MIN_POSITIVE, 50.0, 1200.0]),
        invalid: &cents(&[
            0.0,
            -5.0,
            1201.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ]),
    });
}

/// `(0, 1)`.
#[test]
fn self_transition_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: SelfTransition::new,
        get: SelfTransition::get,
        of: Some(SelfTransition::of),
        valid: &[f64::MIN_POSITIVE, 0.9, 0.999_999_999_999_999_9],
        invalid: &[0.0, 1.0, -0.5, 1.5, NAN, INF, NEG_INF],
    });
}

/// `(0, 100]`.
#[test]
fn sharpness_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Sharpness::new,
        get: Sharpness::get,
        of: Some(Sharpness::of),
        valid: &[f64::MIN_POSITIVE, 8.0, 100.0],
        invalid: &[0.0, -1.0, 100.1, NAN, INF, NEG_INF],
    });
}

/// Zero or more, finite.
#[test]
fn onset_threshold_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: OnsetThreshold::new,
        get: OnsetThreshold::get,
        of: Some(OnsetThreshold::of),
        valid: &amplitudes(&[0.0, 0.3, 1.0, f64::MAX]),
        invalid: &amplitudes(&[-0.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// Above zero, finite.
#[test]
fn onset_sensitivity_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: OnsetSensitivity::new,
        get: OnsetSensitivity::get,
        of: Some(OnsetSensitivity::of),
        valid: &amplitudes(&[f64::MIN_POSITIVE, 1.0, 3.0, f64::MAX]),
        invalid: &amplitudes(&[0.0, -1.0, f64::NAN, f64::INFINITY]),
    });
}

/// `0..=60` seconds.
#[test]
fn onset_gap_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: OnsetGap::new,
        get: OnsetGap::get,
        of: Some(OnsetGap::of),
        valid: &seconds(&[0.0, 0.05, 60.0]),
        invalid: &seconds(&[-0.001, 60.001, f64::NAN, f64::INFINITY]),
    });
}

/// Finite decibels.
#[test]
fn gate_level_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: FiniteLevel::new,
        get: FiniteLevel::get,
        of: Some(FiniteLevel::of),
        valid: &db(&[-300.0, -60.0, 0.0, 12.0, f64::MAX, f64::MIN]),
        invalid: &db(&[f64::NAN, f64::INFINITY, f64::NEG_INFINITY]),
    });
}

/// One sample or more.
#[test]
fn pitch_hop_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PitchHop::new,
        get: PitchHop::get,
        of: Some(PitchHop::of),
        valid: &[Samples::new(1), Samples::new(512), Samples::new(usize::MAX)],
        invalid: &[Samples::new(0)],
    });
}

/// `0..=16` runner-up tempi.
#[test]
fn alternatives_keep_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: Alternatives::new,
        get: Alternatives::get,
        of: Some(Alternatives::of),
        valid: &[0, 2, 16],
        invalid: &[17, usize::MAX],
    });
}

/// `0.01..=0.5`.
#[test]
fn yin_threshold_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: YinThreshold::new,
        get: YinThreshold::get,
        of: Some(YinThreshold::of),
        valid: &[
            Confidence::new(0.01),
            Confidence::new(0.1),
            Confidence::new(0.5),
        ],
        invalid: &[
            Confidence::new(0.0),
            Confidence::new(0.009),
            Confidence::new(0.51),
            Confidence::new(f64::NAN),
        ],
    });
}

/// `0.02..=0.5`.
#[test]
fn pyin_threshold_mean_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PyinThresholdMean::new,
        get: PyinThresholdMean::get,
        of: Some(PyinThresholdMean::of),
        valid: &confidences(&[0.02, 0.1, 0.5]),
        invalid: &confidences(&[0.0, 0.019, 0.51, NAN, INF, NEG_INF]),
    });
}

/// `(0, 0.5]`.
#[test]
fn pyin_switch_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PyinSwitch::new,
        get: PyinSwitch::get,
        of: Some(PyinSwitch::of),
        valid: &confidences(&[f64::MIN_POSITIVE, 0.01, 0.5]),
        invalid: &confidences(&[0.0, 0.51, -0.1, NAN, INF, NEG_INF]),
    });
}

/// `10..=1200` cents.
#[test]
fn pyin_transition_width_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: PyinTransitionWidth::new,
        get: PyinTransitionWidth::get,
        of: Some(PyinTransitionWidth::of),
        valid: &cents(&[10.0, 250.0, 1200.0]),
        invalid: &cents(&[9.9, 1200.1, 0.0, NAN, INF, NEG_INF]),
    });
}
