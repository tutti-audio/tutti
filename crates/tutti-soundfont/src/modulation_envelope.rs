// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_units::{Normalized, Seconds};

use crate::envelope_stage::EnvelopeStage;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;

/// The modulation envelope (SF2 2.04 section 8.1.2, generators 25 to 30) read once a
/// chunk: a closed form of the time elapsed, in `f64`. Its value is a position in `0..=1`
/// that scales a depth in cents.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct ModulationEnvelope {
    sample_rate: i32,

    attack_slope: f64,
    decay_slope: f64,
    release_slope: f64,

    attack_start_time: Seconds,
    hold_start_time: Seconds,
    decay_start_time: Seconds,

    decay_end_time: Seconds,
    release_end_time: Seconds,

    sustain_level: Normalized,
    release_level: Normalized,

    processed_sample_count: usize,
    stage: EnvelopeStage,
    value: Normalized,
}

impl ModulationEnvelope {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            sample_rate: settings.hz(),
            attack_slope: 0.0,
            decay_slope: 0.0,
            release_slope: 0.0,
            attack_start_time: Seconds::new(0.0),
            hold_start_time: Seconds::new(0.0),
            decay_start_time: Seconds::new(0.0),
            decay_end_time: Seconds::new(0.0),
            release_end_time: Seconds::new(0.0),
            sustain_level: Normalized::clamped(0.0),
            release_level: Normalized::clamped(0.0),
            processed_sample_count: 0,
            stage: EnvelopeStage::Delay,
            value: Normalized::clamped(0.0),
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
        self.attack_slope = 1.0 / attack.get();
        self.decay_slope = 1.0 / decay.get();
        self.release_slope = 1.0 / release.get();

        self.attack_start_time = delay;
        self.hold_start_time = self.attack_start_time + attack;
        self.decay_start_time = self.hold_start_time + hold;

        self.decay_end_time = self.decay_start_time + decay;
        self.release_end_time = release;

        self.sustain_level = sustain;
        self.release_level = Normalized::clamped(0.0);

        self.processed_sample_count = 0;
        self.stage = EnvelopeStage::Delay;
        self.value = Normalized::clamped(0.0);

        self.process(0);
    }

    fn elapsed(&self) -> Seconds {
        Seconds::new(self.processed_sample_count as f64 / f64::from(self.sample_rate))
    }

    pub(crate) fn release(&mut self) {
        self.stage = EnvelopeStage::Release;
        self.release_end_time += self.elapsed();
        self.release_level = self.value;
    }

    pub(crate) fn process(&mut self, sample_count: usize) -> bool {
        self.processed_sample_count += sample_count;

        let current_time = self.elapsed();

        while self.stage <= EnvelopeStage::Hold {
            let end_time = match self.stage {
                EnvelopeStage::Delay => self.attack_start_time,
                EnvelopeStage::Attack => self.hold_start_time,
                EnvelopeStage::Hold => self.decay_start_time,
                _ => unreachable!(),
            };

            if current_time < end_time {
                break;
            }
            self.stage = self.stage.next();
        }

        let audible = |value: Normalized| value.get() > SoundFontMath::NON_AUDIBLE.get();
        match self.stage {
            EnvelopeStage::Delay => {
                self.value = Normalized::clamped(0.0);
                true
            }
            EnvelopeStage::Attack => {
                self.value = Normalized::clamped(
                    self.attack_slope * (current_time - self.attack_start_time).get(),
                );
                true
            }
            EnvelopeStage::Hold => {
                self.value = Normalized::clamped(1.0);
                true
            }
            EnvelopeStage::Decay => {
                let level = self.decay_slope * (self.decay_end_time - current_time).get();
                self.value = Normalized::clamped(level.max(self.sustain_level.get()));
                audible(self.value)
            }
            EnvelopeStage::Release => {
                let level = self.release_level.get()
                    * self.release_slope
                    * (self.release_end_time - current_time).get();
                self.value = Normalized::clamped(level.max(0.0));
                audible(self.value)
            }
        }
    }

    /// The position at the last chunk.
    pub(crate) fn value(&self) -> Normalized {
        self.value
    }
}
