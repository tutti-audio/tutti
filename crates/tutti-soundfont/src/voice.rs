// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_dsp::{
    PanLaw, SOLVE_CEILING, SvfCoefficients, SvfControls, SvfModel, SvfType, pan_gains, svf_step,
};
use tutti_sf2::{SamplePool, Side};
use tutti_ump::ControlValue;
use tutti_ump::{MidiChannel, NoteNumber, RegisteredPerNoteController};
use tutti_units::{Db, Hz, Pan, Q, SampleRate, Semitones};
use tutti_voicing::{ChannelState, Glide, NoteControl};

/// The longest an exclusive class's quick release takes, in seconds, to fall
/// 100 dB as every release time does (section 8.1.2, generator 38): the floor
/// every release is given at its start, to avoid a pop. The spec says "rapidly"
/// and no more (SF2 2.04 section 8.1.2, generator 57); this is tutti's figure,
/// the shortest release a voice can have.
pub(crate) const QUICK_RELEASE: f32 = 0.01;

use crate::channel::Channel;
use crate::lfo::Lfo;
use crate::modulation_envelope::ModulationEnvelope;
use crate::oscillator::Oscillator;
use crate::region_ex::RegionEx;
use crate::region_pair::RegionPair;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;
use crate::volume_envelope::VolumeEnvelope;

/// The voice's low-pass for a cutoff in hertz and a resonance as a linear peak
/// height above the DC gain (SF2 2.04 section 8.1.3, generators 8 and 9: the
/// cutoff is the filter's corner and the Q is the height of the resonant peak
/// in decibels, here already converted). `None` from [`SOLVE_CEILING`] of the
/// rate up: the filter would do nothing there.
///
/// The filter is the trapezoidal state-variable low-pass, `1 / (s^2 + s / q + 1)`
/// under the prewarped bilinear map, designed in `f64` and narrowed once to the
/// voice's `f32`. Its DC gain is 1 for every `q`. The prototype's peak height is
/// `P = q / sqrt(1 - 1 / (4 q^2))` for `q > 1 / sqrt(2)` (the maximum of
/// `|H|^2 = 1 / ((1 - x^2)^2 + x^2 / q^2)` over `x`, at `x^2 = 1 - 1 / (2 q^2)`),
/// and the bilinear map moves the peak along the frequency axis and not its
/// height. Solving `q^4 - P^2 q^2 + P^2 / 4 = 0` for the peak `P` the spec asks
/// for gives `q^2 = (P^2 + P sqrt(P^2 - 1)) / 2`, so the peak is the resonance
/// up to [`SvfControls::MAX_Q`] (a resonance of about 60 dB; the model clamps
/// `q` there and a larger resonance peaks at that cap), and a resonance of 1
/// (0 dB) is `q = 1 / sqrt(2)`, Butterworth.
fn low_pass(cutoff: f32, resonance: f32, rate: SampleRate) -> Option<SvfCoefficients<f32>> {
    low_pass_model(cutoff, resonance, rate).map(|model| SvfCoefficients::from(&model))
}

