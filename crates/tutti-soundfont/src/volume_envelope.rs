// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_units::{Amplitude, Seconds};

use crate::envelope_stage::EnvelopeStage;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;

/// The volume envelope (SF2 2.04 section 8.1.2, generators 33 to 38) read once a chunk.
/// Every stage is a closed form of the time elapsed, in `f64`: the fall of a decay or a
/// release is `exp(slope * t)` from where the stage began, so a release of the longest
/// time at the highest rate reaches its level exactly at any width the voice mixes in.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct VolumeEnvelope {
    sample_rate: i32,

    attack_slope: f64,
    decay_slope: f64,
    release_slope: f64,

    attack_start_time: Seconds,
    hold_start_time: Seconds,
    decay_start_time: Seconds,
    release_start_time: Seconds,

    sustain_level: Amplitude,
    release_level: Amplitude,

    processed_sample_count: usize,
    stage: EnvelopeStage,
    value: Amplitude,
}

impl VolumeEnvelope {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            sample_rate: settings.hz(),
            attack_slope: 0.0,
            decay_slope: 0.0,
            release_slope: 0.0,
            attack_start_time: Seconds(0.0),
            hold_start_time: Seconds(0.0),
            decay_start_time: Seconds(0.0),
            release_start_time: Seconds(0.0),
            sustain_level: Amplitude::SILENT,
            release_level: Amplitude::SILENT,
            processed_sample_count: 0,
            stage: EnvelopeStage::Delay,
            value: Amplitude::SILENT,
        }
    }

    pub(crate) fn start(
        &mut self,
        delay: Seconds,
        attack: Seconds,
        hold: Seconds,
        decay: Seconds,
        sustain: Amplitude,
        release: Seconds,
    ) {
        self.attack_slope = 1.0 / attack.get();
        self.decay_slope = -SoundFontMath::HUNDRED_DB_NEPERS / decay.get();
        self.release_slope = -SoundFontMath::HUNDRED_DB_NEPERS / release.get();

        self.attack_start_time = delay;
        self.hold_start_time = self.attack_start_time + attack;
        self.decay_start_time = self.hold_start_time + hold;
        self.release_start_time = Seconds(0.0);

        self.sustain_level = sustain.clamp(Amplitude::SILENT, Amplitude::UNITY);
        self.release_level = Amplitude::SILENT;

        self.processed_sample_count = 0;
        self.stage = EnvelopeStage::Delay;
        self.value = Amplitude::SILENT;

        self.process(0);
    }

    fn elapsed(&self) -> Seconds {
        Seconds(self.processed_sample_count as f64 / f64::from(self.sample_rate))
    }

    pub(crate) fn release(&mut self) {
        self.stage = EnvelopeStage::Release;
        self.release_start_time = self.elapsed();
        self.release_level = self.value;
    }

    /// Releases within `time` at the longest: the release already running,
    /// or the one to come, is made at least as steep as one that falls 100 dB
    /// in that time (the definition of a release time, generator 38).
    /// Called on a voice that is releasing, it continues from the level it
    /// has and never slows the fall.
    pub(crate) fn release_within(&mut self, time: Seconds) {
        self.release_slope = self
            .release_slope
            .min(-SoundFontMath::HUNDRED_DB_NEPERS / time.get());
        if self.stage == EnvelopeStage::Release {
            self.release_start_time = self.elapsed();
            self.release_level = self.value;
        }
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

        match self.stage {
            EnvelopeStage::Delay => {
                self.value = Amplitude::SILENT;
                true
            }
            EnvelopeStage::Attack => {
                self.value =
                    Amplitude(self.attack_slope * (current_time - self.attack_start_time).get());
                true
            }
            EnvelopeStage::Hold => {
                self.value = Amplitude::UNITY;
                true
            }
            EnvelopeStage::Decay => {
                self.value = Amplitude(SoundFontMath::exp_cutoff(
                    self.decay_slope * (current_time - self.decay_start_time).get(),
                ))
                .max(self.sustain_level);
                self.value > SoundFontMath::NON_AUDIBLE
            }
            EnvelopeStage::Release => {
                self.value = self.release_level
                    * SoundFontMath::exp_cutoff(
                        self.release_slope * (current_time - self.release_start_time).get(),
                    );
                self.value > SoundFontMath::NON_AUDIBLE
            }
        }
    }

    /// The level at the last chunk.
    pub(crate) fn value(&self) -> Amplitude {
        self.value
    }
}

