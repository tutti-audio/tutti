//! The voice bank: every sub-voice's DSP state, stored as structure-of-arrays
//! across SIMD lanes, and the one render loop that advances it.
//!
//! # Layout
//!
//! One lane per sub-voice. Voice `v`'s `U` unison sub-voices occupy lanes
//! `v*U .. v*U + U`, so a note and its unison stack sit next to each other and
//! share lane groups; a group is one lane vector per field. A
//! group none of whose lanes belongs to a sounding voice is skipped outright,
//! so the cost is proportional to the sounding sub-voices rounded up to a
//! group, not to the polyphony.
//!
//! A group is [`SynthFloat::Lanes`]: eight lanes (`f32x8`) at `f32`, four
//! (`f64x4`) at `f64`. The bank's state and arithmetic are in the synth's float;
//! the controls that reach it are quantities, crossed once per control step.
//!
//! This avoids a boxed processor per sub-voice ticked per sample through a
//! virtual call. Surge's `QuadFilterChain` and Vital's `poly_float` are the
//! same idea.
//!
//! # What is uniform and what is per lane
//!
//! The oscillator waveform, the filter topology (and, for the SVF, its `Q` and
//! tap) and the envelope shape come from the `SynthConfig` and are the same for
//! every lane — so the whole bank runs one monomorphized loop, and "grouping by
//! filter type" is a single group. Pitch, cutoff, resonance, envelope position
//! and the two output gains are per lane.
//!
//! # Control rate
//!
//! The synth writes lane targets once per control step (at most
//! [`CONTROL_BLOCK`] frames apart, and at every MIDI event boundary). The render
//! ramps each lane linearly from where it is to its target across the step, so
//! pitch — including a portamento glide — changes every sample rather than in
//! stairs, and a cutoff sweep recomputes `tan` once per step instead of per
//! sample. The envelope is the exception: its stages are sample-accurate.

use crate::kernel::{self, MAX_INCREMENT};
use crate::lane::{Lane, SynthFloat};
use crate::{Adsr, FilterType, OscillatorType, SvfMode, UnisonVoices};
use tutti_dsp::{
    EnvelopeDesign, EnvelopeLeg, EnvelopePace, EnvelopeStage, EnvelopeStages, EnvelopeStep,
    LadderCoefficients, LadderDesign, LadderType, SimdLanes, SvfCoefficients, SvfDesign, SvfType,
    ladder_step, svf_step,
};
use tutti_rt::SplitMix64;
use tutti_units::{
    Amplitude, Db, Hz, Normalized, Phase, PhaseIncrement, Q, Resonance, SampleRate, Seconds,
};

/// One group of the bank's lanes at the synth's float `F`.
type V<F> = <F as SynthFloat>::Lanes;
/// The noise generators of one group.
type Rng<F> = <V<F> as Lane>::Rng;

/// The longest run of frames rendered between two control steps.
///
/// Sixteen frames is 0.33 ms at 48 kHz: short enough that a linear ramp
/// between control points is inaudible as a ramp, long enough that the
/// per-step work (a `tan` per moving cutoff, two `sqrt`s per lane for the pan
/// law) is amortized sixteen-fold.
pub(crate) const CONTROL_BLOCK: usize = 16;

/// Below these deltas a cutoff/resonance change does not recompute the filter
/// coefficients — the same guard `SvfFilterNode`/`LadderFilterNode` use.
const FREQ_EPS: f64 = 0.01;
const RES_EPS: f64 = 0.0001;

/// The envelope design of an [`Adsr`] at `rate`: its stages are straight lines, a decay and a
/// release take their stated times from wherever they start, and an attack takes the share of
/// its time the level has left to rise.
fn envelope_design(env: &Adsr, rate: SampleRate) -> EnvelopeDesign {
    EnvelopeDesign::new(&EnvelopeStages::from(*env).pace(EnvelopePace::Stage), rate)
}

/// One group's envelope registers: the level, and the leg each lane runs as one affine move
/// per frame, `level * gain + slope`, until it has moved onto or past `target` in the
/// direction `dir`. A held lane has a `dir` of zero and never reaches.
#[derive(Clone, Copy)]
struct EnvRegs<F: SynthFloat> {
    level: V<F>,
    slope: V<F>,
    gain: V<F>,
    dir: V<F>,
    target: V<F>,
}