/// The model [`low_pass`] narrows: the `f64` design of the voice's low-pass.
fn low_pass_model(cutoff: f32, resonance: f32, rate: SampleRate) -> Option<SvfModel> {
    // The voice's cutoff is an `f32` state; the design takes it as `Hz`.
    let cutoff = Hz(f64::from(cutoff));
    if cutoff.get() >= SOLVE_CEILING * rate.get() {
        return None;
    }
    // A resonance below 0 dB asks for no peak.
    let peak = f64::from(resonance).max(1.0);
    let q = libm::sqrt(0.5 * (peak * peak + peak * libm::sqrt(peak * peak - 1.0)));
    let controls = SvfControls {
        ty: SvfType::LowPass,
        freq: cutoff,
        // `Q` stores an `f32`: the design's `f64` ratio crosses to it here.
        q: Q(q as f32),
        gain: Db::UNITY,
    };
    Some(SvfModel::new(controls, rate))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VoiceState {
    Playing,
    Released,
}

/// What the note's per-note controllers have set.
#[derive(Debug, Clone, Copy)]
struct NoteControls {
    /// The sounding pitch in semitones (the told pitch with the per-note bend
    /// in it, `tutti-voicing`'s), if one was set; else the key's.
    pitch: Option<f32>,
    /// Per-note volume, `0..=1`.
    volume: f32,
    /// Per-note pan in the channel's `-50..=50`, if one was set.
    pan: Option<f32>,
}

impl NoteControls {
    const DEFAULT: Self = Self {
        pitch: None,
        volume: 1_f32,
        pan: None,
    };
}

/// A glide in progress: the pitch is `offset` semitones above the note's own
/// and falls to it in a straight line over `frames` frames, a constant rate in
/// cents (General MIDI 2 section 3.3.3 gives the portamento rate in cents a
/// millisecond).
#[derive(Debug, Clone, Copy)]
struct GlideRun {
    offset: f32,
    frames: f32,
    done: f32,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Voice {
    vol_env: VolumeEnvelope,
    mod_env: ModulationEnvelope,

    vib_lfo: Lfo,
    mod_lfo: Lfo,

    oscillator: Oscillator,
    rate: SampleRate,
    /// The low-pass, or `None` while the cutoff is too near Nyquist to filter.
    low_pass: Option<SvfCoefficients<f32>>,
    /// The low-pass's integrator state `[ic1eq, ic2eq]`.
    history: [f32; 2],

    block: Vec<f32>,

    // A sudden change in the mix gain will cause pop noise.
    // To avoid this, we save the mix gain of the previous block,
    // and smooth out the gain if the gap between the current and previous gain is too large.
    // The actual smoothing process is done in the WriteBlock method of the Synthesizer class.
    pub(crate) previous_mix_gain_left: f32,
    pub(crate) previous_mix_gain_right: f32,
    pub(crate) current_mix_gain_left: f32,
    pub(crate) current_mix_gain_right: f32,

    pub(crate) previous_reverb_send: f32,
    pub(crate) previous_chorus_send: f32,
    pub(crate) current_reverb_send: f32,
    pub(crate) current_chorus_send: f32,

    channel: i32,
    key: i32,
    velocity: i32,
    /// The key the note is played on now: where it started, or where a legato
    /// move or a Portamento Control last took it (the zone's forced key, which
    /// `key` follows, is not it).
    played: i32,
    /// Semitones the voice has been moved from the key it started on.
    transpose: f32,
    glide: Option<GlideRun>,

    // The note's per-note controllers (M2-104 section 7.4); the defaults of
    // `NoteControls::DEFAULT` until a message sets one.
    note: NoteControls,
    /// How far the key sounds from equal temperament, in semitones: the note
    /// logic's tuning of it (`Command::Start`'s `tune`).
    tune: f32,

    note_gain: f32,

    cutoff: f32,
    resonance: f32,

    vib_lfo_to_pitch: f32,
    mod_lfo_to_pitch: f32,
    mod_env_to_pitch: f32,

    mod_lfo_to_cutoff: i32,
    mod_env_to_cutoff: i32,
    dynamic_cutoff: bool,

    mod_lfo_to_volume: f32,
    dynamic_volume: bool,

    instrument_pan: f32,
    instrument_reverb: f32,
    instrument_chorus: f32,

    // Some instruments require fast cutoff change, which can cause pop noise.
    // This is used to smooth out the cutoff frequency.
    smoothed_cutoff: f32,

    voice_state: VoiceState,
    /// Time elapsed in samples, so a voice's first block takes its gains whole.
    voice_length: usize,
}

impl Voice {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            vol_env: VolumeEnvelope::new(settings),
            mod_env: ModulationEnvelope::new(settings),
            vib_lfo: Lfo::new(settings),
            mod_lfo: Lfo::new(settings),
            oscillator: Oscillator::new(settings),
            rate: settings.rate(),
            low_pass: None,
            history: [0_f32; 2],
            block: vec![0_f32; settings.block_len()],
            previous_mix_gain_left: 0_f32,
            previous_mix_gain_right: 0_f32,
            current_mix_gain_left: 0_f32,
            current_mix_gain_right: 0_f32,
            previous_reverb_send: 0_f32,
            previous_chorus_send: 0_f32,
            current_reverb_send: 0_f32,
            current_chorus_send: 0_f32,
            channel: 0,
            key: 0,
            velocity: 0,
            played: 0,
            transpose: 0_f32,
            glide: None,
            note: NoteControls::DEFAULT,
            tune: 0_f32,
            note_gain: 0_f32,
            cutoff: 0_f32,
            resonance: 0_f32,
            vib_lfo_to_pitch: 0_f32,
            mod_lfo_to_pitch: 0_f32,
            mod_env_to_pitch: 0_f32,
            mod_lfo_to_cutoff: 0,
            mod_env_to_cutoff: 0,
            dynamic_cutoff: false,
            mod_lfo_to_volume: 0_f32,
            dynamic_volume: false,
            instrument_pan: 0_f32,
            instrument_reverb: 0_f32,
            instrument_chorus: 0_f32,
            smoothed_cutoff: 0_f32,
            voice_state: VoiceState::Playing,
            voice_length: 0,
        }
    }

    pub(crate) fn start(&mut self, region: &RegionPair, channel: i32, key: i32, velocity: i32) {
        self.played = key;
        self.transpose = 0_f32;
        self.glide = None;
        // Generators 46 and 47 (SF2 2.04 section 8.1.2) force the key and the
        // velocity of the zone: everything keyed on either reads the forced one.
        let key = region
            .instrument
            .forced_key()
            .map_or(key, |k| i32::from(k.get()));
        let velocity = region
            .instrument
            .forced_velocity()
            .map_or(velocity, |v| i32::from(v.get()));
        self.channel = channel;
        self.key = key;
        self.velocity = velocity;
        self.note = NoteControls::DEFAULT;

        if velocity > 0 {
            // The note's level in dB: the default velocity modulator (SF2 8.4.1, 960 cB on
            // the concave curve, read as 40 log10(v / 127) dB; at v = 1 this gives 84 dB, not
            // the curve's 127/128 of 960 cB), less generator 48's attenuation whole (8.1.2),
            // less half of generator 9's resonance, the DC cut the filter's Q brings (8.1.2).
            let sample_attenuation = region.initial_attenuation();
            let filter_attenuation = 0.5_f32 * region.initial_filter_q();
            let decibels = 2_f32 * SoundFontMath::linear_to_decibels(velocity as f32 / 127_f32)
                - sample_attenuation
                - filter_attenuation;
            self.note_gain = SoundFontMath::decibels_to_linear(decibels);
        } else {
            self.note_gain = 0_f32;
        }

        self.cutoff = region.initial_filter_cutoff_frequency();
        self.resonance = SoundFontMath::decibels_to_linear(region.initial_filter_q());

        self.vib_lfo_to_pitch = 0.01_f32 * region.vibrato_lfo_to_pitch() as f32;
        self.mod_lfo_to_pitch = 0.01_f32 * region.modulation_lfo_to_pitch() as f32;
        self.mod_env_to_pitch = 0.01_f32 * region.modulation_envelope_to_pitch() as f32;

        self.mod_lfo_to_cutoff = region.modulation_lfo_to_filter_cutoff_frequency();
        self.mod_env_to_cutoff = region.modulation_envelope_to_filter_cutoff_frequency();
        self.dynamic_cutoff = self.mod_lfo_to_cutoff != 0 || self.mod_env_to_cutoff != 0;

        self.mod_lfo_to_volume = region.modulation_lfo_to_volume();
        self.dynamic_volume = self.mod_lfo_to_volume > 0.05_f32;

        // A sample of a stereo pair sounds on its own side (section 7.10); any
        // other takes the zone's pan.
        self.instrument_pan = match region.instrument.sample_side() {
            Some(Side::Left) => -50_f32,
            Some(Side::Right) => 50_f32,
            None => SoundFontMath::clamp(region.pan(), -50_f32, 50_f32),
        };
        // Tenths of a percent to a fraction of full level (section 8.1.2,
        // generators 15 and 16).
        self.instrument_reverb = 0.001_f32 * region.reverb_effects_send() as f32;
        self.instrument_chorus = 0.001_f32 * region.chorus_effects_send() as f32;

        RegionEx::start_volume_envelope(&mut self.vol_env, region, key, velocity);
        RegionEx::start_modulation_envelope(&mut self.mod_env, region, key, velocity);
        RegionEx::start_vibrato(&mut self.vib_lfo, region, key, velocity);
        RegionEx::start_modulation(&mut self.mod_lfo, region, key, velocity);
        RegionEx::start_oscillator(&mut self.oscillator, region);
        self.history = [0_f32; 2];
        self.low_pass = low_pass(self.cutoff, self.resonance, self.rate);

        self.smoothed_cutoff = self.cutoff;

        self.voice_state = VoiceState::Playing;
        self.voice_length = 0;
    }

    /// Starts the glide a note begins with, if it has one: the voice sounds at
    /// the pitch of the glide's key and arrives at its own.
    pub(crate) fn glide(&mut self, glide: Option<Glide>) {
        self.glide = glide.map(|g| self.glide_run(g));
    }

    /// Sets how far the voice's key sounds from equal temperament, with no new
    /// attack. A glide in progress keeps the offset it left with.
    pub(crate) fn tune(&mut self, tune: Semitones) {
        // The voice's pitch state is `f32`.
        self.tune = tune.get() as f32;
    }

    /// Moves the voice to `key` with no new attack (a legato move, or the voice
    /// a Portamento Control names). With a glide it jumps to the glide's key and
    /// glides to the new one; without, it is at the new key at once.
    pub(crate) fn retarget(&mut self, key: NoteNumber, tune: Semitones, glide: Option<Glide>) {
        self.tune = tune.get() as f32;
        let key = i32::from(key.get());
        self.transpose += (key - self.played) as f32;
        self.played = key;
        self.glide = glide.map(|g| self.glide_run(g));
    }

    fn glide_run(&self, glide: Glide) -> GlideRun {
        GlideRun {
            offset: (i32::from(glide.from.get()) - self.played) as f32
                + glide.from_tune.get() as f32
                - self.tune,
            frames: (glide.time.get() * self.rate.get()) as f32,
            done: 0_f32,
        }
    }

    /// The glide's offset now, and its advance by `frames`.
    fn step_glide(&mut self, frames: usize) -> f32 {
        let Some(run) = &mut self.glide else {
            return 0_f32;
        };
        let offset = if run.frames > 0_f32 {
            run.offset * (1_f32 - run.done / run.frames)
        } else {
            0_f32
        };
        run.done += frames as f32;
        if run.done >= run.frames {
            self.glide = None;
        }
        offset
    }

    /// Takes a per-note controller's value (M2-104 section 7.4). What a
    /// SoundFont voice can carry: the note's sounding pitch (its told Pitch
    /// 7.25 or Attribute Pitch 7.9 and its per-note bend, summed by
    /// `tutti-voicing`; it replaces its key's, and the key still
    /// chooses the sample, envelope scaling and zone, as the voice was
    /// started), Registered Volume (the GM law, as CC 7)
    /// and Registered Pan (as CC 10, replacing the channel's pan for this
    /// note). Everything else (pressure, modulation, breath, balance, the
    /// sound controllers, the effects depths, the assignable controllers) has
    /// no generator in a SoundFont voice to drive and is dropped.
    pub(crate) fn control(&mut self, control: NoteControl) {
        match control {
            NoteControl::Pitch(pitch) => self.note.pitch = Some(pitch.note_number() as f32),
            NoteControl::Registered(RegisteredPerNoteController::Volume, value) => {
                // The voice's gains are `f32` DSP state: narrowed once here.
                self.note.volume = ControlValue::new(value).normalized().get() as f32;
            }
            NoteControl::Registered(RegisteredPerNoteController::Pan, value) => {
                // The pan is `f32` DSP state: narrowed once here.
                self.note.pan =
                    Some((100.0 * ControlValue::new(value).normalized().get() - 50.0) as f32);
            }
            NoteControl::Pressure(_)
            | NoteControl::Registered(..)
            | NoteControl::Assignable(..) => {}
        }
    }

    /// Per-Note Management Reset (M2-104 section 7.4.5): the note's per-note
    /// controllers return to their defaults.
    pub(crate) fn reset_controls(&mut self) {
        self.note = NoteControls::DEFAULT;
    }

    /// Lets the voice go into its release at once (SF2 2.04 section 9.1.7: the
    /// envelope enters its release phase from its current value, so a voice
    /// released in its delay stage, where that value is 0, never sounds). The
    /// volume and modulation envelopes' release stages start, and a loop that
    /// runs until the key is up ends. A voice already released is left alone.
    pub(crate) fn release(&mut self) {
        if self.voice_state == VoiceState::Playing {
            self.vol_env.release();
            self.mod_env.release();
            self.oscillator.release();
            self.voice_state = VoiceState::Released;
        }
    }

    /// Lets the voice go into the quick release of an exclusive class (SF2
    /// 2.04 section 8.1.2, generator 57): its volume
    /// envelope falls over [`QUICK_RELEASE`] at the longest, whatever its
    /// own release is. A voice already releasing is sped up, never slowed.
    pub(crate) fn damp(&mut self) {
        self.vol_env.release_within(QUICK_RELEASE);
        self.release();
    }

    /// The volume envelope's level now: what the core steals by.
    pub(crate) fn level(&self) -> f32 {
        self.vol_env.value()
    }

    pub(crate) fn process(
        &mut self,
        data: SamplePool<'_>,
        channels: &[Channel],
        states: &[ChannelState; MidiChannel::COUNT as usize],
        bends: &[Semitones; MidiChannel::COUNT as usize],
    ) -> bool {
        if self.note_gain < SoundFontMath::NON_AUDIBLE {
            return false;
        }

        let channel_info = &channels[self.channel as usize];
        let state = &states[self.channel as usize];

        if !self.vol_env.process(self.block.len()) {
            return false;
        }

        self.mod_env.process(self.block.len());
        self.vib_lfo.process();
        self.mod_lfo.process();

        // The wheel's depth in cents (the Modulation Depth Range of RPN 5 over its
        // position) and the generator's, as semitones.
        let vib_pitch_change = (0.01_f32 * state.modulation_depth().get() as f32
            + self.vib_lfo_to_pitch)
            * self.vib_lfo.value();
        let mod_pitch_change = self.mod_lfo_to_pitch * self.mod_lfo.value()
            + self.mod_env_to_pitch * self.mod_env.value();
        // The core's pitch for the channel: the wheel over its sensitivity and
        // the Channel Tuning (RP-022).
        let channel_pitch_change = bends[self.channel as usize].get() as f32;
        let glide = self.step_glide(self.block.len());
        let pitch = self.note.pitch.unwrap_or(self.key as f32 + self.tune)
            + self.transpose
            + glide
            + vib_pitch_change
            + mod_pitch_change
            + channel_pitch_change;
        if !self.oscillator.process(data, &mut self.block[..], pitch) {
            return false;
        }

        if self.dynamic_cutoff {
            let cents = self.mod_lfo_to_cutoff as f32 * self.mod_lfo.value()
                + self.mod_env_to_cutoff as f32 * self.mod_env.value();
            let factor = SoundFontMath::cents_to_multiplying_factor(cents);
            let new_cutoff = factor * self.cutoff;

            // The cutoff change is limited within x0.5 and x2 to reduce pop noise.
            let lower_limit = 0.5_f32 * self.smoothed_cutoff;
            let upper_limit = 2_f32 * self.smoothed_cutoff;
            self.smoothed_cutoff = SoundFontMath::clamp(new_cutoff, lower_limit, upper_limit);

            self.low_pass = low_pass(self.smoothed_cutoff, self.resonance, self.rate);
        }
        match &self.low_pass {
            Some(section) => {
                for sample in &mut self.block {
                    *sample = svf_step(section, &mut self.history, *sample);
                }
            }
            // The state a low-pass at DC would hold at the block's last sample, so the
            // section takes over without a step.
            None => {
                if let Some(&last) = self.block.last() {
                    self.history = [0_f32, last];
                }
            }
        }

        self.previous_mix_gain_left = self.current_mix_gain_left;
        self.previous_mix_gain_right = self.current_mix_gain_right;
        self.previous_reverb_send = self.current_reverb_send;
        self.previous_chorus_send = self.current_chorus_send;

        // According to the GM spec, the following value should be squared.
        let ve = state.volume() * state.expression();
        let channel_gain = ve * ve;

        // A per-note Volume is the channel's law (the GM square) applied to
        // the one note.
        let mut mix_gain = self.note_gain
            * channel_gain
            * self.note.volume
            * self.note.volume
            * self.vol_env.value();
        if self.dynamic_volume {
            let decibels = self.mod_lfo_to_volume * self.mod_lfo.value();
            mix_gain *= SoundFontMath::decibels_to_linear(decibels);
        }

        // RP-036's pan, in the percent the zone's pan adds to: the channel's or the
        // note's, plus the zone's, over the 50 that is hard right.
        let pan = self
            .note
            .pan
            .map_or_else(|| 50.0 * state.pan().get(), f64::from);
        let (left, right) = pan_gains(
            Pan((pan + f64::from(self.instrument_pan)) / 50.0),
            PanLaw::Rp036,
        );
        // The voice's mix gains are `f32`: the pan gains narrow into them.
        self.current_mix_gain_left = mix_gain * left.get() as f32;
        self.current_mix_gain_right = mix_gain * right.get() as f32;

        self.current_reverb_send = SoundFontMath::clamp(
            channel_info.reverb_send() + self.instrument_reverb,
            0_f32,
            1_f32,
        );
        self.current_chorus_send = SoundFontMath::clamp(
            channel_info.chorus_send() + self.instrument_chorus,
            0_f32,
            1_f32,
        );

        if self.voice_length == 0 {
            self.previous_mix_gain_left = self.current_mix_gain_left;
            self.previous_mix_gain_right = self.current_mix_gain_right;
            self.previous_reverb_send = self.current_reverb_send;
            self.previous_chorus_send = self.current_chorus_send;
        }

        self.voice_length += self.block.len();

        true
    }

    pub(crate) fn block(&self) -> &[f32] {
        &self.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_dsp::Complex64;
    use tutti_units::Amplitude;

    const RATE: SampleRate = SampleRate::SR_44K1;
    const CUTOFF: f32 = 500.0;

    /// The steady-state gain of the voice's low-pass at `hz`, measured by
    /// running a sine through the section the voice runs.
    fn gain(resonance: f32, hz: f32) -> f64 {
        let section = low_pass(CUTOFF, resonance, RATE).expect("below Nyquist");
        let mut state = [0_f32; 2];
        let w = core::f64::consts::TAU * f64::from(hz) / RATE.get();
        let mut peak = 0.0f64;
        for n in 0..8_000 {
            let x = (w * f64::from(n)).sin() as f32;
            let y = svf_step(&section, &mut state, x);
            if n >= 6_400 {
                peak = peak.max(f64::from(y).abs());
            }
        }
        peak
    }

    /// The RBJ low-pass closed form, from the analog prototype
    /// `1 / (s^2 + s / q + 1)` under the bilinear map: `|H|` at `hz` for a
    /// quality `q`, independent of the code under test.
    fn closed_form(q: f64, hz: f64) -> f64 {
        let s = (core::f64::consts::PI * hz / RATE.get()).tan()
            / (core::f64::consts::PI * f64::from(CUTOFF) / RATE.get()).tan();
        1.0 / ((1.0 - s * s).powi(2) + (s / q).powi(2)).sqrt()
    }

    /// The prototype's `q` whose peak height `q / sqrt(1 - 1 / (4 q^2))` is
    /// `resonance`, found by bisection on that closed form, so the oracle is
    /// not the voice's own inversion.
    fn q_of(resonance: f64) -> f64 {
        let peak = |q: f64| q / (1.0 - 0.25 / (q * q)).sqrt();
        let (mut lo, mut hi) = (0.5f64.sqrt(), resonance.max(1.0) + 1.0);
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if peak(mid) < resonance {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    /// SF2 2.04 section 8.1.3, initialFilterFc and initialFilterQ: the filter is
    /// a 2-pole low-pass whose gain at DC is 0 dB whatever the Q.
    #[test]
    fn the_low_pass_has_unity_gain_at_dc_whatever_the_resonance() {
        for db in [0.0f32, 6.0, 12.0, 24.0] {
            let g = gain(SoundFontMath::decibels_to_linear(db), 10.0);
            assert!((g - 1.0).abs() < 0.01, "{db} dB: {g}");
        }
    }

    /// At the cutoff the 2-pole response is `q` (RBJ: `|H(w0)| = Q`), and the
    /// resonant peak is the spec's initialFilterQ above DC. The sweep finds the
    /// peak on a 5 Hz grid from sine runs of 8000 frames sampled at 44.1 kHz, so
    /// the tolerance is the sum of three derived losses. Grid: `ln|H|` near its
    /// maximum at `x0` falls by `f'' dx^2 / (4 f0)`, with `f = (1 - x^2)^2 + x^2 /
    /// q^2`, `f'' = 8 - 4 / q^2`, `f0 = (4 q^2 - 1) / (4 q^4)` and `dx = 2.5 / 500`
    /// the half grid step. Sampling: the sine's crest falls between samples, at
    /// worst `1 - cos(pi / 88)` for 88 frames a period. Settling: the section's
    /// envelope `exp(-g n / (2 q))` after the 6400 frames before the measurement.
    /// Spec: sf2.synth.filter.resonance.peak, sf2.gen.initialFilterQ
    #[test]
    fn the_gain_at_the_cutoff_is_q_and_the_peak_is_the_spec_peak_height() {
        let g = (core::f64::consts::PI * f64::from(CUTOFF) / RATE.get()).tan();
        for db in [0.0f32, 6.0, 12.0, 24.0] {
            let resonance = SoundFontMath::decibels_to_linear(db);
            let want = f64::from(resonance);
            let q = q_of(want);
            let sampling = 1.0 - (core::f64::consts::PI / 88.0).cos();
            let settling = (-g * 6_400.0 / (2.0 * q)).exp();
            let at_cutoff = gain(resonance, CUTOFF);
            assert!(
                (at_cutoff / q - 1.0).abs() < sampling + settling,
                "{db} dB: {at_cutoff} vs {q}"
            );
            let peak = (300..700)
                .step_by(5)
                .map(|hz| gain(resonance, hz as f32))
                .fold(0.0f64, f64::max);
            let (f2, f0, dx) = (
                8.0 - 4.0 / (q * q),
                (4.0 * q * q - 1.0) / (4.0 * q.powi(4)),
                0.005,
            );
            let grid = f2 * dx * dx / (4.0 * f0);
            let tolerance = grid + sampling + settling;
            if db == 0.0 {
                // 0 dB asks for no peak: the Butterworth maximum is its DC gain, 1.
                assert!(peak < 1.0 + tolerance, "{db} dB: peak {peak}");
            } else {
                assert!(
                    (peak / want - 1.0).abs() < tolerance,
                    "{db} dB: peak {peak} vs {want}, tolerance {tolerance:e}"
                );
            }
        }
    }

    /// Above the cutoff the response follows the closed form and falls at
    /// 12 dB an octave (a 2-pole low-pass).
    /// Spec: sf2.synth.filter.shape, sf2.gen.initialFilterFc
    #[test]
    fn the_stopband_follows_the_closed_form_at_twelve_db_per_octave() {
        let resonance = SoundFontMath::decibels_to_linear(6.0);
        let q = q_of(f64::from(resonance));
        for hz in [2_000.0f32, 4_000.0, 8_000.0] {
            let got = gain(resonance, hz);
            let want = closed_form(q, f64::from(hz));
            assert!((got / want - 1.0).abs() < 0.02, "{hz} Hz: {got} vs {want}");
        }
        let octave = Db::from_amplitude(Amplitude::new(
            gain(resonance, 4_000.0) / gain(resonance, 2_000.0),
        ))
        .get();
        assert!((octave + 12.0).abs() < 1.0, "{octave} dB per octave");
    }

    /// A cutoff at the solve ceiling or above is a bypass.
    /// Spec: sf2.synth.filter.bypass, sf2.gen.initialFilterFc
    #[test]
    fn a_cutoff_near_nyquist_is_no_filter() {
        assert!(low_pass(0.4 * RATE.get() as f32, 1.0, RATE).is_some());
        assert!(low_pass(0.5 * RATE.get() as f32, 1.0, RATE).is_none());
    }

    /// `H(e^jw)` of the trapezoidal section with coefficients `c`, from its state
    /// space `s' = A s + B x`, `y = m0 x + m1 v1 + m2 v2` solved at `z = e^jw`:
    /// `ic1' = (2 a1 - 1) ic1 - 2 a2 ic2 + 2 a2 x`, `ic2' = 2 a2 ic1 + (1 - 2 a3)
    /// ic2 + 2 a3 x`, `v1 = a1 ic1 + a2 (x - ic2)`, `v2 = ic2 + a2 ic1 + a3 (x - ic2)`.
    fn response(c: &SvfCoefficients<f64>, w: f64) -> f64 {
        let z = Complex64::from_polar(1.0, w);
        let one = Complex64::new(1.0, 0.0);
        let (m00, m01) = (z - one * (2.0 * c.a1 - 1.0), one * (2.0 * c.a2));
        let (m10, m11) = (one * (-2.0 * c.a2), z - one * (1.0 - 2.0 * c.a3));
        let (b0, b1) = (one * (2.0 * c.a2), one * (2.0 * c.a3));
        let det = m00 * m11 - m01 * m10;
        let s1 = (b0 * m11 - m01 * b1) / det;
        let s2 = (m00 * b1 - m10 * b0) / det;
        let v1 = s1 * c.a1 + (one - s2) * c.a2;
        let v2 = s2 + s1 * c.a2 + (one - s2) * c.a3;
        (one * c.m0 + v1 * c.m1 + v2 * c.m2).norm()
    }

    /// The `f32` set widened exactly, to analyse what the voice runs.
    fn widened(c: &SvfCoefficients<f32>) -> SvfCoefficients<f64> {
        SvfCoefficients {
            a1: f64::from(c.a1),
            a2: f64::from(c.a2),
            a3: f64::from(c.a3),
            m0: f64::from(c.m0),
            m1: f64::from(c.m1),
            m2: f64::from(c.m2),
        }
    }

    /// The L1 norm of the impulse response from a unit error in `ic2eq` to the
    /// output, in `f64` over the narrowed coefficients, summed until the response
    /// is under 1e-12 for a whole 4800 frames.
    fn error_norm(c: &SvfCoefficients<f64>, frames: usize) -> f64 {
        let mut state = [0.0f64, 1.0];
        let mut norm = 0.0;
        let mut tail = 0.0f64;
        for n in 0..frames {
            let h = svf_step(c, &mut state, 0.0).abs();
            norm += h;
            if n + 4_800 >= frames {
                tail = tail.max(h);
            }
        }
        assert!(tail < 1e-12, "the response has not decayed: {tail:e}");
        norm
    }

    /// The frames the section needs to forget its start to `eps`: the slow
    /// envelope is `exp(-g n / (2 q))`, so `(2 q / g) ln(1 / eps)`.
    fn settle(q: f64, cutoff: f32, rate: SampleRate, eps: f64) -> usize {
        let g = (core::f64::consts::PI * f64::from(cutoff) / rate.get()).tan();
        (2.0 * q / g * (1.0 / eps).ln()).ceil() as usize
    }

    /// The `f32` voice filter at SF2's 20 Hz floor, 48 kHz and 96 kHz, at 0 dB
    /// and 12 dB, against its analytic response. `x` is a DC input not exactly
    /// representable after the section's scalings, so the state rounds.
    ///
    /// The rounding error bound is `A u (5 |h|_1 + 1)`, `A` the state's amplitude
    /// and `u = 2^-24`. From `svf_step`, the roundings on `A`-sized values are
    /// `v2 = ic2 + a2 ic1` (1), `v2 + a3 v3` (2) and `ic2' = 2 v2 - ic2` (3), each
    /// at most `A u` (the products `a2 ic1`, `a3 v3` and the sums for `v1`, `v3`,
    /// `ic1'` are small values and their errors are second order). Errors 1 and 2
    /// enter `ic2'` doubled, error 3 once, so `5 A u` in `ic2eq`, and each
    /// travels to the output through the impulse response of a unit `ic2eq`
    /// error, summing to at most `|h|_1`; `y = v2` also carries error 1 and 2
    /// directly, and is bounded by one more `A u` with the two errors folded.
    #[test]
    fn the_f32_low_pass_at_the_20_hz_floor_holds_dc_to_the_rounding_bound() {
        let u = f64::from(f32::EPSILON) / 2.0;
        let x = 0.3_f32;
        let a = f64::from(x);
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            for db in [0.0f32, 12.0] {
                let resonance = SoundFontMath::decibels_to_linear(db);
                let c32 = low_pass(20.0, resonance, rate).expect("below Nyquist");
                let q = q_of(f64::from(resonance));
                let frames = settle(q, 20.0, rate, 1e-13);
                let norm = error_norm(&widened(&c32), frames);
                let bound = a * u * (5.0 * norm + 1.0);
                let mut state = [0_f32; 2];
                let mut worst = 0.0f64;
                for n in 0..frames + 4_800 {
                    let y = svf_step(&c32, &mut state, x);
                    if n >= frames {
                        worst = worst.max((f64::from(y) - a).abs());
                    }
                }
                eprintln!("{rate:?} {db} dB: deviation {worst:e}, bound {bound:e}, |h|_1 {norm}");
                assert!(worst > 0.0, "{rate:?} {db} dB: no rounding occurred");
                assert!(
                    worst <= bound,
                    "{rate:?} {db} dB: deviation {worst:e} over {bound:e}"
                );
            }
        }
    }

    /// The `f32` voice filter at the 20 Hz floor has the analytic magnitude at
    /// the cutoff. The oracle is `|H(fc)|` of the `f32`-narrowed coefficients,
    /// evaluated in `f64`; the output is within the rounding bound of
    /// [`the_f32_low_pass_at_the_20_hz_floor_holds_dc_to_the_rounding_bound`]
    /// (relative to the state's amplitude `q`, which is `|H|` for a unit sine). The
    /// narrowing itself moves `|H(fc)|` by at most `u / (g (g + k)) + 6 u`. `a2`
    /// and `a3` keep relative precision (`g a1` and `g^2 a1`), and `ln|H|` at
    /// `x = 1` moves by at most 1 per unit relative change of `g` or `k`
    /// (`d ln|H| / d ln x = -f' / (2 f) = -1` at `x = 1`): `6 u` covers them. `a1 =
    /// 1 / (1 + g (g + k))` is within `g (g + k)` of 1, so its absolute rounding
    /// `u` is a relative change `u / (g (g + k))` of the damping term the section
    /// loses per step, and of `k`: the dominant term at this extreme.
    /// The window is whole periods, so the quadrature projection is exact.
    /// Spec: sf2.synth.filter.shape, sf2.gen.initialFilterFc
    #[test]
    fn the_f32_low_pass_at_the_20_hz_floor_has_its_analytic_magnitude_at_the_cutoff() {
        let u = f64::from(f32::EPSILON) / 2.0;
        let hz = 20.0;
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            assert_eq!(rate.get() % hz, 0.0, "the window is whole periods");
            let period = (rate.get() / hz) as usize;
            for db in [0.0f32, 12.0] {
                let resonance = SoundFontMath::decibels_to_linear(db);
                let model = low_pass_model(20.0, resonance, rate).expect("below Nyquist");
                let c64 = SvfCoefficients::<f64>::from(&model);
                let c32 = SvfCoefficients::<f32>::from(&model);
                let w = core::f64::consts::TAU * hz / rate.get();
                let (h64, h32) = (response(&c64, w), response(&widened(&c32), w));
                let q = q_of(f64::from(resonance));
                let g = (core::f64::consts::PI * 20.0 / rate.get()).tan();
                let narrowing = u / (g * (g + 1.0 / q)) + 6.0 * u;
                assert!(
                    (h32 / h64 - 1.0).abs() <= narrowing,
                    "{rate:?} {db} dB: narrowing moved |H| from {h64} to {h32}, bound {narrowing:e}"
                );
                let frames = settle(q, 20.0, rate, 1e-13);
                let norm = error_norm(&widened(&c32), frames);
                let bound = u * (5.0 * norm + 1.0);
                let mut state = [0_f32; 2];
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for n in 0..frames.next_multiple_of(period) + 8 * period {
                    let x = (w * n as f64).sin() as f32;
                    let y = f64::from(svf_step(&c32, &mut state, x));
                    if n >= frames.next_multiple_of(period) {
                        re += y * (w * n as f64).sin();
                        im += y * (w * n as f64).cos();
                    }
                }
                let magnitude = 2.0 * re.hypot(im) / (8 * period) as f64;
                eprintln!(
                    "{rate:?} {db} dB: |H| {magnitude}, oracle {h32}, bound {bound:e}, narrowing {:e}",
                    (h32 / h64 - 1.0).abs()
                );
                assert!(
                    (magnitude / h32 - 1.0).abs() <= bound,
                    "{rate:?} {db} dB: {magnitude} vs {h32}, bound {bound:e}"
                );
            }
        }
    }
}