/// SF2 2.04 section 8.1.2: the volume envelope's decay (generator 36) and
/// release (generator 38) times are the time to fall 100 dB, at a constant dB
/// per time unit. So half the time on, the level is 50 dB down from where the
/// phase began, whatever that level was.
///
/// The envelope is a closed form of the time elapsed in `f64`, and every time
/// here is a whole number of frames, so the fall is the figure to the rounding
/// of `exp` and `log10`: `1e-12` dB.
#[cfg(test)]
mod tests {
    use tutti_units::{Amplitude, Db, SampleRate, Seconds};

    use super::VolumeEnvelope;
    use crate::render::Render;

    const RATE: f64 = 48_000.0;
    /// The tolerance of a fall in decibels: a few units of `2^-52` on a level
    /// that is a time's `exp`, then a `log10` and a scale of 20.
    const DB: f64 = 1e-12;

    fn envelope(decay: f64, sustain: Amplitude, release: f64) -> VolumeEnvelope {
        let mut envelope = VolumeEnvelope::new(&Render::new(SampleRate::of(RATE)));
        // No delay, a 1 ms attack, no hold.
        envelope.start(
            Seconds(0.0),
            Seconds(0.001),
            Seconds(0.0),
            Seconds(decay),
            sustain,
            Seconds(release),
        );
        envelope
    }

    fn db(level: Amplitude) -> f64 {
        Db::from_amplitude(level).get()
    }

    fn frames(seconds: f64) -> usize {
        (seconds * RATE).round() as usize
    }

    /// Release: from the level at key-off, `fraction` of the release time on is
    /// `fraction` of 100 dB down.
    /// Spec: sf2.synth.env.release-time, sf2.gen.releaseVolEnv
    #[test]
    fn a_release_falls_100_db_in_its_time() {
        // Key-off from full level, and from the sustain at -6 dB: the fall is
        // the same 100 dB a release time, from wherever it starts.
        for sustain_db in [0.0, 6.0] {
            let sustain = Db::new(-sustain_db).to_amplitude();
            let mut envelope = envelope(1.0, sustain, 2.0);
            envelope.process(frames(0.5));
            let before = db(envelope.value());
            envelope.release();
            let mut elapsed = 0;
            for fraction in [0.1, 0.3, 0.5] {
                let to = frames(fraction * 2.0);
                envelope.process(to - elapsed);
                elapsed = to;
                let fell = before - db(envelope.value());
                assert!(
                    (fell - 100.0 * fraction).abs() < DB,
                    "{fraction} of the release is {fell} dB down, not {}",
                    100.0 * fraction
                );
            }
        }
    }

    /// Decay: the same figure, to a sustain level of 0 (a 100 dB sustain is
    /// below the voice's end).
    /// Spec: sf2.synth.env.decay-time, sf2.synth.env.decay-linear, sf2.gen.decayVolEnv
    #[test]
    fn a_decay_falls_100_db_in_its_time() {
        let mut envelope = envelope(2.0, Amplitude::SILENT, 1.0);
        envelope.process(frames(0.001));
        for fraction in [0.1, 0.3, 0.5] {
            let mut again = envelope.clone();
            again.process(frames(fraction * 2.0));
            let fell = -db(again.value());
            assert!(
                (fell - 100.0 * fraction).abs() < DB,
                "{fraction} of the decay is {fell} dB down"
            );
        }
    }

    /// The quick release of an exclusive class falls 100 dB in its time too, so
    /// it is -50 dB after half of it.
    /// Spec: sf2.gen.exclusiveClass
    #[test]
    fn a_quick_release_falls_100_db_in_its_time() {
        let mut envelope = envelope(10.0, Amplitude::UNITY, 5.0);
        envelope.process(frames(0.01));
        envelope.release_within(Seconds(0.01));
        envelope.release();
        envelope.process(frames(0.005));
        assert!((-db(envelope.value()) - 50.0).abs() < DB);
    }
}
