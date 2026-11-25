//! Unison: the detune ratios, pans, phases and gains for the stack of
//! sub-voices that sound one note.
//!
//! Pure derivation — this module computes per-sub-voice parameters and never
//! touches a sample. `crate::synth_voice` reads them when it renders. Detune
//! and spread are recomputed only when they change, so the audio thread does an
//! array index rather than the trigonometry.

use tutti_buffers::Float;
use tutti_rt::{Param, SplitMix64};
use tutti_units::{Amplitude, CentsSpan, Pan, Phase, Spread};

pub(crate) const MAX_UNISON_VOICES: usize = UnisonVoices::MAX as usize;

/// How many detuned sub-voices stack on one note: 1 to [`MAX`](Self::MAX).
///
/// `1` is plain unison-off. `new` refuses and never clamps; `of` is for a
/// literal or a constant. Even counts have no centre voice, so the written
/// pitch is not itself sounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnisonVoices(u8);

impl UnisonVoices {
    /// The most sub-voices per note: the stack is sized for it, so nothing
    /// allocates on the audio thread.
    pub const MAX: u8 = 16;
    /// One sub-voice: no stack.
    pub const ONE: Self = Self(1);

    /// `count` sub-voices, or `None` for none or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn new(count: u8) -> Option<Self> {
        if count == 0 || count > Self::MAX {
            None
        } else {
            Some(Self(count))
        }
    }

    /// `count` sub-voices, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `count` is 0 or more than [`MAX`](Self::MAX).
    #[must_use]
    pub const fn of(count: u8) -> Self {
        match Self::new(count) {
            Some(voices) => voices,
            None => panic!("a unison stack is 1 to 16 sub-voices"),
        }
    }

    /// The number of sub-voices.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for UnisonVoices {
    fn default() -> Self {
        Self::ONE
    }
}

/// The refused count of [`UnisonVoices`]'s `TryFrom<u8>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a unison stack is 1 to {} sub-voices, but was {}", UnisonVoices::MAX, .0)]
pub struct UnisonVoicesError(pub u8);

impl TryFrom<u8> for UnisonVoices {
    type Error = UnisonVoicesError;

    fn try_from(count: u8) -> Result<Self, UnisonVoicesError> {
        Self::new(count).ok_or(UnisonVoicesError(count))
    }
}

impl From<UnisonVoices> for u8 {
    fn from(voices: UnisonVoices) -> u8 {
        voices.0
    }
}

/// Stacked, detuned copies of a single note — the "supersaw" thickener.
///
/// Every field here multiplies the cost of *each* voice: a synth with
/// `polyphony: 8` and `voice_count: 7` runs 56 oscillator lanes. The
/// sub-voices are summed to a constant-power total, so raising
/// [`voice_count`](Self::voice_count) thickens the tone without raising the
/// level.
#[derive(Debug, Clone, PartialEq)]
pub struct UnisonConfig {
    pub(crate) voice_count: UnisonVoices,
    pub(crate) detune_cents: CentsSpan,
    pub(crate) stereo_spread: Spread,
    pub(crate) phase_randomize: bool,
}

impl UnisonConfig {
    /// A stack of one: [`UnisonVoices::ONE`], no detune, every sub-voice at centre and
    /// in phase.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            voice_count: UnisonVoices::ONE,
            detune_cents: CentsSpan::ZERO,
            stereo_spread: Spread::POINT,
            phase_randomize: false,
        }
    }

    /// Sub-voices per note, 1..=16. `1` is plain unison-off and the default.
    /// Even counts have no centre voice, so the written pitch is not itself
    /// sounded.
    #[must_use]
    pub const fn voice_count(mut self, count: UnisonVoices) -> Self {
        self.voice_count = count;
        self
    }

    /// Detune amount as a [`CentsSpan`], applied symmetrically: sub-voices are spaced
    /// evenly over +-this value, so the outermost pair sits `2 * detune_cents`
    /// apart and the centre (odd counts only) stays at the written pitch.
    /// A few cents is a chorus shimmer; tens of cents is an overtly out-of-tune stack.
    #[must_use]
    pub const fn detune_cents(mut self, detune: CentsSpan) -> Self {
        self.detune_cents = detune;
        self
    }

    /// How wide the sub-voices are panned, as a [`Spread`] in 0.0..1.0.
    /// `0.0` stacks them all at centre, `1.0` puts the outermost pair hard
    /// left and hard right, with the rest spaced evenly between. Pans track
    /// detune position, so the lowest sub-voice is always the leftmost.
    #[must_use]
    pub const fn stereo_spread(mut self, spread: Spread) -> Self {
        self.stereo_spread = spread;
        self
    }

    /// Whether each note-on starts its sub-voices at random phases.
    ///
    /// `false` (the default) starts them all in phase, so every note begins
    /// with a brief transient spike as the copies reinforce before the detune
    /// pulls them apart. `true` trades that consistent attack for a softer,
    /// note-to-note-varying one. The generator is a seeded [`SplitMix64`], so a
    /// render is reproducible.
    #[must_use]
    pub const fn phase_randomize(mut self, randomize: bool) -> Self {
        self.phase_randomize = randomize;
        self
    }
}

impl Default for UnisonConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One unison sub-voice's derived parameters, recomputed by every unison
/// setter.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct UnisonVoiceParams {
    /// Pitch ratio against the note's pitch; 1.0 is the center pitch. A bare
    /// ratio: it is the output of `Semitones::to_pitch_ratio`, and a
    /// multiplier is not one of the units.
    pub freq_ratio: f64,
    /// Stereo position of this sub-voice.
    pub pan: Pan,
    /// Start phase from the last random draw; `Phase::START` when
    /// `phase_randomize` is off.
    pub phase_offset: Phase,
    /// Gain of this sub-voice, `1 / sqrt(count)`, so the stack's power does
    /// not grow with the voice count.
    pub amplitude: Amplitude,
}

