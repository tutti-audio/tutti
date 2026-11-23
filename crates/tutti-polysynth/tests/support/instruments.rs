//! The four instrument voices a test plays, each with a config that rings: a body of two
//! modes, two strings, and four harmonics.

use std::num::NonZeroUsize;

use tutti_dsp::{Mode, Modes, Partial, Partials};
use tutti_polysynth::{
    AdditiveVoiceConfig, InstrumentConfig, InstrumentVoice, KarplusVoiceConfig, ModalVoiceConfig,
    NoteInstrument, WaveguideVoiceConfig,
};
use tutti_units::{Amplitude, FrequencyRatio, Gain, Hz, Seconds};
use tutti_voicing::VoiceConfig;

use super::Hand;

pub fn modal() -> ModalVoiceConfig {
    let mode = |frequency, decay, gain| Mode {
        frequency: Hz::new(frequency),
        decay: Seconds::new(decay),
        gain: Gain::new(gain),
    };
    let modes = [mode(440.0, 3.0, 1.0), mode(880.0, 1.5, 0.5)];
    ModalVoiceConfig::new(Modes::try_from(&modes[..]).unwrap())
}

pub fn karplus() -> KarplusVoiceConfig {
    KarplusVoiceConfig::new()
}

pub fn waveguide() -> WaveguideVoiceConfig {
    WaveguideVoiceConfig::new()
}

pub fn additive() -> AdditiveVoiceConfig {
    let partial = |ratio, amplitude| {
        Partial::new(
            FrequencyRatio::new(ratio).unwrap(),
            Amplitude::new(amplitude),
        )
    };
    let partials = [
        partial(1.0, 1.0),
        partial(2.0, 0.5),
        partial(3.0, 0.25),
        partial(4.0, 0.125),
    ];
    AdditiveVoiceConfig::new(Partials::try_from(&partials[..]).unwrap())
}

/// Calls `$check::<Voice>(name, config)` for each of the four voices.
macro_rules! each_voice {
    ($check:ident) => {{
        use tutti_polysynth::{AdditiveVoice, KarplusVoice, ModalVoice, WaveguideVoice};
        $check::<ModalVoice<f32>>("modal", $crate::support::instruments::modal());
        $check::<WaveguideVoice<f32>>("waveguide", $crate::support::instruments::waveguide());
        $check::<KarplusVoice<f32>>("karplus", $crate::support::instruments::karplus());
        $check::<AdditiveVoice<f32>>("additive", $crate::support::instruments::additive());
    }};
}
pub(crate) use each_voice;

/// An instrument of `voice`, with `rules`, prepared at 48 kHz.
pub fn hand<V: InstrumentVoice<Float = f32>>(
    voice: V::Config,
    rules: VoiceConfig,
) -> Hand<NoteInstrument<V>> {
    Hand::new(NoteInstrument::new(InstrumentConfig::new(voice, rules)))
}

/// `VoiceConfig::poly` of `voices` voices.
pub fn poly(voices: usize) -> VoiceConfig {
    VoiceConfig::poly(NonZeroUsize::new(voices).unwrap())
}
