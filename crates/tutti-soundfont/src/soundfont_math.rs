// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_units::{Amplitude, Cents, Hz, Seconds};

/// The SoundFont units (SF2 2.04 section 9.3) as `tutti_units` quantities, in `f64`: a
/// generator is a raw integer, and a voice crosses to its float once, at its start.
#[non_exhaustive]
pub(crate) struct SoundFontMath {}

impl SoundFontMath {
    /// The level (-60 dB) below which a voice's envelope is dropped: tutti's
    /// threshold, not the spec's. A release time is the time to -100 dB, so a
    /// released voice is dropped at 0.6 of it.
    pub(crate) const NON_AUDIBLE: Amplitude = Amplitude::new(1.0e-3);

    /// A time in absolute timecents as seconds: `2^(x / 1200)`, the 1200th root
    /// of two per timecent.
    pub(crate) fn timecents_to_seconds(timecents: i32) -> Seconds {
        Seconds::new(Cents::new(f64::from(timecents)).to_pitch_ratio())
    }

    /// A frequency in absolute cents (section 9.3): 8.176 Hz at 0, an octave per 1200.
    pub(crate) fn cents_to_hertz(cents: i32) -> Hz {
        Hz::new(8.176 * Cents::new(f64::from(cents)).to_pitch_ratio())
    }

    /// A key-number scaling of a time (generators 39 and 40 and their volume and
    /// modulation siblings): the factor for `key`, `2^(cents (60 - key) / 1200)`.
    pub(crate) fn key_number_to_multiplying_factor(cents: i32, key: i32) -> f64 {
        Cents::new(f64::from(cents * (60 - key))).to_pitch_ratio()
    }
}

/// SF2 2.04 section 9.3 defines the units generators are written in. The
/// figures below are the definitions' own, not another render's.
#[cfg(test)]
mod tests {
    use super::SoundFontMath;

    fn close(a: f64, b: f64, relative: f64) -> bool {
        ((a - b) / b).abs() < relative
    }

    /// 9.3, absolute timecents: zero is one second, and a timecent is the 1200th
    /// root of two in time, so 1200 doubles it and -1200 halves it.
    /// Spec: sf2.units.timecent.absolute, sf2.synth.lfo.delay-example
    #[test]
    fn timecents_are_one_second_at_zero_and_double_every_1200() {
        let seconds = |timecents| SoundFontMath::timecents_to_seconds(timecents).get();
        assert_eq!(seconds(0), 1.0);
        assert!(close(seconds(1200), 2.0, 1e-14));
        assert!(close(seconds(-1200), 0.5, 1e-14));
        // 9.3's own example, in the generator table: -7973 timecents is 10 ms.
        assert!(close(seconds(-7973), 0.01, 1e-3));
    }

    /// 9.3, absolute cents: 6900 is 440 Hz (A-440), and 1200 cents is an octave.
    /// Spec: sf2.units.cent.absolute
    #[test]
    fn absolute_cents_6900_is_440_hz_and_an_octave_is_1200() {
        let hertz = |cents| SoundFontMath::cents_to_hertz(cents).get();
        assert!(close(hertz(6900), 440.0, 1e-3));
        assert!(close(hertz(8100) / hertz(6900), 2.0, 1e-14));
    }

    /// 8.1.2, generators 31, 32, 39 and 40 (key number to envelope hold and
    /// decay): the time at key 60 is unchanged, and a value of 100 makes it
    /// track the keyboard (an octave up halves it). The spec's own worked
    /// example: a hold of -7973 timecents (10 ms) with a key-number-to-hold of
    /// 50, played at key 36, is 20 ms.
    /// Spec: sf2.synth.env.keynum-tracking, sf2.synth.env.keynum-example
    #[test]
    fn key_number_scaling_follows_the_spec_example() {
        let base = SoundFontMath::timecents_to_seconds(-7973).get();
        let factor = SoundFontMath::key_number_to_multiplying_factor(50, 36);
        assert!(close(base * factor, 0.02, 1e-3), "{}", base * factor);
    }
}
