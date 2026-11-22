// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_buffers::Float;
use tutti_dsp::{
    PanLaw, SOLVE_CEILING, SimdLanes, SvfCoefficients, SvfControls, SvfModel, SvfType, pan_gains,
    svf_step,
};
use tutti_sf2::{SamplePool, Side};
use tutti_ump::{MidiChannel, NoteNumber, RegisteredPerNoteController};
use tutti_units::{
    Amplitude, Cents, Db, Hz, Normalized, Pan, Q, SampleRate, Samples, Seconds, Semitones,
};
use tutti_voicing::{ChannelState, Glide, NoteControl};

/// The longest an exclusive class's quick release takes, to fall
/// 100 dB as every release time does (section 8.1.2, generator 38): the floor
/// every release is given at its start, to avoid a pop. The spec says "rapidly"
/// and no more (SF2 2.04 section 8.1.2, generator 57); this is tutti's figure,
/// the shortest release a voice can have.
pub(crate) const QUICK_RELEASE: Seconds = Seconds::new(0.01);

use crate::channel::Channel;
use crate::lfo::Lfo;
use crate::modulation_envelope::ModulationEnvelope;
use crate::oscillator::Oscillator;
use crate::region_ex::RegionEx;
use crate::region_pair::RegionPair;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;
use crate::volume_envelope::VolumeEnvelope;

/// The voice's low-pass for a cutoff and a resonance as a linear peak
/// height above the DC gain (SF2 2.04 section 8.1.3, generators 8 and 9: the
/// cutoff is the filter's corner and the Q is the height of the resonant peak
/// in decibels, here already converted). `None` from [`SOLVE_CEILING`] of the
/// rate up: the filter would do nothing there.
///
/// The filter is the trapezoidal state-variable low-pass, `1 / (s^2 + s / q + 1)`
/// under the prewarped bilinear map, designed in `f64` and crossed once to the
/// voice's float. Its DC gain is 1 for every `q`. The prototype's peak height is
/// `P = q / sqrt(1 - 1 / (4 q^2))` for `q > 1 / sqrt(2)` (the maximum of
/// `|H|^2 = 1 / ((1 - x^2)^2 + x^2 / q^2)` over `x`, at `x^2 = 1 - 1 / (2 q^2)`),
/// and the bilinear map moves the peak along the frequency axis and not its
/// height. Solving `q^4 - P^2 q^2 + P^2 / 4 = 0` for the peak `P` the spec asks
/// for gives `q^2 = (P^2 + P sqrt(P^2 - 1)) / 2`, so the peak is the resonance
/// up to [`SvfControls::MAX_Q`] (a resonance of about 60 dB; the model clamps
/// `q` there and a larger resonance peaks at that cap), and a resonance of 1
/// (0 dB) is `q = 1 / sqrt(2)`, Butterworth.
fn low_pass<F: Float + SimdLanes>(
    cutoff: Hz,
    resonance: Amplitude,
    rate: SampleRate,
) -> Option<SvfCoefficients<F>> {
    low_pass_model(cutoff, resonance, rate).map(|model| SvfCoefficients::from(&model))
}

