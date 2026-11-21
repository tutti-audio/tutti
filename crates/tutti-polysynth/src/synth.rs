//! [`SynthConfig`] and the value types that configure a synth (oscillator /
//! filter / envelope). The synth itself ([`PolySynth`](crate::PolySynth)) and
//! its per-voice engine live in `crate::polysynth` / `crate::synth_voice`.

use core::fmt;
use std::num::NonZeroUsize;

use crate::{AllocationStrategy, PortamentoConfig, Tuning, UnisonConfig, VoiceMode};
use tutti_dsp::SvfControls;
use tutti_midi_protocol::SysexDeviceId;
use tutti_music::Adsr;
use tutti_ump::MidiGroup;
use tutti_units::{Amplitude, Depth, Hz, Normalized, Q, Resonance, Semitones};

/// The waveform every sub-voice of every voice generates.
///
/// One oscillator per synth, chosen at construction: the voice bank is built
/// for it, so changing it needs a new [`PolySynth`](crate::PolySynth). The
/// pitched waveforms are band-limited (PolyBLEP / PolyBLAMP), so a high note
/// does not fold its harmonics back below Nyquist as inharmonic partials. All
/// variants except [`Noise`](Self::Noise) track the voice's pitch; `Noise`
/// ignores it entirely.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum OscillatorType {
    /// A pure sine — no harmonics for the filter to work on.
    Sine,
    /// Band-limited sawtooth. The default, and the usual subtractive starting
    /// point: every harmonic present, so the filter has material to remove.
    #[default]
    Saw,
    /// Band-limited pulse whose duty cycle is `pulse_width`.
    Square {
        /// Duty cycle as a fraction of the period. `0.5` is a true square
        /// (odd harmonics only); values either side thin the tone toward a
        /// nasal pulse. The bank keeps it within `0.01..=0.99` (a pulse at 0 or 1 is
        /// DC). Fixed for the life of the
        /// synth — it is baked into the voice bank at construction rather than
        /// read per sample.
        pulse_width: Normalized,
    },
    /// Triangle — odd harmonics rolling off steeply, a mellow alternative to
    /// [`Square`](Self::Square).
    Triangle,
    /// Pink noise. Pitch-free: the voice's pitch is still tracked but no
    /// oscillator reads it, so note number only selects which voice sounds.
    /// `Display` renders this as "Pink Noise".
    Noise,
}

tutti_units::ranged! {
    /// A synth's starting master gain: finite and at least zero, a linear
    /// [`Amplitude`]. Above unity boosts.
    MasterVolume(Amplitude), MasterVolumeOutOfRange, "a finite gain of at least zero",
    |raw| raw.get() >= 0.0 && raw.get() <= f64::MAX
}

impl MasterVolume {
    /// Unity gain, the [`SynthConfig`] default.
    pub const UNITY: Self = Self(Amplitude::UNITY);
}

tutti_units::ranged! {
    /// The [`Q`] of a synth's state-variable filter: from [`SvfControls::MIN_Q`]
    /// to [`SvfControls::MAX_Q`], the range its poles are solved and stable in.
    SvfQ(Q), SvfQOutOfRange, "a Q from 0.01 to 1000",
    |raw| raw.get() >= SvfControls::MIN_Q && raw.get() <= SvfControls::MAX_Q
}

impl SvfQ {
    /// The maximally flat response, `1/sqrt(2)`.
    pub const BUTTERWORTH: Self = Self(Q::BUTTERWORTH);
}

/// Which response of the state-variable filter [`FilterType::Svf`] taps.
///
/// All four share one cutoff and one `Q`; only the output tap differs.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum SvfMode {
    /// Passes below cutoff. The default, and the conventional subtractive tap.
    #[default]
    Lowpass,
    /// Passes above cutoff.
    Highpass,
    /// Passes a band around cutoff, whose width narrows as `Q` rises.
    Bandpass,
    /// Rejects a band around cutoff, passing everything else.
    Notch,
}