impl<F: SynthFloat> EnvRegs<F> {
    /// Lane `k` takes `leg`: its level jumps to the leg's start.
    fn enter(&mut self, k: usize, leg: &EnvelopeLeg) {
        let (slope, gain) = match leg.step::<F>() {
            EnvelopeStep::Hold => (F::ZERO, F::ONE),
            EnvelopeStep::Add(slope) => (slope, F::ONE),
            EnvelopeStep::Mul(gain) => (F::ZERO, gain),
        };
        let dir = if leg.rises() {
            F::ONE
        } else if leg.falls() {
            -F::ONE
        } else {
            F::ZERO
        };
        self.level.as_mut_slice()[k] = F::from_design(leg.from().get());
        self.slope.as_mut_slice()[k] = slope;
        self.gain.as_mut_slice()[k] = gain;
        self.dir.as_mut_slice()[k] = dir;
        self.target.as_mut_slice()[k] = F::from_design(leg.to().get());
    }
}

/// The oscillator every lane runs.
#[derive(Debug, Clone, Copy)]
enum Osc {
    Sine,
    Saw,
    Pulse { width: Normalized },
    Triangle,
    Noise,
}

/// The filter every lane runs.
#[derive(Debug, Clone, Copy)]
enum Filter<F> {
    None,
    /// Simper SVF (`SvfFilterNode`'s topology). `q` and the output tap are
    /// fixed for the synth, so `m` is uniform; only `d, a2, a3` vary per lane.
    Svf {
        ty: SvfType,
        q: Q,
        m: [F; 3],
    },
    /// Four-stage ladder with zero-delay `tanh` feedback (`LadderFilterNode`'s topology),
    /// read at the fourth stage: the 24 dB/octave Moog response.
    Ladder,
}

/// The narrowest pulse the oscillator renders, as a fraction of the cycle.
const MIN_PULSE_WIDTH: f64 = 0.01;

const OSC_SINE: u8 = 0;
const OSC_SAW: u8 = 1;
const OSC_PULSE: u8 = 2;
const OSC_TRIANGLE: u8 = 3;
const OSC_NOISE: u8 = 4;
const FILTER_NONE: u8 = 0;
const FILTER_SVF: u8 = 1;
const FILTER_LADDER: u8 = 2;

/// Every sub-voice's DSP state, in groups of [`SynthFloat::Lanes`].
#[derive(Clone)]
pub(crate) struct VoiceBank<F: SynthFloat> {
    osc: Osc,
    filter: Filter<F>,
    env_config: Adsr,
    design: EnvelopeDesign,
    /// Per-sample one-pole coefficient the output gains smooth with: the
    /// fraction of the gap one sample closes, a small number designed in `f64`.
    gain_coeff: F,
    sample_rate: SampleRate,
    /// Sub-voices per voice: lane stride between consecutive voices.
    stride: usize,
    voices: usize,

    // Oscillator.
    phase: Vec<V<F>>,
    /// The rounding error the phase's last sum left, which the next one takes back
    /// (compensated summation). Over `n` cycles the phase is the exact `n * inc` turns
    /// to within the stored increment's rounding (a constant pitch bias of `EPSILON / 2`
    /// relative, so `n * EPSILON / 2` turns) plus at most `EPSILON * inc / 2` once per
    /// cycle, on the sample after a wrap: there `|phase| >= |step|`, which the
    /// compensation (Fast2Sum) needs, does not hold. A plain sum instead loses up to half
    /// a step of the phase on every sample.
    phase_err: Vec<V<F>>,
    inc: Vec<V<F>>,
    inc_target: Vec<V<F>>,
    rng: Vec<Rng<F>>,

    // Filter: integrator state (SVF uses two, the ladder four) and the
    // coefficients (SVF `d`, `a2`, `a3`; ladder `g1`, `k`), current and target.
    z: Vec<[V<F>; 4]>,
    coef: Vec<[V<F>; 3]>,
    coef_target: Vec<[V<F>; 3]>,
    cutoff_seen: Vec<Hz>,
    res_seen: Vec<Resonance>,