/// The model [`low_pass`] crosses: the `f64` design of the voice's low-pass.
fn low_pass_model(cutoff: Hz, resonance: Amplitude, rate: SampleRate) -> Option<SvfModel> {
    if cutoff.get() >= SOLVE_CEILING * rate.get() {
        return None;
    }
    // A resonance below 0 dB asks for no peak.
    let peak = resonance.get().max(1.0);
    let q = libm::sqrt(0.5 * (peak * peak + peak * libm::sqrt(peak * peak - 1.0)));
    let controls = SvfControls {
        ty: SvfType::LowPass,
        freq: cutoff,
        q: Q::new(q),
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
    /// The sounding pitch (the told pitch with the per-note bend
    /// in it, `tutti-voicing`'s), if one was set; else the key's.
    pitch: Option<Semitones>,
    /// Per-note volume.
    volume: Amplitude,
    /// Per-note pan, if one was set.
    pan: Option<Pan>,
}

impl NoteControls {
    const DEFAULT: Self = Self {
        pitch: None,
        volume: Amplitude::UNITY,
        pan: None,
    };
}

/// A glide in progress: the pitch is `offset` above the note's own and falls
/// to it in a straight line over `frames` frames, a constant rate in cents
/// (General MIDI 2 section 3.3.3 gives the portamento rate in cents a
/// millisecond).
#[derive(Debug, Clone, Copy)]
struct GlideRun {
    offset: Semitones,
    frames: Samples,
    done: Samples,
}

/// A voice: its envelopes, LFOs, oscillator and filter, rendering a chunk at a time in the
/// graph's float `F`. Its controls are designed in `f64` quantities, at its start and, for the filter, per chunk
/// while the cutoff moves; what reaches `F` is the chunk of audio, the filter's coefficients and state, and the gains
/// the mix ramps between.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Voice<F: Float + SimdLanes> {
    vol_env: VolumeEnvelope,
    mod_env: ModulationEnvelope,

    vib_lfo: Lfo,
    mod_lfo: Lfo,

    oscillator: Oscillator,
    rate: SampleRate,
    /// The low-pass, designed at the start and per chunk while the cutoff moves, or `None`
    /// while the cutoff is too near Nyquist to filter.
    low_pass: Option<SvfCoefficients<F>>,
    /// The low-pass's integrator state `[ic1eq, ic2eq]`.
    history: [F; 2],

    block: Vec<F>,

    // A sudden change in the mix gain will cause pop noise.
    // To avoid this, we save the mix gain of the previous block,
    // and smooth out the gain if the gap between the current and previous gain is too large.
    // The actual smoothing process is done in the WriteBlock method of the Synthesizer class.
    pub(crate) previous_mix_gain_left: F,
    pub(crate) previous_mix_gain_right: F,
    pub(crate) current_mix_gain_left: F,
    pub(crate) current_mix_gain_right: F,

    pub(crate) previous_reverb_send: F,
    pub(crate) previous_chorus_send: F,
    pub(crate) current_reverb_send: F,
    pub(crate) current_chorus_send: F,

    channel: i32,
    key: i32,
    velocity: i32,
    /// The key the note is played on now: where it started, or where a legato
    /// move or a Portamento Control last took it (the zone's forced key, which
    /// `key` follows, is not it).
    played: i32,
    /// How far the voice has been moved from the key it started on.
    transpose: Semitones,
    glide: Option<GlideRun>,

    // The note's per-note controllers (M2-104 section 7.4); the defaults of
    // `NoteControls::DEFAULT` until a message sets one.
    note: NoteControls,
    /// How far the key sounds from equal temperament: the note logic's tuning
    /// of it (`VoiceCommand::Start`'s `tune`).
    tune: Semitones,

    note_gain: Amplitude,

    cutoff: Hz,
    /// The resonance as the linear height of the filter's peak.
    resonance: Amplitude,

    vib_lfo_to_pitch: Semitones,
    mod_lfo_to_pitch: Semitones,
    mod_env_to_pitch: Semitones,

    mod_lfo_to_cutoff: Cents,
    mod_env_to_cutoff: Cents,
    dynamic_cutoff: bool,

    mod_lfo_to_volume: Db,
    dynamic_volume: bool,

    instrument_pan: Pan,
    /// The zone's reverb and chorus sends, as fractions of full level.
    instrument_reverb: Amplitude,
    instrument_chorus: Amplitude,

    // Some instruments require fast cutoff change, which can cause pop noise.
    // This is used to smooth out the cutoff frequency.
    smoothed_cutoff: Hz,

    voice_state: VoiceState,
    /// Time elapsed in samples, so a voice's first block takes its gains whole.
    voice_length: usize,
}