/// Which filter each voice runs, and its settings.
///
/// Chosen at construction and baked into the voice bank, so the *variant*
/// cannot change on a live synth. The cutoff and resonance within it can: each
/// voice re-derives them from these base values every control step, which is
/// what [`FilterModConfig`], CC74, CC71 and MPE slide move.
///
/// # The two variants carry different resonance types deliberately
///
/// [`Resonance`] and [`Q`] both answer "how resonant", but a ladder at 1.0
/// self-oscillates while a `Q` of 1.0 is a mild bell. Swapping them is silent to
/// the compiler when both are bare floats, and sounds like a mistuned filter.
/// The nodes downstream take `Resonance` and `Q` respectively; these fields
/// carry that distinction all the way in.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum FilterType {
    /// A four-pole Moog-style ladder lowpass, 24 dB/octave.
    Moog {
        /// Corner frequency in [`Hz`]. This is the *base* cutoff — every
        /// modulation source multiplies it rather than replacing it — and the
        /// value each voice returns to on `reset`.
        cutoff: Hz,
        /// Ladder feedback. Musically 0.0 (no emphasis) to ~1.0, where the
        /// ladder self-oscillates; CC71 sweeps this up to a ceiling of 0.95.
        resonance: Resonance,
    },
    /// A state-variable filter, 12 dB/octave, tapped per [`SvfMode`].
    Svf {
        /// Corner (or center, for [`SvfMode::Bandpass`]/[`SvfMode::Notch`])
        /// frequency in [`Hz`]. Base value, modulated multiplicatively as above.
        cutoff: Hz,
        /// Filter [`SvfQ`]. Around 0.707 is maximally flat; higher values put a
        /// resonant peak at cutoff and narrow the band of the band tabs.
        ///
        /// Unlike `cutoff`, this one is fixed for the life of the synth —
        /// the SVF's output mix depends on it and is shared by every voice, so
        /// resonance modulation (CC71) reaches only [`Moog`](Self::Moog).
        q: SvfQ,
        /// Which response is tapped.
        mode: SvfMode,
    },
    /// No filter: the oscillator goes straight to the amplitude envelope. The
    /// default. Voices still compute a cutoff (from a base parked at 20 kHz),
    /// but nothing reads it.
    #[default]
    None,
}

impl fmt::Display for OscillatorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OscillatorType::Sine => write!(f, "Sine"),
            OscillatorType::Saw => write!(f, "Saw"),
            OscillatorType::Square { .. } => write!(f, "Square"),
            OscillatorType::Triangle => write!(f, "Triangle"),
            OscillatorType::Noise => write!(f, "Pink Noise"),
        }
    }
}

impl fmt::Display for SvfMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SvfMode::Lowpass => write!(f, "Lowpass"),
            SvfMode::Highpass => write!(f, "Highpass"),
            SvfMode::Bandpass => write!(f, "Bandpass"),
            SvfMode::Notch => write!(f, "Notch"),
        }
    }
}

impl fmt::Display for FilterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterType::Moog { .. } => write!(f, "Moog Ladder"),
            FilterType::Svf { mode, .. } => write!(f, "SVF ({})", mode),
            FilterType::None => write!(f, "None"),
        }
    }
}

/// Per-voice modulation routed onto the filter cutoff.
///
/// All three sources multiply the [`FilterType`] base cutoff rather than
/// replacing it, and they compound: with every depth at zero the whole block is
/// skipped, which is the default and costs nothing per sample. Each voice runs
/// its own LFO phase, so the sweep is not synchronised across a chord.
///
/// Evaluated once per control step (at most 16 frames apart), on the audio
/// thread; the filter coefficients are ramped per sample between steps.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FilterModConfig {
    /// How far the mod wheel (CC1) opens the filter, as a [`Depth`] in 0.0..1.0.
    /// At 1.0 a fully-open wheel doubles the cutoff frequency; at 0.0 (the
    /// default) the wheel is unrouted.
    pub mod_wheel_depth: Depth,
    /// How far note velocity opens the filter, as a [`Depth`] in 0.0..1.0.
    /// At 1.0 the softest note halves the cutoff and the hardest leaves it at
    /// the base value — velocity only ever darkens. Per-note, latched at
    /// note-on from that voice's velocity.
    pub velocity_depth: Depth,
    /// LFO rate in [`Hz`], above zero. `None` (the default) has no LFO — the
    /// sweep block is skipped, so a nonzero [`lfo_depth`](Self::lfo_depth)
    /// alone does nothing.
    ///
    /// The one [`Hz`] among three [`Depth`]s, sitting between two depths that
    /// read nothing like a frequency — as bare floats any permutation of the
    /// four compiles.
    pub lfo_rate: Option<Hz>,
    /// How far the LFO sweeps the cutoff, as a [`Depth`] in 0.0..1.0. At 1.0
    /// the sine sweeps cutoff ±50% around its base. Needs an
    /// [`lfo_rate`](Self::lfo_rate) to do anything.
    pub lfo_depth: Depth,
}

