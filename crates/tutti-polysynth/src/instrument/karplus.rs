//! A plucked string: [`KarplusStrong`] driven by a burst of noise.

use tutti_buffers::Float;
use tutti_dsp::{EnvelopeStages, KarplusStrong, KarplusStrongControls, LowestPitch};
use tutti_ump::MidiChannel;
use tutti_units::{Hz, SampleRate, Seconds, Semitones};

use super::InstrumentVoice;
use super::note::{Note, Strike, gate, note_voice};

/// What a [`KarplusVoice`] is made from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KarplusVoiceConfig {
    lowest: LowestPitch,
    decay: Seconds,
    strike: Strike,
    envelope: EnvelopeStages,
}

impl KarplusVoiceConfig {
    /// A string that goes down to 20 Hz, rings for 2 s, is plucked by a 5 ms burst and lets go
    /// in 0.25 s.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lowest: LowestPitch::DEFAULT,
            decay: Seconds::new(2.0),
            strike: Strike::Burst(Seconds::new(0.005)),
            envelope: gate(Seconds::new(0.25)),
        }
    }

    /// The lowest pitch the string sounds: the length of its delay line.
    #[must_use]
    pub fn lowest(mut self, lowest: LowestPitch) -> Self {
        self.lowest = lowest;
        self
    }

    /// The time the loop takes to fall 60 dB.
    #[must_use]
    pub fn decay(mut self, decay: Seconds) -> Self {
        self.decay = decay;
        self
    }

    /// What plucks the string.
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

impl Default for KarplusVoiceConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A note on a [`KarplusStrong`] string.
///
/// Its pitch is held between the config's lowest and `fs / 6` (the delay line, an average and
/// the all-pass of the tuning cannot make a loop shorter than 6 frames): a key above that sounds
/// the top. A bend retunes the loop at once and keeps what rings, so it is not click-free. A
/// glide is not followed: a legato move is a jump.
#[derive(Clone, Debug)]
pub struct KarplusVoice<F: Float> {
    note: Note<F>,
    string: KarplusStrong<F>,
    decay: Seconds,
    frequency: Hz,
}

impl<F: Float> KarplusVoice<F> {
    fn restart(&mut self) {
        self.string.reset();
        self.frequency = Hz::ZERO;
    }
}

note_voice!(KarplusVoice);

impl<F: Float> InstrumentVoice for KarplusVoice<F> {
    type Float = F;
    type Config = KarplusVoiceConfig;

    fn new(config: &KarplusVoiceConfig, rate: SampleRate) -> Self {
        Self {
            note: Note::new(&config.envelope, Some(config.strike), rate),
            string: KarplusStrong::new(config.lowest, rate),
            decay: config.decay,
            frequency: Hz::ZERO,
        }
    }

    fn channel(&self) -> MidiChannel {
        self.note.channel()
    }

    fn render(&mut self, bend: Semitones, out: &mut [F]) {
        let frequency = self.note.frequency(bend);
        if frequency != self.frequency {
            self.frequency = frequency;
            self.string.retarget(&KarplusStrongControls {
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