    // Amplitude envelope.
    env: Vec<V<F>>,
    /// The rounding error the level's last sum left (as `phase_err`): a stage's
    /// slope under half the level's resolution still moves the level, so a
    /// 100 s release at 192 kHz does not stall.
    env_err: Vec<V<F>>,
    env_slope: Vec<V<F>>,
    env_gain: Vec<V<F>>,
    env_dir: Vec<V<F>>,
    env_target: Vec<V<F>>,
    stage: Vec<EnvelopeStage>,

    // Output gains (pan law × unison gain × voice gain), current and target.
    // Unlike pitch and cutoff these are smoothed by a one-pole at a fixed time
    // constant rather than ramping across the control step: `tick` takes a
    // control step every frame, so a step-long ramp there would be a one-sample
    // jump — a click on every velocity or pressure change.
    gain: Vec<[V<F>; 2]>,
    gain_target: Vec<[V<F>; 2]>,

    /// Groups holding at least one sounding lane this control step. Cleared by
    /// every render and re-marked by the next control step.
    live: Vec<bool>,
    out: [[F; CONTROL_BLOCK]; 2],
}

impl<F: SynthFloat> std::fmt::Debug for VoiceBank<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceBank")
            .field("osc", &self.osc)
            .field("filter", &self.filter)
            .field("env_config", &self.env_config)
            .field("design", &self.design)
            .field("gain_coeff", &self.gain_coeff)
            .field("sample_rate", &self.sample_rate)
            .field("stride", &self.stride)
            .field("voices", &self.voices)
            .finish_non_exhaustive()
    }
}

impl<F: SynthFloat> VoiceBank<F> {
    /// Build a bank for `voices` voices of `stride` sub-voices each.
    ///
    /// Allocates every lane array; control-thread only.
    pub(crate) fn new(
        oscillator: OscillatorType,
        filter: &FilterType,
        envelope: &Adsr,
        sample_rate: SampleRate,
        voices: usize,
        stride: UnisonVoices,
    ) -> Self {
        let osc = match oscillator {
            OscillatorType::Sine => Osc::Sine,
            OscillatorType::Saw => Osc::Saw,
            // Clamped short of both ends: at 0 or 1 the two edges coincide and
            // their PolyBLEP corrections overlap into a spike rather than
            // cancelling, and the "pulse" is DC.
            OscillatorType::Square { pulse_width } => Osc::Pulse {
                width: Normalized::clamped(
                    pulse_width
                        .get()
                        .clamp(MIN_PULSE_WIDTH, 1.0 - MIN_PULSE_WIDTH),
                ),
            },
            OscillatorType::Triangle => Osc::Triangle,
            OscillatorType::Noise => Osc::Noise,
        };
        let filter = match *filter {
            FilterType::None => Filter::None,
            FilterType::Moog { .. } => Filter::Ladder,
            FilterType::Svf { q, mode, .. } => {
                let ty = match mode {
                    SvfMode::Lowpass => SvfType::LowPass,
                    SvfMode::Highpass => SvfType::HighPass,
                    SvfMode::Bandpass => SvfType::BandPass,
                    SvfMode::Notch => SvfType::Notch,
                };
                // The output mix depends on the tap and `Q` alone, so any
                // cutoff gives the same `m`.
                let c = SvfDesign::new(ty, Hz::new(1000.0), q, Db::new(0.0), sample_rate);
                Filter::Svf {
                    ty,
                    q,
                    m: [c.m0, c.m1, c.m2].map(F::from_design),
                }
            }
        };
        let mut bank = Self {
            osc,
            filter,
            env_config: *envelope,
            design: envelope_design(envelope, sample_rate),
            gain_coeff: F::from_design(GAIN_TIME_CONSTANT.one_pole_coeff(sample_rate)),
            sample_rate,
            stride: usize::from(stride.get()),
            voices,
            phase: Vec::new(),
            phase_err: Vec::new(),
            inc: Vec::new(),
            inc_target: Vec::new(),
            rng: Vec::new(),
            z: Vec::new(),
            coef: Vec::new(),
            coef_target: Vec::new(),
            cutoff_seen: Vec::new(),
            res_seen: Vec::new(),
            env: Vec::new(),
            env_err: Vec::new(),
            env_slope: Vec::new(),
            env_gain: Vec::new(),
            env_dir: Vec::new(),
            env_target: Vec::new(),
            stage: Vec::new(),
            gain: Vec::new(),
            gain_target: Vec::new(),
            live: Vec::new(),
            out: [[F::ZERO; CONTROL_BLOCK]; 2],
        };
        bank.allocate_lanes();
        bank
    }