/// Everything a [`PolySynth`](crate::PolySynth) is built from.
///
/// Fill it with `Default` plus struct update, and hand it to
/// [`PolySynth::new`](crate::PolySynth::new). The rate is not here: the synth
/// renders at the rate its graph prepares it at.
///
/// # Most of this is construction-only
///
/// The synth keeps the config and reads parts of it per block, but the voice
/// bank every voice renders in is built once from
/// [`oscillator`](Self::oscillator), [`filter`](Self::filter) and
/// [`envelope`](Self::envelope). Changing those three needs a new synth. Once
/// the synth is in a graph, its live controls are its `ParamSet`: the master
/// `Volume` and, with a unison engine, `Detune` and `StereoSpread`. Everything
/// else here is read per note or per sample and has no control after the
/// synth is built.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthConfig {
    /// How many notes may sound at once. At least 1 (the type refuses 0); there
    /// is no upper bound. Every voice is fully constructed up front, so this is
    /// a memory and CPU budget rather than just a ceiling: 64 voices of saw
    /// through a ladder filter is real work every block.
    pub polyphony: NonZeroUsize,
    /// Poly, mono or legato. Under mono and legato only slot 0 is ever used,
    /// whatever `polyphony` says.
    pub voice_mode: VoiceMode,
    /// The waveform every voice generates. Construction-only.
    pub oscillator: OscillatorType,
    /// The filter every voice runs. The variant is construction-only; its
    /// cutoff moves at runtime under [`filter_mod`](Self::filter_mod), CC74 and
    /// MPE slide.
    pub filter: FilterType,
    /// The amplitude envelope every voice's gate drives. Construction-only, and
    /// its release sets the synth's reported `tail()`.
    pub envelope: Adsr,
    /// The glide the synth gives a note on its own, whatever the stream says, or
    /// `None` for none. A note also glides when the MIDI stream asks for it:
    /// Portamento On/Off (CC 65) with Portamento Time (CC 5) on its channel, or a
    /// Portamento Control (CC 84), which `tutti-voicing` resolves; that glide
    /// is the stream's and replaces this one for the note it is for. Each voice
    /// glides alone.
    pub portamento: Option<PortamentoConfig>,
    /// Detuned stacked sub-voices per note, or `None` for one oscillator per
    /// voice. `Some` allocates that many sub-voice lanes *per voice*, so
    /// the real oscillator count is `polyphony * voice_count`. As with
    /// portamento, a synth built with `None` has no unison engine to configure
    /// later.
    pub unison: Option<UnisonConfig>,
    /// Which voice gets taken when all `polyphony` voices are busy and another
    /// note arrives.
    pub allocation_strategy: AllocationStrategy,
    /// The scale the synth starts with: stored as tuning program 0 of bank 0
    /// (the MIDI Tuning Standard's table, [`Tuning::table`]), which every channel
    /// plays until a Tuning Program or Bank Select says otherwise. The note
    /// logic keeps every later change to it; defaults to 12-tone equal
    /// temperament with A4 = 440 Hz.
    pub tuning: Tuning,
    /// How far a full-scale channel pitch bend sweeps, in [`Semitones`],
    /// applied symmetrically either side of the note: each channel's default,
    /// which its Pitch Bend Sensitivity RPN (0) sets. Default 2.0, the MIDI
    /// convention of a whole tone. Applied as a frequency ratio,
    /// so it bends by the same interval in every [`tuning`](Self::tuning).
    pub pitch_bend_range: Semitones,
    /// Cutoff modulation sources. All depths default to zero, which skips the
    /// per-sample modulation work entirely.
    pub filter_mod: FilterModConfig,
    /// The Group whose sixteen channels this synth plays; a message in
    /// another Group is not read. A Group is a channel space of its own (M2-104
    /// section 2.1.2, Appendix B.1), so its channels' notes, wheels, pedals and
    /// MPE zones are not another Group's. Default the first.
    pub group: MidiGroup,
    /// The Universal System Exclusive device id this synth answers to besides
    /// the all-call (M1 p.40): a message addressed to it, or to all, is read
    /// (Master Volume, Balance and Tuning, General MIDI System On and Off).
    /// Master Volume and Balance keep what arrives on this id apart from what
    /// arrives on the all-call (M1 p.57). Default [`SysexDeviceId::DEFAULT`].
    pub device: SysexDeviceId,
    /// The master output gain the synth starts at, a linear [`Amplitude`]: `1.0` (the
    /// default) is unity, `0.0` silence, and values above unity boost. Applied once per block to the summed mix, not per voice. Once the synth
    /// is in a graph its gain is its `ParamSet` entry, `Volume`, which a host writes and a
    /// modulation lane takes as its knob.
    pub volume: MasterVolume,
}

impl SynthConfig {
    /// The polyphony of [`Default`]: 8 voices.
    pub const DEFAULT_POLYPHONY: NonZeroUsize = NonZeroUsize::new(8).unwrap();
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            polyphony: Self::DEFAULT_POLYPHONY,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::default(),
            filter: FilterType::default(),
            envelope: Adsr::default(),
            portamento: None,
            unison: None,
            allocation_strategy: AllocationStrategy::Oldest,
            tuning: Tuning::equal_temperament(),
            pitch_bend_range: Semitones::new(2.0),
            filter_mod: FilterModConfig::default(),
            group: MidiGroup::FIRST,
            device: SysexDeviceId::DEFAULT,
            volume: MasterVolume::UNITY,
        }
    }
}

#[cfg(test)]
mod tests;
