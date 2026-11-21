// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_dsp::{Lfo as Wave, LfoWave};
use tutti_units::{Bipolar, Hz, SampleRate, Samples, Seconds};

use crate::render::Render;

/// A triangle LFO (SF2 2.04 section 8.1.2, generators 21 to 24) read once a chunk: its
/// value is a closed form of the frames elapsed, so it does not drift however long the voice
/// sounds. A frequency of a millihertz or less is no LFO: its value stays zero.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Lfo {
    rate: SampleRate,
    block_size: Samples,
    wave: Option<Wave>,
}

impl Lfo {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            rate: settings.rate(),
            block_size: Samples::new(settings.block_len()),
            wave: None,
        }
    }

    pub(crate) fn start(&mut self, delay: Seconds, frequency: Hz) {
        self.wave = (frequency > Hz::new(1.0e-3))
            .then(|| Wave::new(LfoWave::Triangle, frequency, self.rate).delay(delay));
    }

    pub(crate) fn process(&mut self) {
        if let Some(wave) = &mut self.wave {
            wave.advance(self.block_size);
        }
    }

    /// The wave's value in `-1..=1` at the last chunk.
    pub(crate) fn value(&self) -> Bipolar {
        self.wave.as_ref().map_or(Bipolar::ZERO, Wave::value)
    }
}
