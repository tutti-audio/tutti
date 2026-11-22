//! Every range-checked setting keeps the range contract: `new` refuses
//! and never clamps, `of` panics, `TryFrom` agrees and carries the refused raw.
//! One row per type: its test name, the range, then the raws it takes and refuses.

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_units::{Amplitude, Cents, Confidence, Db, Samples, Seconds};

use crate::{
    Alternatives, BandCount, BandsPerOctave, FiniteLevel, HarmonicCount, HarmonicDecay, NoteSpan,
    OnsetGap, OnsetSensitivity, OnsetThreshold, PeakDecay, PeakFloor, PitchHop, PitchSplit,
    PriorSpread, PyinSwitch, PyinThresholdMean, PyinTransitionWidth, SelfTransition, Sharpness,
    SpectrumFloor, SpectrumTime, Tightness, TuningOffset, YinThreshold,
};

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

/// A test per row: `$ty` keeps the range-checked shape over `valid` and `invalid`.
macro_rules! range_contracts {
    ($($(#[$doc:meta])* $name:ident: $ty:ty, valid $valid:expr, invalid $invalid:expr;)*) => {$(
        $(#[$doc])*
        #[test]
        fn $name() {
            check_range_contract(&RangeContract {
                new: <$ty>::new,
                get: <$ty>::get,
                of: Some(<$ty>::of),
                valid: &$valid,
                invalid: &$invalid,
            });
        }
    )*};
}

range_contracts! {
    /// `1..=16` partials.
    harmonics_keep_the_range_checked_shape: HarmonicCount, valid [1, 4, 16], invalid [0, 17, usize::MAX];
    /// `(0, 1]`.
    harmonic_decay_keeps_the_range_checked_shape: HarmonicDecay, valid [f64::MIN_POSITIVE, 0.6, 1.0], invalid [0.0, -0.1, 1.000_000_1, NAN, INF, NEG_INF];
    /// `-120..=0` dB.
    peak_floor_keeps_the_range_checked_shape: PeakFloor, valid db(&[-120.0, -40.0, 0.0]), invalid db(&[1.0, -121.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// `-50..=50` cents.
    tuning_offset_keeps_the_range_checked_shape: TuningOffset, valid cents(&[-50.0, 0.0, 12.0, 50.0]), invalid cents(&[51.0, -51.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// Above zero and finite.
    prior_spread_keeps_the_range_checked_shape: PriorSpread, valid [f64::MIN_POSITIVE, 1.0, f64::MAX], invalid [0.0, -1.0, NAN, INF, NEG_INF];
    /// Zero or more, finite.
    tightness_keeps_the_range_checked_shape: Tightness, valid [0.0, 100.0, f64::MAX], invalid [-1.0, -f64::MIN_POSITIVE, NAN, INF, NEG_INF];
    /// `0..=3600` s.
    spectrum_time_keeps_the_range_checked_shape: SpectrumTime, valid seconds(&[0.0, 1.0, 3600.0]), invalid seconds(&[-1.0, 3601.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// `-300..=0` dB.
    spectrum_floor_keeps_the_range_checked_shape: SpectrumFloor, valid db(&[-300.0, -96.0, 0.0]), invalid db(&[1.0, -301.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// `0..=10000` dB per second.
    peak_decay_keeps_the_range_checked_shape: PeakDecay, valid db(&[0.0, 20.0, 10_000.0]), invalid db(&[-1.0, 10_001.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// `1..=48` bands.
    bands_per_octave_keep_the_range_checked_shape: BandsPerOctave, valid [1, 3, 48], invalid [0, 49, usize::MAX];
    /// `1..=4096` bands.
    band_count_keeps_the_range_checked_shape: BandCount, valid [1, 24, 4096], invalid [0, 4097, usize::MAX];
    /// `0..=60` s.
    note_span_keeps_the_range_checked_shape: NoteSpan, valid seconds(&[0.0, 0.06, 60.0]), invalid seconds(&[-1.0, 61.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// `(0, 1200]` cents.
    pitch_split_keeps_the_range_checked_shape: PitchSplit, valid cents(&[f64::MIN_POSITIVE, 50.0, 1200.0]), invalid cents(&[
            0.0,
            -5.0,
            1201.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ]);
    /// `(0, 1)`.
    self_transition_keeps_the_range_checked_shape: SelfTransition, valid [f64::MIN_POSITIVE, 0.9, 0.999_999_999_999_999_9], invalid [0.0, 1.0, -0.5, 1.5, NAN, INF, NEG_INF];
    /// `(0, 100]`.
    sharpness_keeps_the_range_checked_shape: Sharpness, valid [f64::MIN_POSITIVE, 8.0, 100.0], invalid [0.0, -1.0, 100.1, NAN, INF, NEG_INF];
    /// Zero or more, finite.
    onset_threshold_keeps_the_range_checked_shape: OnsetThreshold, valid amplitudes(&[0.0, 0.3, 1.0, f64::MAX]), invalid amplitudes(&[-0.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// Above zero, finite.
    onset_sensitivity_keeps_the_range_checked_shape: OnsetSensitivity, valid amplitudes(&[f64::MIN_POSITIVE, 1.0, 3.0, f64::MAX]), invalid amplitudes(&[0.0, -1.0, f64::NAN, f64::INFINITY]);
    /// `0..=60` seconds.
    onset_gap_keeps_the_range_checked_shape: OnsetGap, valid seconds(&[0.0, 0.05, 60.0]), invalid seconds(&[-0.001, 60.001, f64::NAN, f64::INFINITY]);
    /// Finite decibels.
    gate_level_keeps_the_range_checked_shape: FiniteLevel, valid db(&[-300.0, -60.0, 0.0, 12.0, f64::MAX, f64::MIN]), invalid db(&[f64::NAN, f64::INFINITY, f64::NEG_INFINITY]);
    /// One sample or more.
    pitch_hop_keeps_the_range_checked_shape: PitchHop, valid [Samples::new(1), Samples::new(512), Samples::new(usize::MAX)], invalid [Samples::new(0)];
    /// `0..=16` runner-up tempi.
    alternatives_keep_the_range_checked_shape: Alternatives, valid [0, 2, 16], invalid [17, usize::MAX];
    /// `0.01..=0.5`.
    yin_threshold_keeps_the_range_checked_shape: YinThreshold, valid [
            Confidence::new(0.01),
            Confidence::new(0.1),
            Confidence::new(0.5),
        ], invalid [
            Confidence::new(0.0),
            Confidence::new(0.009),
            Confidence::new(0.51),
            Confidence::new(f64::NAN),
        ];
    /// `0.02..=0.5`.
    pyin_threshold_mean_keeps_the_range_checked_shape: PyinThresholdMean, valid confidences(&[0.02, 0.1, 0.5]), invalid confidences(&[0.0, 0.019, 0.51, NAN, INF, NEG_INF]);
    /// `(0, 0.5]`.
    pyin_switch_keeps_the_range_checked_shape: PyinSwitch, valid confidences(&[f64::MIN_POSITIVE, 0.01, 0.5]), invalid confidences(&[0.0, 0.51, -0.1, NAN, INF, NEG_INF]);
    /// `10..=1200` cents.
    pyin_transition_width_keeps_the_range_checked_shape: PyinTransitionWidth, valid cents(&[10.0, 250.0, 1200.0]), invalid cents(&[9.9, 1200.1, 0.0, NAN, INF, NEG_INF]);
}
