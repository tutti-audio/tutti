//! A struck body: [`ModalBank`] driven by an impulse.

use tutti_buffers::{ChannelLayout, Float, InputOutput, Planes, PlanesMut};
use tutti_dsp::{EnvelopeStages, ModalBank, ModalControls, Modes};
use tutti_units::{Hz, SampleRate, Seconds};
use tutti_voicing::VoiceAllocator;

use super::InstrumentVoice;
use super::note::{Note, Strike, gate, note_voice};

/// What a [`ModalVoice`] is made from.
#[derive(Clone, Debug, PartialEq)]
pub struct ModalVoiceConfig {
    modes: Modes,
    strike: Strike,
    envelope: EnvelopeStages,
}

impl ModalVoiceConfig {
    /// A body of `modes`, struck by an impulse, that lets go in 0.25 s. The first mode is the
    /// fundamental: a key sets its frequency, and the others keep their ratio to it.
    #[must_use]
    pub fn new(modes: Modes) -> Self {
        Self {
            modes,
            strike: Strike::Impulse,
            envelope: gate(Seconds::new(0.25)),
        }
    }

    /// What strikes the body.
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

/// A note on a [`ModalBank`].
///
/// The key sets the first mode's frequency and the other modes keep their configured ratio to
/// it, so a mode at or above the Nyquist frequency is silent: a high key loses its upper
/// modes, and the fundamental itself when the key is at or above it. The modes' decays are
/// the config's, whatever the key. A bend moves every mode at once. A glide is not followed:
/// a legato move is a jump.
#[derive(Clone, Debug)]
pub struct ModalVoice<F: Float> {
    note: Note<F>,
    bank: ModalBank<F>,
    decay: Seconds,
    frequency: Hz,
}

impl<F: Float> ModalVoice<F> {
    fn restart(&mut self) {
        self.bank.reset();
        self.frequency = Hz::ZERO;
    }
}

note_voice!(ModalVoice);

impl<F: Float> InstrumentVoice for ModalVoice<F> {
    type Float = F;
    type Config = ModalVoiceConfig;

    fn new(config: &ModalVoiceConfig, rate: SampleRate) -> Self {
        Self {
            note: Note::new(&config.envelope, Some(config.strike), rate),
            bank: ModalBank::new(&config.modes, ChannelLayout::MONO, rate),
            decay: ModalControls::of_first(&config.modes).decay,
            frequency: Hz::ZERO,
        }
    }

    fn render(&mut self, allocator: &VoiceAllocator, out: &mut [F]) {
        let frequency = self
            .note
            .frequency(allocator.channel_pitch(self.note.channel()));
        if frequency != self.frequency {
            self.frequency = frequency;
            self.bank.retarget(&ModalControls {
                frequency,
                decay: self.decay,
            });
        }
        let bank = &mut self.bank;
        self.note.render(out, |x, y| {
            // One mono plane each way, as wide as the bank: the block cannot mismatch.
            let (input, mut output) = ([x], [y]);
            let block = Planes::new(&input)
                .and_then(|x| PlanesMut::new(&mut output).map(|y| (x, y)))
                .and_then(|(x, y)| InputOutput::paired(x, y));
            if let Ok(mut block) = block {
                let _ = bank.process(&mut block);
            }
        });
    }

    fn is_finished(&self) -> bool {
        self.note.is_finished()
    }
}
