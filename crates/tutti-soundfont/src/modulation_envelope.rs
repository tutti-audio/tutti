// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_dsp::{Envelope, EnvelopeDesign, EnvelopeStages};
use tutti_units::{Amplitude, Normalized, SampleRate, Samples, Seconds};

use crate::render::Render;

/// The modulation envelope (SF2 2.04 section 8.1.2, generators 25 to 30) read once a
/// chunk: every stage a straight line, a closed form of the frames elapsed. Its value is a
/// position in `0..=1` that scales a depth in cents.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct ModulationEnvelope {
    rate: SampleRate,
    envelope: Envelope,
}

impl ModulationEnvelope {
    pub(crate) fn new(settings: &Render) -> Self {
        let rate = settings.rate();
        let idle = EnvelopeStages::new(
            Seconds::new(0.0),
            Seconds::new(0.0),
            Amplitude::SILENT,
            Seconds::new(0.0),
        );
        Self {
            rate,
            envelope: Envelope::new(EnvelopeDesign::new(&idle, rate)),
        }
    }

    pub(crate) fn start(
        &mut self,
        delay: Seconds,
        attack: Seconds,
        hold: Seconds,
        decay: Seconds,
        sustain: Normalized,
        release: Seconds,
    ) {
        let stages = EnvelopeStages::new(attack, decay, Amplitude::new(sustain.get()), release)
            .delay(delay)
            .hold(hold);
        self.envelope.start(EnvelopeDesign::new(&stages, self.rate));
    }

    pub(crate) fn release(&mut self) {
        self.envelope.release();
    }

    pub(crate) fn process(&mut self, sample_count: usize) {
        self.envelope.advance(Samples::new(sample_count));
    }

    /// The position at the last chunk.
    pub(crate) fn value(&self) -> Normalized {
        Normalized::clamped(self.envelope.level().get())
    }
}