impl<F: Float + SimdLanes> Voice<F> {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            vol_env: VolumeEnvelope::new(settings),
            mod_env: ModulationEnvelope::new(settings),
            vib_lfo: Lfo::new(settings),
            mod_lfo: Lfo::new(settings),
            oscillator: Oscillator::new(settings),
            rate: settings.rate(),
            low_pass: None,
            history: [F::ZERO; 2],
            block: vec![F::ZERO; settings.block_len()],
            previous_mix_gain_left: F::ZERO,
            previous_mix_gain_right: F::ZERO,
            current_mix_gain_left: F::ZERO,
            current_mix_gain_right: F::ZERO,
            previous_reverb_send: F::ZERO,
            previous_chorus_send: F::ZERO,
            current_reverb_send: F::ZERO,
            current_chorus_send: F::ZERO,
            channel: 0,
            key: 0,
            velocity: 0,
            played: 0,
            transpose: Semitones::new(0.0),
            glide: None,
            note: NoteControls::DEFAULT,
            tune: Semitones::new(0.0),
            note_gain: Amplitude::SILENT,
            cutoff: Hz::new(0.0),
            resonance: Amplitude::SILENT,
            vib_lfo_to_pitch: Semitones::new(0.0),
            mod_lfo_to_pitch: Semitones::new(0.0),
            mod_env_to_pitch: Semitones::new(0.0),
            mod_lfo_to_cutoff: Cents::new(0.0),
            mod_env_to_cutoff: Cents::new(0.0),
            dynamic_cutoff: false,
            mod_lfo_to_volume: Db::UNITY,
            dynamic_volume: false,
            instrument_pan: Pan::new(0.0),
            instrument_reverb: Amplitude::SILENT,
            instrument_chorus: Amplitude::SILENT,
            smoothed_cutoff: Hz::new(0.0),
            voice_state: VoiceState::Playing,
            voice_length: 0,
        }
    }

    pub(crate) fn start(&mut self, region: &RegionPair, channel: i32, key: i32, velocity: i32) {
        self.played = key;
        self.transpose = Semitones::new(0.0);
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
            let curve = Db::from_amplitude(Amplitude::new(f64::from(velocity) / 127.0));
            let filter_attenuation = Db::new(0.5 * region.initial_filter_q().get());
            self.note_gain =
                (curve + curve - region.initial_attenuation() - filter_attenuation).to_amplitude();
        } else {
            self.note_gain = Amplitude::SILENT;
        }

        self.cutoff = region.initial_filter_cutoff_frequency();
        self.resonance = region.initial_filter_q().to_amplitude();

        // A generator's cents as the semitones it moves the pitch by.
        self.vib_lfo_to_pitch = region.vibrato_lfo_to_pitch().into();
        self.mod_lfo_to_pitch = region.modulation_lfo_to_pitch().into();
        self.mod_env_to_pitch = region.modulation_envelope_to_pitch().into();

        self.mod_lfo_to_cutoff = region.modulation_lfo_to_filter_cutoff_frequency();
        self.mod_env_to_cutoff = region.modulation_envelope_to_filter_cutoff_frequency();
        self.dynamic_cutoff =
            self.mod_lfo_to_cutoff.get() != 0.0 || self.mod_env_to_cutoff.get() != 0.0;

        self.mod_lfo_to_volume = region.modulation_lfo_to_volume();
        self.dynamic_volume = self.mod_lfo_to_volume > Db::new(0.05);

        // A sample of a stereo pair sounds on its own side (section 7.10); any
        // other takes the zone's pan.
        self.instrument_pan = match region.instrument.sample_side() {
            Some(Side::Left) => Pan::new(-1.0),
            Some(Side::Right) => Pan::new(1.0),
            None => region.pan(),
        };
        // Tenths of a percent to a fraction of full level (section 8.1.2,
        // generators 15 and 16).
        self.instrument_reverb = Amplitude::new(0.001 * f64::from(region.reverb_effects_send()));
        self.instrument_chorus = Amplitude::new(0.001 * f64::from(region.chorus_effects_send()));

        RegionEx::start_volume_envelope(&mut self.vol_env, region, key, velocity);
        RegionEx::start_modulation_envelope(&mut self.mod_env, region, key, velocity);
        RegionEx::start_vibrato(&mut self.vib_lfo, region, key, velocity);
        RegionEx::start_modulation(&mut self.mod_lfo, region, key, velocity);
        RegionEx::start_oscillator(&mut self.oscillator, region);
        self.history = [F::ZERO; 2];
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
        self.tune = tune;
    }

    /// Moves the voice to `key` with no new attack (a legato move, or the voice
    /// a Portamento Control names). With a glide it jumps to the glide's key and
    /// glides to the new one; without, it is at the new key at once.
    pub(crate) fn retarget(&mut self, key: NoteNumber, tune: Semitones, glide: Option<Glide>) {
        self.tune = tune;
        let key = i32::from(key.get());
        self.transpose += Semitones::new(f64::from(key - self.played));
        self.played = key;
        self.glide = glide.map(|g| self.glide_run(g));
    }

    fn glide_run(&self, glide: Glide) -> GlideRun {
        GlideRun {
            offset: Semitones::new(f64::from(i32::from(glide.from.get()) - self.played))
                + glide.from_tune
                - self.tune,
            frames: glide.time.to_samples(self.rate),
            done: Samples::new(0),
        }
    }

    /// The glide's offset now, and its advance by `frames`.
    fn step_glide(&mut self, frames: usize) -> Semitones {
        let Some(run) = &mut self.glide else {
            return Semitones::new(0.0);
        };
        let offset = if run.frames.get() > 0 {
            run.offset * (1.0 - Normalized::progress(run.done, run.frames).get())
        } else {
            Semitones::new(0.0)
        };
        run.done = Samples::new(run.done.get() + frames);
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
            NoteControl::Pitch(pitch) => {
                self.note.pitch = Some(Semitones::new(pitch.note_number()))
            }
            NoteControl::Registered(RegisteredPerNoteController::Volume, value) => {
                self.note.volume = Amplitude::new(value.normalized().get());
            }
            NoteControl::Registered(RegisteredPerNoteController::Pan, value) => {
                self.note.pan = Some(Pan::new(2.0 * value.normalized().get() - 1.0));
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
    pub(crate) fn level(&self) -> Amplitude {
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

        // The wheel's depth (the Modulation Depth Range of RPN 5 over its
        // position) and the generator's.
        let vib_pitch_change = (Semitones::from(state.modulation_depth()) + self.vib_lfo_to_pitch)
            * self.vib_lfo.value().get();
        let mod_pitch_change = self.mod_lfo_to_pitch * self.mod_lfo.value().get()
            + self.mod_env_to_pitch * self.mod_env.value().get();
        // The core's pitch for the channel: the wheel over its sensitivity and
        // the Channel Tuning (RP-022).
        let channel_pitch_change = bends[self.channel as usize];
        let glide = self.step_glide(self.block.len());
        let pitch = self
            .note
            .pitch
            .unwrap_or(Semitones::new(f64::from(self.key)) + self.tune)
            + self.transpose
            + glide
            + vib_pitch_change
            + mod_pitch_change
            + channel_pitch_change;
        if !self.oscillator.process(data, &mut self.block[..], pitch) {
            return false;
        }

        if self.dynamic_cutoff {
            let cents = self.mod_lfo_to_cutoff * self.mod_lfo.value().get()
                + self.mod_env_to_cutoff * self.mod_env.value().get();
            let new_cutoff = self.cutoff * cents.to_pitch_ratio();

            // The cutoff change is limited within x0.5 and x2 to reduce pop noise.
            let lower_limit = self.smoothed_cutoff * 0.5;
            let upper_limit = self.smoothed_cutoff * 2.0;
            self.smoothed_cutoff = new_cutoff.clamp(lower_limit, upper_limit);

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
                    self.history = [F::ZERO, last];
                }
            }
        }

        self.previous_mix_gain_left = self.current_mix_gain_left;
        self.previous_mix_gain_right = self.current_mix_gain_right;
        self.previous_reverb_send = self.current_reverb_send;
        self.previous_chorus_send = self.current_chorus_send;

        // According to the GM spec, the following value should be squared.
        let ve = f64::from(state.volume()) * f64::from(state.expression());
        let channel_gain = ve * ve;

        // A per-note Volume is the channel's law (the GM square) applied to
        // the one note.
        let note_volume = self.note.volume.get();
        let mut mix_gain = self.note_gain
            * (channel_gain * note_volume * note_volume * self.vol_env.value().get());
        if self.dynamic_volume {
            let level = Db::new(self.mod_lfo_to_volume.get() * self.mod_lfo.value().get());
            mix_gain *= level.to_amplitude().get();
        }

        // RP-036's pan: the channel's or the note's, plus the zone's.
        let pan = self.note.pan.unwrap_or_else(|| state.pan());
        let (left, right) = pan_gains(
            Pan::new(pan.get() + self.instrument_pan.get()),
            PanLaw::Rp036,
        );
        self.current_mix_gain_left = F::from_unit(mix_gain * left.get());
        self.current_mix_gain_right = F::from_unit(mix_gain * right.get());

        self.current_reverb_send =
            F::from_unit(send(channel_info.reverb_send(), self.instrument_reverb));
        self.current_chorus_send =
            F::from_unit(send(channel_info.chorus_send(), self.instrument_chorus));

        if self.voice_length == 0 {
            self.previous_mix_gain_left = self.current_mix_gain_left;
            self.previous_mix_gain_right = self.current_mix_gain_right;
            self.previous_reverb_send = self.current_reverb_send;
            self.previous_chorus_send = self.current_chorus_send;
        }

        self.voice_length += self.block.len();

        true
    }

    pub(crate) fn block(&self) -> &[F] {
        &self.block
    }
}

