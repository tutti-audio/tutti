// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_dsp::{Envelope, EnvelopeCurve, EnvelopeDesign, EnvelopeStage, EnvelopeStages};
use tutti_units::{Amplitude, Db, Normalized, SampleRate, Samples, Seconds};

use crate::render::Render;
use crate::soundfont_math::NON_AUDIBLE;

/// The fall of the decay and release times (SF2 2.04 section 8.1.2, generators 36 and 38):
/// the time to fall 100 dB, at a constant dB per time unit.
const FALL: EnvelopeCurve = match EnvelopeCurve::db_linear(Db::new(100.0)) {
    Some(curve) => curve,
    None => panic!("100 dB is a fall"),
};

/// A SoundFont delay, attack, hold, decay, sustain and release envelope (SF2 2.04 section
/// 8.1.2) read once a chunk. Every stage is a closed form of the frames elapsed, so a release
/// of the longest time at the highest rate reaches its level exactly at any width the voice
/// mixes in.
///
/// The volume envelope (generators 33 to 38, [`volume`](Self::volume)) attacks linearly and
/// decays and releases linearly in decibels, 100 dB over their times. The modulation
/// envelope (generators 25 to 30, [`modulation`](Self::modulation)) is straight lines in
/// every stage: its level is a position in `0..=1` that scales a depth in cents.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct SoundFontEnvelope {
    rate: SampleRate,
    /// The fall of the decay and release: the volume envelope's, none for straight lines.
    fall: Option<EnvelopeCurve>,
    envelope: Envelope,
}

impl SoundFontEnvelope {
    /// The volume envelope, idle.
    pub(crate) fn volume(settings: &Render) -> Self {
        Self::idle(settings, Some(FALL))
    }

    /// The modulation envelope, idle.
    pub(crate) fn modulation(settings: &Render) -> Self {
        Self::idle(settings, None)
    }

    fn idle(settings: &Render, fall: Option<EnvelopeCurve>) -> Self {
        let rate = settings.rate();
        let idle = EnvelopeStages::new(
            Seconds::new(0.0),
            Seconds::new(0.0),
            Normalized::of(0.0),
            Seconds::new(0.0),
        );
        Self {
            rate,
            fall,
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
        let mut stages = EnvelopeStages::new(attack, decay, sustain, release)
            .delay(delay)
            .hold(hold);
        if let Some(fall) = self.fall {
            stages = stages.decay_curve(fall).release_curve(fall);
        }
        self.envelope.start(EnvelopeDesign::new(&stages, self.rate));
    }

    pub(crate) fn release(&mut self) {
        self.envelope.release();
    }

    /// Releases within `time` at the longest: the release already running,
    /// or the one to come, is made at least as steep as one that falls 100 dB
    /// in that time (the definition of a release time, generator 38).
    /// Called on a voice that is releasing, it continues from the level it
    /// has and never slows the fall.
    pub(crate) fn release_within(&mut self, time: Seconds) {
        self.envelope.release_within(time);
    }

    /// Moves on by `sample_count` frames.
    pub(crate) fn advance(&mut self, sample_count: usize) {
        self.envelope.advance(Samples::new(sample_count));
    }

    /// Whether the voice is still audible: a decay, sustain or release under
    /// [`NON_AUDIBLE`] ends it.
    pub(crate) fn audible(&self) -> bool {
        match self.envelope.stage() {
            EnvelopeStage::Delay | EnvelopeStage::Attack | EnvelopeStage::Hold => true,
            EnvelopeStage::Decay
            | EnvelopeStage::Sustain
            | EnvelopeStage::Release
            | EnvelopeStage::Idle => self.envelope.level() > NON_AUDIBLE,
        }
    }

    /// The level at the last chunk.
    pub(crate) fn level(&self) -> Amplitude {
        self.envelope.level()
    }

    /// The level at the last chunk as a position in `0..=1`.
    pub(crate) fn position(&self) -> Normalized {
        Normalized::clamped(self.envelope.level().get())
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
    use tutti_units::{Amplitude, Db, Normalized, SampleRate, Seconds};

    use super::SoundFontEnvelope;
    use crate::render::Render;

    const RATE: f64 = 48_000.0;
    /// The tolerance of a fall in decibels: a few units of `2^-52` on a level
    /// that is a time's `exp`, then a `log10` and a scale of 20.
    const DB: f64 = 1e-12;

    fn envelope(decay: f64, sustain: Normalized, release: f64) -> SoundFontEnvelope {
        let mut envelope = SoundFontEnvelope::volume(&Render::new(SampleRate::of(RATE)));
        // No delay, a 1 ms attack, no hold.
        envelope.start(
            Seconds::new(0.0),
            Seconds::new(0.001),
            Seconds::new(0.0),
            Seconds::new(decay),
            sustain,
            Seconds::new(release),
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
            let sustain = Normalized::clamped(Db::new(-sustain_db).to_amplitude().get());
            let mut envelope = envelope(1.0, sustain, 2.0);
            envelope.advance(frames(0.5));
            let before = db(envelope.level());
            envelope.release();
            let mut elapsed = 0;
            for fraction in [0.1, 0.3, 0.5] {
                let to = frames(fraction * 2.0);
                envelope.advance(to - elapsed);
                elapsed = to;
                let fell = before - db(envelope.level());
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
        let mut envelope = envelope(2.0, Normalized::of(0.0), 1.0);
        envelope.advance(frames(0.001));
        for fraction in [0.1, 0.3, 0.5] {
            let mut again = envelope.clone();
            again.advance(frames(fraction * 2.0));
            let fell = -db(again.level());
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
        let mut envelope = envelope(10.0, Normalized::of(1.0), 5.0);
        envelope.advance(frames(0.01));
        envelope.release_within(Seconds::new(0.01));
        envelope.release();
        envelope.advance(frames(0.005));
        assert!((-db(envelope.level()) - 50.0).abs() < DB);
    }
}
