// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_units::{Bipolar, Hz, Seconds};

use crate::render::Render;

/// A triangle LFO (SF2 2.04 section 8.1.2, generators 21 to 24) read once a chunk: its
/// value is a closed form of the time elapsed, in `f64`, so it does not drift however
/// long the voice sounds.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Lfo {
    sample_rate: i32,
    block_size: usize,

    active: bool,

    delay: Seconds,
    period: Seconds,

    processed_sample_count: usize,
    value: Bipolar,
}

impl Lfo {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            sample_rate: settings.hz(),
            block_size: settings.block_len(),
            active: false,
            delay: Seconds(0.0),
            period: Seconds(0.0),
            processed_sample_count: 0,
            value: Bipolar::ZERO,
        }
    }

    pub(crate) fn start(&mut self, delay: Seconds, frequency: Hz) {
        if frequency > Hz(1.0e-3) {
            self.active = true;

            self.delay = delay;
            self.period = Seconds(1.0 / frequency.get());

            self.processed_sample_count = 0;
            self.value = Bipolar::ZERO;
        } else {
            self.active = false;
            self.value = Bipolar::ZERO;
        }
    }

    pub(crate) fn process(&mut self) {
        if !self.active {
            return;
        }

        self.processed_sample_count += self.block_size;

        let current_time =
            Seconds(self.processed_sample_count as f64 / f64::from(self.sample_rate));

        if current_time < self.delay {
            self.value = Bipolar::ZERO;
        } else {
            let phase = ((current_time - self.delay).get() % self.period.get()) / self.period.get();
            if phase < 0.25 {
                self.value = Bipolar::clamped(4.0 * phase);
            } else if phase < 0.75 {
                self.value = Bipolar::clamped(4.0 * (0.5 - phase));
            } else {
                self.value = Bipolar::clamped(4.0 * (phase - 1.0));
            }
        }
    }

    /// The wave's value in `-1..=1` at the last chunk.
    pub(crate) fn value(&self) -> Bipolar {
        self.value
    }
}
