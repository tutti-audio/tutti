//! What every voice of an instrument holds: the note, its gate envelope and its strike.

use tutti_buffers::Float;
use tutti_dsp::{Envelope, EnvelopeCurve, EnvelopeDesign, EnvelopeStages, Noise, NoiseColour};
use tutti_ump::{MidiChannel, NoteNumber};
use tutti_units::{
    Amplitude, Db, Hz, Normalized, SampleRate, Samples, Seconds, Semitones, Velocity,
};
use tutti_voicing::{NoteControl, NoteStart};

use crate::tuning::key_freq;

/// The seed of the burst: every strike is the same stream, so a strike scales exactly with its
/// velocity.
const BURST_SEED: u64 = 0x0074_7574_7469;

/// How long a damped note takes to fall 60 dB.
const DAMP: Seconds = Seconds::new(0.02);

/// The frames a voice renders at a time, in arrays on the stack.
const CHUNK: usize = 64;

/// What strikes a voice: the excitation its resonator is driven with, scaled by the note's
/// velocity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strike {
    /// One frame at the velocity: the response of the resonator itself.
    Impulse,
    /// White noise at the velocity for this long, at least one frame: a pluck or a blow.
    Burst(Seconds),
}

/// The strike in progress: what a voice reads a frame at a time.
#[derive(Clone, Debug)]
struct Excitation<F: Float> {
    noise: Noise<F>,
    level: F,
    remaining: usize,
    noisy: bool,
}

impl<F: Float> Excitation<F> {
    fn new() -> Self {
        Self {
            noise: Noise::new(NoiseColour::White, BURST_SEED),
            level: F::ZERO,
            remaining: 0,
            noisy: false,
        }
    }

    /// Begins `strike` at `level`; no strike is silence.
    fn start(&mut self, strike: Option<Strike>, level: Amplitude, rate: SampleRate) {
        self.noise.reset();
        self.level = F::from_unit(level);
        (self.remaining, self.noisy) = match strike {
            None => (0, false),
            Some(Strike::Impulse) => (1, false),
            Some(Strike::Burst(length)) => (length.to_samples_ceil(rate).get().max(1), true),
        };
    }

    #[inline]
    fn next(&mut self) -> F {
        if self.remaining == 0 {
            return F::ZERO;
        }
        self.remaining -= 1;
        if self.noisy {
            self.level * self.noise.next_sample()
        } else {
            self.level
        }
    }
}

/// An envelope whose sustain is the whole level and whose release falls 60 dB over `release`:
/// the gate of a voice that rings until its key goes up.
#[must_use]
pub(super) fn gate(release: Seconds) -> EnvelopeStages {
    let stages = EnvelopeStages::new(Seconds::ZERO, Seconds::ZERO, Normalized::of(1.0), release);
    match EnvelopeCurve::db_linear(Db::new(60.0)) {
        Some(fall) => stages.release_curve(fall),
        None => stages,
    }
}

/// The note a voice sounds and the gain that follows its key.
#[derive(Clone, Debug)]
pub(super) struct Note<F: Float> {
    rate: SampleRate,
    design: EnvelopeDesign,
    envelope: Envelope,
    strike: Option<Strike>,
    excitation: Excitation<F>,
    channel: MidiChannel,
    key: NoteNumber,
    tune: Semitones,
    /// The note number a per-note pitch controller set, which replaces the key's.
    pitch: Option<f64>,
    velocity: Velocity,
    /// The largest sample of the last chunk: what the allocator's quietest steal reads.
    peak: f64,
}

impl<F: Float> Note<F> {
    pub(super) fn new(stages: &EnvelopeStages, strike: Option<Strike>, rate: SampleRate) -> Self {
        let design = EnvelopeDesign::new(stages, rate);
        Self {
            rate,
            design,
            envelope: Envelope::new(design),
            strike,
            excitation: Excitation::new(),
            channel: MidiChannel::FIRST,
            key: NoteNumber::of(0),
            tune: Semitones::UNISON,
            pitch: None,
            velocity: Velocity::SILENT,
            peak: 0.0,
        }
    }

    pub(super) fn start(&mut self, note: &NoteStart) {
        self.channel = note.channel;
        self.key = note.key;
        self.tune = note.tune;
        self.pitch = None;
        self.velocity = note.velocity.velocity();
        self.peak = self.velocity.get();
        self.envelope.start(self.design);
        // A stage of no time is over at once: the envelope is at its attack's end.
        self.envelope.advance(Samples::new(0));
        self.excitation
            .start(self.strike, self.velocity_amplitude(), self.rate);
    }