    /// Group and lane-within-group of a flat lane index.
    #[inline(always)]
    fn split(lane: usize) -> (usize, usize) {
        split::<V<F>>(lane)
    }

    fn lanes(&self) -> usize {
        self.voices * self.stride
    }

    fn groups(&self) -> usize {
        self.lanes().div_ceil(V::<F>::LANES)
    }

    /// Size every lane array to `voices * stride`, zeroed.
    fn allocate_lanes(&mut self) {
        let groups = self.groups();
        let lanes = groups * V::<F>::LANES;
        let zero = V::<F>::ZERO;
        self.phase = vec![zero; groups];
        self.phase_err = vec![zero; groups];
        self.inc = vec![zero; groups];
        self.inc_target = vec![zero; groups];
        // A distinct seed per lane: two lanes with one seed would play the same noise in
        // both ears.
        let mut counter = 0u64;
        self.rng = (0..groups)
            .map(|_| {
                V::<F>::seed_rng(|| {
                    counter += 1;
                    seed_from(counter)
                })
            })
            .collect();
        self.z = vec![[zero; 4]; groups];
        self.coef = vec![[zero; 3]; groups];
        self.coef_target = vec![[zero; 3]; groups];
        // NaN never compares within epsilon, so the first `set_filter` always
        // computes coefficients.
        self.cutoff_seen = vec![Hz::new(f64::NAN); lanes];
        self.res_seen = vec![Resonance::new(f64::NAN); lanes];
        self.env = vec![zero; groups];
        self.env_err = vec![zero; groups];
        self.env_slope = vec![zero; groups];
        self.env_gain = vec![V::<F>::splat(F::ONE); groups];
        self.env_dir = vec![zero; groups];
        self.env_target = vec![zero; groups];
        self.stage = vec![EnvelopeStage::Idle; lanes];
        self.gain = vec![[zero; 2]; groups];
        self.gain_target = vec![[zero; 2]; groups];
        self.live = vec![false; groups];
    }

