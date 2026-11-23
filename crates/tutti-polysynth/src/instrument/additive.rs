//! A sum of sine partials: [`AdditiveBank`] under an [`Envelope`](tutti_dsp::Envelope).

use tutti_buffers::{Float, MonoMut};
use tutti_dsp::{AdditiveBank, AdditiveControls, EnvelopeStages, Partials};
use tutti_ump::MidiChannel;
use tutti_units::{Hz, Normalized, SampleRate, Seconds, Semitones};

use super::InstrumentVoice;
use super::note::{Note, note_voice};

/// What an [`AdditiveVoice`] is made from.
#[derive(Clone, Debug, PartialEq)]
pub struct AdditiveVoiceConfig {
    partials: Partials,
    envelope: EnvelopeStages,
}

impl AdditiveVoiceConfig {
    /// `partials` under a 5 ms attack, a 0.1 s decay to 0.7 and a release of 0.25 s. A partial
    /// is a ratio of the key's frequency at an amplitude.
    #[must_use]
    pub fn new(partials: Partials) -> Self {
        Self {
            partials,
            envelope: EnvelopeStages::new(
                Seconds::new(0.005),
                Seconds::new(0.1),
                Normalized::of(0.7),
                Seconds::new(0.25),
            ),
        }
    }

    /// The envelope the partials' sum follows.
    #[must_use]
    pub fn envelope(mut self, envelope: EnvelopeStages) -> Self {
        self.envelope = envelope;
        self
    }
}

/// A note on an [`AdditiveBank`].
///
/// Each partial is a sine at its ratio of the key's frequency; one at or above the Nyquist
/// frequency is dropped, so no partial folds back and a high key sounds fewer of them. The
/// velocity is the bank's level, and the envelope shapes the whole sum. A glide is not
/// followed: a legato move is a jump.
#[derive(Clone, Debug)]
pub struct AdditiveVoice<F: Float> {
    note: Note<F>,
    bank: AdditiveBank<F>,
    frequency: Hz,
}

impl<F: Float> AdditiveVoice<F> {
    fn restart(&mut self) {
        self.bank.reset();
        self.frequency = Hz::ZERO;
    }
}

note_voice!(AdditiveVoice);

impl<F: Float> InstrumentVoice for AdditiveVoice<F> {
    type Float = F;
    type Config = AdditiveVoiceConfig;

    fn new(config: &AdditiveVoiceConfig, rate: SampleRate) -> Self {
        Self {
            note: Note::new(&config.envelope, None, rate),
            bank: AdditiveBank::new(config.partials.clone(), rate),
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
            self.bank.retarget(&AdditiveControls {
                frequency,
                level: self.note.velocity_amplitude(),
            });
        }
        let bank = &mut self.bank;
        self.note.render(out, |_, y| bank.process(MonoMut::new(y)));
    }

    fn is_finished(&self) -> bool {
        self.note.is_finished()
    }
}