    pub(super) fn retarget(&mut self, key: NoteNumber, tune: Semitones) {
        self.key = key;
        self.tune = tune;
        self.pitch = None;
    }

    pub(super) fn set_tune(&mut self, tune: Semitones) {
        self.tune = tune;
    }

    pub(super) fn release(&mut self) {
        self.envelope.release();
    }

    pub(super) fn damp(&mut self) {
        self.envelope.release_within(DAMP);
        self.envelope.release();
    }

    pub(super) fn control(&mut self, control: NoteControl) {
        if let NoteControl::Pitch(pitch) = control {
            self.pitch = Some(pitch.note_number());
        }
    }

    pub(super) fn reset_controls(&mut self) {
        self.pitch = None;
    }

    pub(super) fn level(&self) -> Amplitude {
        Amplitude::new(self.peak)
    }

    pub(super) fn channel(&self) -> MidiChannel {
        self.channel
    }

    /// The velocity as the amplitude a strike has: linear.
    pub(super) fn velocity_amplitude(&self) -> Amplitude {
        Amplitude::new(self.velocity.get())
    }

    /// Whether the release has ended.
    pub(super) fn is_finished(&self) -> bool {
        self.envelope.is_idle()
    }

    /// The pitch the note sounds at, with its channel's `bend` on it.
    pub(super) fn frequency(&self, bend: Semitones) -> Hz {
        let base = match self.pitch {
            Some(pitch) => key_freq(pitch, Semitones::UNISON),
            None => key_freq(f64::from(self.key.get()), self.tune),
        };
        base * bend.to_pitch_ratio()
    }

    /// Adds the note's sound to `out`. `sound` turns a chunk of the strike into a chunk of the
    /// resonator's output; the gate's level, ramped across the chunk, scales what it makes.
    /// Nothing allocates.
    pub(super) fn render(&mut self, out: &mut [F], mut sound: impl FnMut(&[F], &mut [F])) {
        let mut strike = [F::ZERO; CHUNK];
        let mut made = [F::ZERO; CHUNK];
        for span in out.chunks_mut(CHUNK) {
            let n = span.len();
            for x in &mut strike[..n] {
                *x = self.excitation.next();
            }
            sound(&strike[..n], &mut made[..n]);
            let from = self.envelope.level().get();
            self.envelope.advance(Samples::new(n));
            let to = self.envelope.level().get();
            let mut peak = 0.0_f64;
            for (i, (y, &m)) in span.iter_mut().zip(&made[..n]).enumerate() {
                let gain = from + (to - from) * ((i + 1) as f64 / n as f64);
                let heard = m * F::from_design(gain);
                let heard_f64: f64 = heard.into();
                peak = peak.max(heard_f64.abs());
                *y += heard;
            }
            self.peak = peak;
        }
    }
}

/// `NoteVoice` for a voice whose `note` field is a [`Note`] and whose `restart` clears its
/// resonator: what every instrument voice shares.
macro_rules! note_voice {
    ($voice:ident) => {
        impl<F: tutti_buffers::Float> tutti_voicing::NoteVoice for $voice<F> {
            type Context = ();

            fn start(&mut self, note: &tutti_voicing::NoteStart, _: &()) {
                self.note.start(note);
                self.restart();
            }

            fn retarget(
                &mut self,
                key: tutti_ump::NoteNumber,
                tune: tutti_units::Semitones,
                _velocity: tutti_ump::NoteVelocity,
                _glide: Option<tutti_voicing::Glide>,
            ) {
                self.note.retarget(key, tune);
            }

            fn tune(&mut self, tune: tutti_units::Semitones) {
                self.note.set_tune(tune);
            }

            fn release(&mut self, _velocity: tutti_ump::NoteVelocity) {
                self.note.release();
            }

            fn damp(&mut self) {
                self.note.damp();
            }

            fn control(&mut self, control: tutti_voicing::NoteControl) {
                self.note.control(control);
            }

            fn reset_controls(&mut self) {
                self.note.reset_controls();
            }

            fn level(&self) -> tutti_units::Amplitude {
                self.note.level()
            }
        }
    };
}
pub(super) use note_voice;