    /// Lane of voice `voice`'s sub-voice `sub`.
    #[inline]
    pub(crate) fn lane(&self, voice: usize, sub: usize) -> usize {
        voice * self.stride + sub
    }

    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Re-derive everything rate-dependent. Pitch increments and filter
    /// coefficients are rederived at the next control step, from the `Hz` the
    /// voices hold; the envelope slopes are rederived here.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.design = envelope_design(&self.env_config, sample_rate);
        self.gain_coeff = F::from_design(GAIN_TIME_CONSTANT.one_pole_coeff(sample_rate));
        // The SVF's output mix depends on the tap and `Q`, not the rate, so
        // only the per-lane coefficients need recomputing.
        self.cutoff_seen.fill(Hz::new(f64::NAN));
        self.res_seen.fill(Resonance::new(f64::NAN));
    }

    /// Zero every lane's state: silence, idle envelopes, cleared filters.
    ///
    /// Fills in place rather than reallocating, so it is safe wherever the
    /// host calls `reset`. The noise generators keep running: a reset that
    /// reseeded them would replay the same noise after every transport stop.
    pub(crate) fn reset(&mut self) {
        let zero = V::<F>::ZERO;
        for field in [
            &mut self.phase,
            &mut self.phase_err,
            &mut self.inc,
            &mut self.inc_target,
            &mut self.env,
            &mut self.env_err,
            &mut self.env_slope,
            &mut self.env_dir,
            &mut self.env_target,
        ] {
            field.fill(zero);
        }
        self.env_gain.fill(V::<F>::splat(F::ONE));
        self.z.fill([zero; 4]);
        self.coef.fill([zero; 3]);
        self.coef_target.fill([zero; 3]);
        self.gain.fill([zero; 2]);
        self.gain_target.fill([zero; 2]);
        self.cutoff_seen.fill(Hz::new(f64::NAN));
        self.res_seen.fill(Resonance::new(f64::NAN));
        self.stage.fill(EnvelopeStage::Idle);
        self.live.fill(false);
    }

    // --- Control-step writes, one lane at a time -------------------------

    /// Aim the lane's oscillator at `freq`.
    pub(crate) fn set_pitch(&mut self, lane: usize, freq: Hz) {
        let inc = PhaseIncrement::per_sample(freq, self.sample_rate)
            .clamp(PhaseIncrement::new(0.0), MAX_INCREMENT);
        set(&mut self.inc_target, lane, F::from_unit(inc));
    }

    /// Aim the filters of `lanes` consecutive lanes from `first` — one voice's
    /// sub-voices, which share a cutoff — at `cutoff` (and, for the ladder,
    /// `resonance`). Computes the coefficients once for all of them, and only
    /// when either value moved past its epsilon.
    pub(crate) fn set_filter(
        &mut self,
        first: usize,
        lanes: usize,
        cutoff: Hz,
        resonance: Resonance,
    ) {
        let unchanged = (cutoff.get() - self.cutoff_seen[first].get()).abs() <= FREQ_EPS
            && (resonance.get() - self.res_seen[first].get()).abs() <= RES_EPS;
        if unchanged {
            return;
        }
        let coef = match self.filter {
            Filter::None => return,
            Filter::Svf { ty, q, .. } => {
                let s = SvfDesign::new(ty, cutoff, q, Db::new(0.0), self.sample_rate);
                [s.d, s.a2, s.a3].map(F::from_design)
            }
            Filter::Ladder => {
                let l = LadderDesign::new(cutoff, resonance, self.sample_rate);
                [l.g / (1.0 + l.g), l.k, 0.0].map(F::from_design)
            }
        };
        for lane in first..first + lanes {
            self.cutoff_seen[lane] = cutoff;
            self.res_seen[lane] = resonance;
            let (g, k) = Self::split(lane);
            for (i, c) in coef.into_iter().enumerate() {
                self.coef_target[g][i].as_mut_slice()[k] = c;
            }
        }
    }

    /// Set the lane's left and right output gains.
    pub(crate) fn set_gains(&mut self, lane: usize, left: Amplitude, right: Amplitude) {
        let (g, k) = Self::split(lane);
        self.gain_target[g][0].as_mut_slice()[k] = F::from_unit(left);
        self.gain_target[g][1].as_mut_slice()[k] = F::from_unit(right);
    }

    /// Jump the lane's pitch and filter straight to their targets, so a new
    /// note starts on its own pitch and cutoff rather than gliding in from the
    /// previous note's. Changes the waveform's slope, never its value, so it
    /// cannot click.
    pub(crate) fn snap_tone(&mut self, lane: usize) {
        let (g, k) = Self::split(lane);
        let inc = self.inc_target[g].as_slice()[k];
        self.inc[g].as_mut_slice()[k] = inc;
        for i in 0..3 {
            let c = self.coef_target[g][i].as_slice()[k];
            self.coef[g][i].as_mut_slice()[k] = c;
        }
    }

    /// Jump the lane's output gains straight to their targets.
    ///
    /// Only for a silent lane: on a sounding one this is a step in the output
    /// — a click — which is why a stolen voice ramps its gain instead.
    pub(crate) fn snap_gain(&mut self, lane: usize) {
        let (g, k) = Self::split(lane);
        for i in 0..2 {
            let c = self.gain_target[g][i].as_slice()[k];
            self.gain[g][i].as_mut_slice()[k] = c;
        }
    }

    /// Prepare a silent lane for a new note: oscillator at `phase`, filter
    /// cleared.
    ///
    /// Only for a lane whose envelope is at zero — resetting a sounding lane's
    /// phase is an audible click, which is why a stolen or retriggered voice
    /// skips this and keeps running.
    pub(crate) fn start(&mut self, lane: usize, phase: Phase) {
        let (g, k) = Self::split(lane);
        self.phase[g].as_mut_slice()[k] = F::from_unit(phase);
        self.phase_err[g].as_mut_slice()[k] = F::ZERO;
        for z in &mut self.z[g][..4] {
            z.as_mut_slice()[k] = F::ZERO;
        }
    }

    /// Open the lane's gate: attack from wherever the envelope is.
    pub(crate) fn gate_on(&mut self, lane: usize) {
        self.enter(lane, EnvelopeStage::Attack);
    }

    /// Close the lane's gate: release from wherever the envelope is.
    pub(crate) fn gate_off(&mut self, lane: usize) {
        self.enter(lane, EnvelopeStage::Release);
    }

    /// Silence the lane immediately (All Sound Off): envelope to zero, filter
    /// cleared.
    pub(crate) fn kill(&mut self, lane: usize) {
        self.enter_at(lane, EnvelopeStage::Idle, F::ZERO);
        self.start(lane, Phase::START);
        self.set_gains(lane, Amplitude::SILENT, Amplitude::SILENT);
        self.snap_gain(lane);
    }

    /// [`kill`](Self::kill) every sub-voice of voice `voice`.
    pub(crate) fn kill_voice(&mut self, voice: usize) {
        for sub in 0..self.stride {
            self.kill(self.lane(voice, sub));
        }
    }

    fn enter(&mut self, lane: usize, stage: EnvelopeStage) {
        let (g, k) = Self::split(lane);
        let level = self.env[g].as_slice()[k];
        self.enter_at(lane, stage, level);
    }

    fn enter_at(&mut self, lane: usize, stage: EnvelopeStage, level: F) {
        let leg = self.design.enter(stage, Amplitude::new(level.into()));
        let (g, k) = Self::split(lane);
        let mut regs = self.regs(g);
        regs.enter(k, &leg);
        self.stage[lane] = leg.stage();
        self.env_err[g].as_mut_slice()[k] = F::ZERO;
        self.set_regs(g, regs);
    }

    /// Group `g`'s envelope registers.
    fn regs(&self, g: usize) -> EnvRegs<F> {
        EnvRegs {
            level: self.env[g],
            slope: self.env_slope[g],
            gain: self.env_gain[g],
            dir: self.env_dir[g],
            target: self.env_target[g],
        }
    }

    fn set_regs(&mut self, g: usize, regs: EnvRegs<F>) {
        self.env[g] = regs.level;
        self.env_slope[g] = regs.slope;
        self.env_gain[g] = regs.gain;
        self.env_dir[g] = regs.dir;
        self.env_target[g] = regs.target;
    }

    /// Include this lane's group in the next render.
    #[inline]
    pub(crate) fn mark_live(&mut self, lane: usize) {
        self.live[lane / V::<F>::LANES] = true;
    }

    /// Whether the lane's envelope has finished its release.
    pub(crate) fn is_idle(&self, lane: usize) -> bool {
        self.stage[lane] == EnvelopeStage::Idle
    }

    /// The lane's envelope level, `0.0..=1.0`.
    pub(crate) fn envelope(&self, lane: usize) -> Amplitude {
        let (g, k) = Self::split(lane);
        Amplitude::new(self.env[g].as_slice()[k].into())
    }

    /// The lane's oscillator phase. Observability for tests.
    #[cfg(test)]
    pub(crate) fn phase(&self, lane: usize) -> F {
        let (g, k) = Self::split(lane);
        self.phase[g].as_slice()[k]
    }

    // --- Render ----------------------------------------------------------

    /// Render `n <= CONTROL_BLOCK` frames of every live group and return the
    /// summed `(left, right)` mix.
    ///
    /// Clears the live set, so a lane only sounds in the step after the
    /// control code marks it.
    pub(crate) fn render(&mut self, n: usize) -> (&[F], &[F]) {
        debug_assert!(n <= CONTROL_BLOCK);
        let mut acc = [[V::<F>::ZERO; 2]; CONTROL_BLOCK];
        let osc = match self.osc {
            Osc::Sine => OSC_SINE,
            Osc::Saw => OSC_SAW,
            Osc::Pulse { .. } => OSC_PULSE,
            Osc::Triangle => OSC_TRIANGLE,
            Osc::Noise => OSC_NOISE,
        };
        let filter = match self.filter {
            Filter::None => FILTER_NONE,
            Filter::Svf { .. } => FILTER_SVF,
            Filter::Ladder => FILTER_LADDER,
        };
        for g in 0..self.live.len() {
            if !self.live[g] {
                continue;
            }
            self.live[g] = false;
            // One monomorphized loop per (oscillator, filter) pair: the match
            // runs once per group per step, never per sample.
            macro_rules! dispatch {
                ($($o:ident),*; $($f:ident),*) => {
                    dispatch!(@o [$($o),*] [$($f),*])
                };
                (@o [$($o:ident),*] $fs:tt) => {
                    match osc {
                        $($o => dispatch!(@f $o $fs),)*
                        _ => unreachable!(),
                    }
                };
                (@f $o:ident [$($f:ident),*]) => {
                    match filter {
                        $($f => self.render_group::<$o, $f>(g, n, &mut acc),)*
                        _ => unreachable!(),
                    }
                };
            }
            dispatch!(OSC_SINE, OSC_SAW, OSC_PULSE, OSC_TRIANGLE, OSC_NOISE;
                      FILTER_NONE, FILTER_SVF, FILTER_LADDER);
        }
        for (i, frame) in acc.iter().enumerate().take(n) {
            self.out[0][i] = frame[0].sum();
            self.out[1][i] = frame[1].sum();
        }
        (&self.out[0][..n], &self.out[1][..n])
    }

    #[inline(always)]
    fn render_group<const OSC: u8, const FILTER: u8>(
        &mut self,
        g: usize,
        n: usize,
        acc: &mut [[V<F>; 2]; CONTROL_BLOCK],
    ) {
        let ramp = V::<F>::splat(F::from_design(1.0 / n as f64));

        let mut phase = self.phase[g];
        let mut phase_err = self.phase_err[g];
        let mut inc = self.inc[g];
        let inc_target = self.inc_target[g];
        let inc_step = (inc_target - inc) * ramp;
        let width = match self.osc {
            Osc::Pulse { width } => V::<F>::splat(F::from_unit(width)),
            _ => V::<F>::lit(0.5),
        };
        let mut rng = self.rng[g].clone();

        let mut z = self.z[g];
        let mut c = self.coef[g];
        let c_target = self.coef_target[g];
        let c_step = [
            (c_target[0] - c[0]) * ramp,
            (c_target[1] - c[1]) * ramp,
            (c_target[2] - c[2]) * ramp,
        ];
        let m = match self.filter {
            Filter::Svf { m, .. } => m.map(V::<F>::splat),
            _ => [V::<F>::ZERO; 3],
        };

        let mut env = self.regs(g);
        let mut env_err = self.env_err[g];

        let [mut gain_l, mut gain_r] = self.gain[g];
        let [gain_l_target, gain_r_target] = self.gain_target[g];
        let smoothing = V::<F>::splat(self.gain_coeff);

        for frame in acc.iter_mut().take(n) {
            inc += inc_step;
            // `max` keeps an idle lane's zero increment from dividing by zero;
            // the corrections it feeds are masked to zero there anyway.
            let dt = inc.max(V::<F>::lit(1e-9));
            let x = match OSC {
                OSC_SINE => kernel::sine(phase),
                OSC_SAW => kernel::saw(phase, dt, V::<F>::ONE / dt),
                OSC_PULSE => kernel::pulse(phase, width, dt, V::<F>::ONE / dt),
                OSC_TRIANGLE => kernel::triangle(phase, dt, V::<F>::ONE / dt),
                _ => V::<F>::noise(&mut rng),
            };
            // The phase adds its increment compensated: the error of each sum is
            // taken back by the next, so the pitch is the increment's however
            // small it is beside the phase.
            let step = inc - phase_err;
            let sum = phase + step;
            phase_err = (sum - phase) - step;
            phase = kernel::wrap_once(sum);

            let y = match FILTER {
                FILTER_SVF => {
                    c[0] += c_step[0];
                    c[1] += c_step[1];
                    c[2] += c_step[2];
                    let coefficients = SvfCoefficients {
                        d: c[0],
                        a2: c[1],
                        a3: c[2],
                        m0: m[0],
                        m1: m[1],
                        m2: m[2],
                    };
                    let mut s = [z[0], z[1]];
                    let y = svf_step(&coefficients, &mut s, x);
                    [z[0], z[1]] = s;
                    y
                }
                FILTER_LADDER => {
                    c[0] += c_step[0];
                    c[1] += c_step[1];
                    let coefficients = LadderCoefficients { g1: c[0], k: c[1] };
                    ladder_step(&coefficients, LadderType::LP24, &mut z, x, kernel::tanh)
                }
                _ => x,
            };

            // The level scales by its gain and adds its slope compensated, as the phase does.
            let scaled = env.level * env.gain;
            let step = env.slope - env_err;
            let sum = scaled + step;
            env_err = (sum - scaled) - step;
            env.level = sum;
            // A lane reached its stage's target when it has moved onto or past
            // it in the direction of travel; a held stage never "reaches".
            let reached =
                ((env.level - env.target) * env.dir).ge(V::<F>::ZERO) & env.dir.ne(V::<F>::ZERO);
            if reached.any() {
                env.level = reached.pick(env.target, env.level);
                env_err = reached.pick(V::<F>::ZERO, env_err);
                self.advance_envelopes(g, &mut env, reached);
            }

            gain_l = (gain_l_target - gain_l).mul_add(smoothing, gain_l);
            gain_r = (gain_r_target - gain_r).mul_add(smoothing, gain_r);
            let out = y * env.level;
            frame[0] += out * gain_l;
            frame[1] += out * gain_r;
        }

        self.phase[g] = phase;
        self.phase_err[g] = phase_err;
        // Land exactly on the targets rather than on the accumulated ramp, so
        // rounding in the ramp never builds up across steps.
        self.inc[g] = inc_target;
        self.rng[g] = rng;
        self.z[g] = z;
        self.coef[g] = c_target;
        self.set_regs(g, env);
        self.env_err[g] = env_err;
        self.gain[g] = [gain_l, gain_r];
    }

    /// Move every lane in `reached` on to its next envelope stage.
    ///
    /// Out of line and scalar: a lane changes stage a handful of times per
    /// note, so this runs on a vanishing fraction of samples. The `env_err` of
    /// every lane in `reached` was zeroed in `render_group` when it took its
    /// target, so each next stage starts uncompensated.
    #[cold]
    #[inline(never)]
    fn advance_envelopes(&mut self, g: usize, env: &mut EnvRegs<F>, reached: V<F>) {
        let reached = reached.bits();
        for k in 0..V::<F>::LANES {
            if reached & (1 << k) == 0 {
                continue;
            }
            let lane = g * V::<F>::LANES + k;
            let level = Amplitude::new(env.level.as_slice()[k].into());
            let leg = self.design.enter(self.stage[lane].next(), level);
            self.stage[lane] = leg.stage();
            env.enter(k, &leg);
        }
    }
}

/// The time constant of the output-gain one-pole smoother (not `tutti_dsp`'s 5 ms
/// linear `GAIN_RAMP`): half a millisecond settles a
/// velocity change to 1% in about 2.3 ms — inaudible as a fade, long enough
/// that no single sample steps by more than 4% of the change at 48 kHz.
const GAIN_TIME_CONSTANT: Seconds = Seconds::new(0.0005);

/// The `n`th noise seed: [`SplitMix64::mix`] of the counter, a bijection, so distinct counters
/// give distinct seeds and no stream is another's shifted copy.
fn seed_from(n: u64) -> u64 {
    SplitMix64::mix(n)
}

/// Group and lane-within-group of a flat lane index.
#[inline(always)]
fn split<V: Lane>(lane: usize) -> (usize, usize) {
    (lane / V::LANES, lane % V::LANES)
}

#[inline(always)]
fn set<V: Lane>(field: &mut [V], lane: usize, value: V::Scalar) {
    let (g, k) = split::<V>(lane);
    field[g].as_mut_slice()[k] = value;
}

#[cfg(test)]
mod tests;
