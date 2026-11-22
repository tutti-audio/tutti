use super::*;
use crate::{GlideTime, PortamentoMode, UnisonVoices};
use tutti_fixtures::{RangeContract, check_range_contract};

/// **A starting master volume is a finite gain of at least zero**: silence is valid, a
/// negative gain, NaN and the infinities are refused rather than read as 0.
#[test]
fn master_volume_takes_every_finite_gain_from_zero() {
    check_range_contract(&RangeContract {
        new: MasterVolume::new,
        get: MasterVolume::get,
        of: Some(MasterVolume::of),
        valid: &[
            Amplitude::SILENT,
            Amplitude::new(0.5),
            Amplitude::UNITY,
            Amplitude::new(4.0),
        ],
        invalid: &[
            Amplitude::new(-0.001),
            Amplitude::new(f64::NAN),
            Amplitude::new(f64::INFINITY),
            Amplitude::new(f64::NEG_INFINITY),
        ],
    });
}

/// **The SVF's Q runs from 0.01 to 1000**, the range its coefficients are solved and stable
/// in: both ends are valid, and a Q at or below zero, past the ceiling, or NaN is refused.
#[test]
fn svf_q_takes_exactly_the_stable_range() {
    check_range_contract(&RangeContract {
        new: SvfQ::new,
        get: SvfQ::get,
        of: Some(SvfQ::of),
        valid: &[
            Q::new(SvfControls::MIN_Q),
            Q::BUTTERWORTH,
            Q::new(SvfControls::MAX_Q),
        ],
        invalid: &[
            Q::new(0.0),
            Q::new(-1.0),
            Q::new(0.009_999),
            Q::new(1_000.001),
            Q::new(f64::NAN),
            Q::new(f64::INFINITY),
        ],
    });
}

/// **A fresh config is the documented starting synth**: 8 poly saws with no filter, glide
/// or unison, the oldest voice stolen first, equal temperament, a whole-tone bend, no
/// filter modulation, the first Group, the default device id and unity volume; the glide
/// and unison it grows are off and a stack of one.
#[test]
fn new_configs_are_the_documented_starting_synth() {
    let c = SynthConfig::new();
    assert_eq!(c.polyphony.get(), 8);
    assert_eq!(c.voice_mode, VoiceMode::Poly);
    assert_eq!(c.oscillator, OscillatorType::Saw);
    assert_eq!(c.filter, FilterType::None);
    assert_eq!(c.ladder_saturation, LadderSaturation::PerStage);
    assert_eq!(c.envelope, Adsr::default());
    assert_eq!(c.portamento, None);
    assert_eq!(c.unison, None);
    assert_eq!(c.allocation_strategy, AllocationStrategy::Oldest);
    assert_eq!(c.tuning, Tuning::equal_temperament());
    assert_eq!(c.pitch_bend_range, Semitones::new(2.0));
    assert_eq!(c.group, MidiGroup::FIRST);
    assert_eq!(c.device, SysexDeviceId::DEFAULT);
    assert_eq!(c.volume, MasterVolume::UNITY);

    let m = c.filter_mod;
    assert_eq!(m.mod_wheel_depth, Depth::of(0.0));
    assert_eq!(m.velocity_depth, Depth::of(0.0));
    assert_eq!(m.lfo_rate, None);
    assert_eq!(m.lfo_depth, Depth::of(0.0));

    let p = PortamentoConfig::new();
    assert_eq!(p.mode, PortamentoMode::Off);
    assert_eq!(p.curve, tutti_curve::Law::Linear);
    assert_eq!(p.time, GlideTime::of(tutti_units::Seconds::new(0.1)));
    assert!(p.constant_time);

    let u = UnisonConfig::new();
    assert_eq!(u.voice_count, UnisonVoices::ONE);
    assert_eq!(u.detune_cents, tutti_units::CentsSpan::ZERO);
    assert_eq!(u.stereo_spread, tutti_units::Spread::POINT);
    assert!(!u.phase_randomize);
}