/// The seed of the phase scatter, so a render is reproducible.
const PHASE_SEED: u64 = 12345;

#[derive(Debug, Clone)]
pub(crate) struct UnisonEngine {
    config: UnisonConfig,
    voices: [UnisonVoiceParams; MAX_UNISON_VOICES],
    rng: SplitMix64,
    /// Control-rate-modulatable mirrors of `config.detune_cents` /
    /// `config.stereo_spread`. A modulator writes these (through the
    /// synth's `ParamSet`); [`sync_from_atomics`](Self::sync_from_atomics),
    /// called once per block, folds any change back into `config` + a
    /// recompute. Detune/spread only affect per-voice params on recompute (not
    /// per-sample), so a block-rate sync is exact.
    detune: Param<CentsSpan>,
    spread: Param<Spread>,
}

impl UnisonEngine {
    pub(crate) fn new(config: UnisonConfig) -> Self {
        let detune = Param::new(config.detune_cents);
        let spread = Param::new(config.stereo_spread);
        let mut engine = Self {
            config,
            voices: [UnisonVoiceParams::default(); MAX_UNISON_VOICES],
            rng: SplitMix64::new(PHASE_SEED),
            detune,
            spread,
        };
        engine.recompute_params();
        engine
    }

    /// Stops sharing the detune and spread cells with the live engine and its
    /// mod targets, keeping their current values (see `Param::detach`): the
    /// half of `PolySynth`'s `fork_fresh` that makes a fork render the unison
    /// it was taken with.
    pub(crate) fn detach(&mut self) {
        self.detune.detach();
        self.spread.detach();
    }

    /// The detune cell (cents), for the synth's declared params.
    pub(crate) fn detune(&self) -> &Param<CentsSpan> {
        &self.detune
    }

    /// The stereo-spread cell (0..1), for the synth's declared params.
    pub(crate) fn spread(&self) -> &Param<Spread> {
        &self.spread
    }

    /// Folds any change to the detune/spread atomics back into
    /// `config` and recompute per-voice params. Called once per block. Cheap
    /// when nothing moved (compares against the current config first).
    ///
    /// `driven` is the block's detune and spread from the synth's param
    /// ports where a lane drives them; `None` reads the atomic.
    pub(crate) fn sync_from_atomics(&mut self, [detune, spread]: [Option<f64>; 2]) {
        // A lane's value is any float; the span saturates it (a negative
        // detune is no detune), so the comparison against the stored config
        // never recomputes a block for an out-of-range lane.
        let detune = detune.map_or_else(|| self.detune.load(), CentsSpan::clamped);
        let spread = Spread::new_clamped(spread.unwrap_or_else(|| self.spread.load().get()));
        let changed = detune != self.config.detune_cents
            || (spread.get() - self.config.stereo_spread.get()).abs() > f64::EPSILON;
        if changed {
            self.config.detune_cents = detune;
            self.config.stereo_spread = spread;
            self.recompute_params();
        }
    }

    pub(crate) fn recompute_params(&mut self) {
        let count = usize::from(self.config.voice_count.get());

        let amplitude = Amplitude::new(1.0 / (count as f64).sqrt());
        // A converter, not a divide: the span's unit changes from cents to
        // semitones here.
        let detune_semitones = self.config.detune_cents.to_semitones();

        for i in 0..count {
            let position = if count == 1 {
                0.0
            } else {
                (i as f64 / (count - 1) as f64) * 2.0 - 1.0
            };

            // `Semitones * f64` is opted in, so spreading the detune across the
            // voice's position stays in the unit, and the exponent conversion
            // happens once at the end.
            let freq_ratio = (detune_semitones * position).to_pitch_ratio();
            let pan = Pan::new(position * self.config.stereo_spread.get());

            // The phase offset is kept: it is the last `randomize_phases`
            // draw, not a function of detune or spread, and a detune moving
            // under modulation must not wipe it.
            self.voices[i] = UnisonVoiceParams {
                freq_ratio,
                pan,
                phase_offset: self.voices[i].phase_offset,
                amplitude,
            };
        }

        for voice in &mut self.voices[count..MAX_UNISON_VOICES] {
            *voice = UnisonVoiceParams::default();
        }
    }

    /// Draws a fresh set of start phases, one per sub-voice, into the
    /// params table, and return them.
    ///
    /// Every call is a new draw, so each voice that starts calls this for its
    /// own stack: two notes of a chord struck together get different phases.
    /// Without `phase_randomize` every phase is [`Phase::START`].
    pub(crate) fn randomize_phases(&mut self) -> &[UnisonVoiceParams] {
        if !self.config.phase_randomize {
            for v in &mut self.voices {
                v.phase_offset = Phase::START;
            }
            return self.all_params();
        }

        let count = usize::from(self.config.voice_count.get());

        for i in 0..count {
            self.voices[i].phase_offset = Phase::new(f64::from_fraction_bits(self.rng.next_u64()));
        }
        self.all_params()
    }

    #[inline]
    pub(crate) fn voice_count(&self) -> UnisonVoices {
        self.config.voice_count
    }

    #[inline]
    pub(crate) fn voice_params(&self, index: usize) -> &UnisonVoiceParams {
        &self.voices[index.min(MAX_UNISON_VOICES - 1)]
    }

    #[inline]
    pub(crate) fn all_params(&self) -> &[UnisonVoiceParams] {
        &self.voices[..usize::from(self.config.voice_count.get())]
    }

    pub(crate) fn config(&self) -> &UnisonConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests;
