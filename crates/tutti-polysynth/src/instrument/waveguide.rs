//! A string as two travelling waves: [`WaveguideString`] driven by a burst of noise.

use tutti_buffers::Float;
use tutti_dsp::{
    EnvelopeStages, LowestPitch, WaveguideControls, WaveguidePositions, WaveguideString,
};
use tutti_units::{Hz, SampleRate, Seconds};
use tutti_voicing::VoiceAllocator;

use super::InstrumentVoice;
use super::note::{Note, Strike, gate, note_voice};

/// What a [`WaveguideVoice`] is made from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveguideVoiceConfig {
    positions: WaveguidePositions,
    lowest: LowestPitch,
    decay: Seconds,
    strike: Strike,
    envelope: EnvelopeStages,
}

impl WaveguideVoiceConfig {
    /// A string excited a fifth of the way along and read a third of the way along, that goes
    /// down to 20 Hz, rings for 2 s, is struck by a 5 ms burst and lets go in 0.25 s.
    #[must_use]
    pub fn new() -> Self {
        Self {
            positions: WaveguidePositions::DEFAULT,
            lowest: LowestPitch::DEFAULT,
            decay: Seconds::new(2.0),
            strike: Strike::Burst(Seconds::new(0.005)),
            envelope: gate(Seconds::new(0.25)),
        }
    }

    /// Where the string is excited and where it is read.
    #[must_use]
    pub fn positions(mut self, positions: WaveguidePositions) -> Self {
        self.positions = positions;
        self
    }

    /// The lowest pitch the string sounds: the length of its rails.
    #[must_use]
    pub fn lowest(mut self, lowest: LowestPitch) -> Self {
        self.lowest = lowest;
        self
    }

    /// The time a round trip's gain takes to fall 60 dB.
    #[must_use]
    pub fn decay(mut self, decay: Seconds) -> Self {
        self.decay = decay;
        self
    }

    /// What strikes the string.
    #[must_use]
    pub fn strike(mut self, strike: Strike) -> Self {
        self.strike = strike;
        self
    }

    /// The gate a key opens and closes.
    #[must_use]
    pub fn envelope(mut self, envelope: EnvelopeStages) -> Self {
        self.envelope = envelope;
        self
    }
}

impl Default for WaveguideVoiceConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A note on a [`WaveguideString`].
///
/// Its pitch is held between the config's lowest and `fs / 8` (two rails and the ends' delay
/// cannot make a round trip shorter than 8 frames): a key above that sounds the top. A bend
/// retunes the string at once and keeps the waves, so it is not click-free. A glide is not
/// followed: a legato move is a jump.
#[derive(Clone, Debug)]
pub struct WaveguideVoice<F: Float> {
    note: Note<F>,
    string: WaveguideString<F>,
    decay: Seconds,
    frequency: Hz,
}

impl<F: Float> WaveguideVoice<F> {
    fn restart(&mut self) {
        self.string.reset();
        self.frequency = Hz::ZERO;
    }
}

note_voice!(WaveguideVoice);

impl<F: Float> InstrumentVoice for WaveguideVoice<F> {
    type Float = F;
    type Config = WaveguideVoiceConfig;

    fn new(config: &WaveguideVoiceConfig, rate: SampleRate) -> Self {
        Self {
            note: Note::new(&config.envelope, Some(config.strike), rate),
            string: WaveguideString::new(config.positions, config.lowest, rate),
            decay: config.decay,
            frequency: Hz::ZERO,
        }
    }

    fn render(&mut self, allocator: &VoiceAllocator, out: &mut [F]) {
        let frequency = self
            .note
            .frequency(allocator.channel_pitch(self.note.channel()));
        if frequency != self.frequency {
            self.frequency = frequency;
            self.string.retarget(&WaveguideControls {
                frequency,
                decay: self.decay,
            });
        }
        let string = &mut self.string;
        self.note.render(out, |x, y| {
            for (y, &x) in y.iter_mut().zip(x) {
                *y = string.tick(x);
            }
        });
    }

    fn is_finished(&self) -> bool {
        self.note.is_finished()
    }
}