/// A send: the channel's level and the zone's, summed and limited to the generator's
/// range (section 8.1.3, 0 to 1000 tenths of a percent).
fn send(channel: Amplitude, instrument: Amplitude) -> Amplitude {
    Amplitude::new(channel.get() + instrument.get()).clamp(Amplitude::SILENT, Amplitude::UNITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_dsp::Complex64;
    const RATE: SampleRate = SampleRate::SR_44K1;
    const CUTOFF: Hz = Hz::new(500.0);
    /// SF2's lowest filter cutoff, 1500 absolute cents (generator 8): about 20 Hz.
    const FLOOR: Hz = Hz::new(20.0);

    /// The steady-state gain of the voice's low-pass at `hz`, measured by
    /// running a sine through the section the voice runs.
    fn gain(resonance: Amplitude, hz: f64) -> f64 {
        let section = low_pass::<f32>(CUTOFF, resonance, RATE).expect("below Nyquist");
        let mut state = [0_f32; 2];
        let w = core::f64::consts::TAU * hz / RATE.get();
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
            / (core::f64::consts::PI * CUTOFF.get() / RATE.get()).tan();
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
        for db in [0.0, 6.0, 12.0, 24.0] {
            let g = gain(Db::new(db).to_amplitude(), 10.0);
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
        let g = (core::f64::consts::PI * CUTOFF.get() / RATE.get()).tan();
        for db in [0.0, 6.0, 12.0, 24.0] {
            let resonance = Db::new(db).to_amplitude();
            let want = resonance.get();
            let q = q_of(want);
            let sampling = 1.0 - (core::f64::consts::PI / 88.0).cos();
            let settling = (-g * 6_400.0 / (2.0 * q)).exp();
            let at_cutoff = gain(resonance, CUTOFF.get());
            assert!(
                (at_cutoff / q - 1.0).abs() < sampling + settling,
                "{db} dB: {at_cutoff} vs {q}"
            );
            let peak = (300..700)
                .step_by(5)
                .map(|hz| gain(resonance, f64::from(hz)))
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
        let resonance = Db::new(6.0).to_amplitude();
        let q = q_of(resonance.get());
        for hz in [2_000.0, 4_000.0, 8_000.0] {
            let got = gain(resonance, hz);
            let want = closed_form(q, hz);
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
        let none = Amplitude::UNITY;
        assert!(low_pass::<f32>(Hz::new(0.4 * RATE.get()), none, RATE).is_some());
        assert!(low_pass::<f32>(Hz::new(0.5 * RATE.get()), none, RATE).is_none());
    }

    /// `H(e^jw)` of the trapezoidal section with coefficients `c`, from its state
    /// space `s' = A s + B x`, `y = m0 x + m1 v1 + m2 v2` solved at `z = e^jw`:
    /// `ic1' = (2 a1 - 1) ic1 - 2 a2 ic2 + 2 a2 x`, `ic2' = 2 a2 ic1 + (1 - 2 a3)
    /// ic2 + 2 a3 x`, `v1 = a1 ic1 + a2 (x - ic2)`, `v2 = ic2 + a2 ic1 + a3 (x - ic2)`, with
    /// `a1 = 1 - d`.
    fn response(c: &SvfCoefficients<f64>, w: f64) -> f64 {
        let z = Complex64::from_polar(1.0, w);
        let one = Complex64::new(1.0, 0.0);
        let a1 = 1.0 - c.d;
        let (m00, m01) = (z - one * (2.0 * a1 - 1.0), one * (2.0 * c.a2));
        let (m10, m11) = (one * (-2.0 * c.a2), z - one * (1.0 - 2.0 * c.a3));
        let (b0, b1) = (one * (2.0 * c.a2), one * (2.0 * c.a3));
        let det = m00 * m11 - m01 * m10;
        let s1 = (b0 * m11 - m01 * b1) / det;
        let s2 = (m00 * b1 - m10 * b0) / det;
        let v1 = s1 * a1 + (one - s2) * c.a2;
        let v2 = s2 + s1 * c.a2 + (one - s2) * c.a3;
        (one * c.m0 + v1 * c.m1 + v2 * c.m2).norm()
    }

    /// The `f32` set widened exactly, to analyse what the voice runs.
    fn widened(c: &SvfCoefficients<f32>) -> SvfCoefficients<f64> {
        SvfCoefficients {
            d: f64::from(c.d),
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
    fn settle(q: f64, cutoff: Hz, rate: SampleRate, eps: f64) -> usize {
        let g = (core::f64::consts::PI * cutoff.get() / rate.get()).tan();
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
            for db in [0.0, 12.0] {
                let resonance = Db::new(db).to_amplitude();
                let c32 = low_pass::<f32>(FLOOR, resonance, rate).expect("below Nyquist");
                let q = q_of(resonance.get());
                let frames = settle(q, FLOOR, rate, 1e-13);
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
        let hz = FLOOR.get();
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            assert_eq!(rate.get() % hz, 0.0, "the window is whole periods");
            let period = (rate.get() / hz) as usize;
            for db in [0.0, 12.0] {
                let resonance = Db::new(db).to_amplitude();
                let model = low_pass_model(FLOOR, resonance, rate).expect("below Nyquist");
                let c64 = SvfCoefficients::<f64>::from(&model);
                let c32 = SvfCoefficients::<f32>::from(&model);
                let w = core::f64::consts::TAU * hz / rate.get();
                let (h64, h32) = (response(&c64, w), response(&widened(&c32), w));
                let q = q_of(resonance.get());
                let g = (core::f64::consts::PI * FLOOR.get() / rate.get()).tan();
                let narrowing = u / (g * (g + 1.0 / q)) + 6.0 * u;
                assert!(
                    (h32 / h64 - 1.0).abs() <= narrowing,
                    "{rate:?} {db} dB: narrowing moved |H| from {h64} to {h32}, bound {narrowing:e}"
                );
                let frames = settle(q, FLOOR, rate, 1e-13);
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

    /// A font whose one instrument zone has `release` timecents of volume-envelope release.
    fn font_with_release(release: i16) -> tutti_sf2::SoundFont {
        use tutti_fixtures::sf2::{Inst, Preset, bytes, generator};
        let file = bytes(
            &[Preset {
                name: "p",
                bank: 0,
                program: 0,
                zones: vec![(vec![], 0)],
            }],
            &[Inst {
                name: "i",
                zones: vec![vec![(generator::RELEASE_VOL_ENV, release)]],
            }],
        );
        tutti_sf2::SoundFont::open(file).expect("the built SoundFont parses")
    }

    /// The left mix gain of a voice at `rate` the `last`th chunk after its key-off over the
    /// `first`th's, with the font's one zone struck at velocity 127 and run through its
    /// attack to full level before the key-off.
    fn gain_ratio_after_release<F: Float + SimdLanes>(
        font: &tutti_sf2::SoundFont,
        rate: SampleRate,
        first: usize,
        last: usize,
    ) -> f64 {
        let channels = vec![Channel::new(false); 16];
        let states = [ChannelState::new(Semitones::new(2.0)); 16];
        let bends = [Semitones::new(0.0); 16];
        let preset = &font.presets()[0].regions()[0];
        let instrument = &font.instruments()[preset.instrument_id()].regions()[0];
        let mut voice = Voice::<F>::new(&Render::new(rate));
        voice.start(&RegionPair::new(preset, instrument), 0, 60, 127);
        let data = font.sample_pool();
        let chunks = |voice: &mut Voice<F>, n: usize| {
            for _ in 0..n {
                assert!(voice.process(data, &channels, &states, &bends));
            }
        };
        // The delay, attack, hold and decay at their default millisecond each are 96 chunks at
        // 192 kHz.
        chunks(&mut voice, 200);
        voice.release();
        chunks(&mut voice, first);
        let g1: f64 = voice.current_mix_gain_left.into();
        chunks(&mut voice, last - first);
        let g2: f64 = voice.current_mix_gain_left.into();
        g2 / g1
    }

    /// The longest volume-envelope release SF2 allows, 8000 timecents (`R = 2^(8000/1200)` s,
    /// about 101.6 s), at the highest rate a voice runs at, 192 kHz: the mix gain 25 s into the
    /// release over its first chunk's is `exp(-5 ln 10 dt / R)` (the release falls 100 dB, a
    /// factor `10^-5` in amplitude, in `R`), whatever the float the voice mixes in. Returns the
    /// relative deviation, asserted within `2 u + 1e-15`: each gain is an `f64` figure rounded
    /// once to `F` (relative `u = eps / 2`), their quotient is taken in `f64`, and `1e-15`
    /// covers the `f64` evaluation of the closed form (a time is a count over the rate, the
    /// fall one `exp`).
    ///
    /// The envelope is a closed form of the time, so no float stalls it. A one-pole `e <- r e`
    /// with `r = exp(-5 ln 10 / (R fs))` would: at `f32`, `1 - r = 5.9e-7` sits on a grid of
    /// `2^-24 = 6e-8`, so the pole's exponent is wrong by up to 5%.
    fn deviation_at_the_longest_release<F: Float + SimdLanes>() -> f64 {
        let rate = SampleRate::SR_192K;
        let release = 2f64.powf(8000.0 / 1200.0);
        let (first, last) = (1, (25.0 * rate.get() / 8.0) as usize);
        let seconds = (last - first) as f64 * 8.0 / rate.get();
        let want = (-5.0 * 10f64.ln() * seconds / release).exp();
        let got = gain_ratio_after_release::<F>(&font_with_release(8000), rate, first, last);
        let deviation = (got / want - 1.0).abs();
        let epsilon: f64 = F::EPSILON.into();
        let bound = epsilon + 1e-15;
        assert!(deviation <= bound, "deviation {deviation:e} over {bound:e}");
        deviation
    }

    /// The `f32` voice at the longest release and the highest rate has its closed-form level
    /// (see `deviation_at_the_longest_release`): the bound is `2 u + 1e-15 = 1.19e-7`.
    /// Spec: sf2.synth.env.release-time, sf2.gen.releaseVolEnv
    #[test]
    fn an_f32_voice_has_the_closed_form_level_at_the_longest_release_and_highest_rate() {
        deviation_at_the_longest_release::<f32>();
    }

    /// The `f64` voice at the same figure, within `2 u + 1e-15` for the `f64` `u`.
    /// Spec: sf2.synth.env.release-time, sf2.gen.releaseVolEnv
    #[test]
    fn an_f64_voice_has_the_closed_form_level_at_the_longest_release_and_highest_rate() {
        deviation_at_the_longest_release::<f64>();
    }
}
