//! The multi-stage envelope: one design of stage timing and curves, read by a per-chunk
//! runner ([`Envelope`]) and, as per-frame increments, by a vector bank.
//!
//! # Stages
//!
//! An envelope is delay, attack, hold, decay, sustain and release ([`EnvelopeStage`]), then
//! idle. The delay holds silence, the attack rises to unity, the hold stays at unity, the
//! decay falls from unity to the sustain level, the sustain holds, and the release falls to
//! silence from wherever the level is when the key goes up. A retrigger attacks from the
//! current level.
//!
//! # Curves and time
//!
//! The attack is a straight line in amplitude. A decay and a release are a straight line in
//! amplitude ([`EnvelopeCurve::Linear`]) or a straight line in decibels
//! ([`EnvelopeCurve::DbLinear`]), which falls a stated depth over the time and is the
//! exponential of a constant rate. A dB-linear fall ends at its floor, `depth` below unity,
//! and the level then goes to the stage's own level: silence after a release.
//!
//! [`EnvelopePace`] says what a decay or release time means. At
//! [`FullScale`](EnvelopePace::FullScale) the time is what a fall over the whole scale takes
//! (unity to silence, or `depth` dB), so a fall from a lower level, or to a sustain level,
//! takes proportionally less. At [`Stage`](EnvelopePace::Stage) the stage takes the time,
//! whatever level it starts from and ends at. An attack is always at the full-scale pace, so
//! a retrigger from a high level takes less than the attack time.
//!
//! A time is kept exactly, as a count of frames that may be fractional: a stage ends where its
//! time ends, between two frames if need be, and the next begins from there. A stage of zero
//! time takes none: the level goes to the stage's end at once, whatever the curve, and the next
//! stage begins.
//!
//! # One law, two readers
//!
//! [`EnvelopeDesign`] is designed once, in `f64`. [`EnvelopeDesign::enter`] gives the
//! [`EnvelopeLeg`] a stage runs: its start and end levels, its length in frames, and its law.
//! [`EnvelopeLeg::level_at`] is the closed form of the level at an elapsed frame count, and
//! [`EnvelopeLeg::step`] is the same law as one per-frame increment in the float `F`, crossed
//! once. [`Envelope`] advances by the closed form, so a caller that reads a chunk at a time
//! and one that reads every frame see the same levels.

use tutti_buffers::Float;
use tutti_music::Adsr;
use tutti_units::{Amplitude, Db, SampleRate, Samples, Seconds};

/// A relative tolerance on a leg's length: a leg whose elapsed frames are within it of its
/// length has ended, so a rounding error in a product never costs a frame.
const END_TOLERANCE: f64 = 1e-12;

/// The nepers in a decibel of amplitude: `ln 10 / 20`.
const NEPERS_PER_DB: f64 = core::f64::consts::LN_10 / 20.0;

/// The shape of a decay or a release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EnvelopeCurve {
    /// A straight line in amplitude.
    Linear,
    /// A straight line in decibels: the level falls `depth` over the stage time. The
    /// magnitude of `depth` is the fall; a depth of zero is a fall of no length.
    DbLinear {
        /// The fall over the whole scale.
        depth: Db,
    },
}

/// What a decay or release time measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopePace {
    /// The time of a fall over the whole scale; a shorter fall takes proportionally less.
    FullScale,
    /// The time the stage takes, from any start level to its end level.
    Stage,
}

/// The stages of an envelope, in units: the control-side description
/// [`EnvelopeDesign::new`] turns into frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeStages {
    delay: Seconds,
    attack: Seconds,
    hold: Seconds,
    decay: Seconds,
    sustain: Amplitude,
    release: Seconds,
    decay_curve: EnvelopeCurve,
    release_curve: EnvelopeCurve,
    pace: EnvelopePace,
}

impl EnvelopeStages {
    /// Stages with no delay and no hold, linear curves and the full-scale pace.
    /// The sustain is held within `0..=1`.
    #[must_use]
    pub const fn new(
        attack: Seconds,
        decay: Seconds,
        sustain: Amplitude,
        release: Seconds,
    ) -> Self {
        Self {
            delay: Seconds::new(0.0),
            attack,
            hold: Seconds::new(0.0),
            decay,
            sustain,
            release,
            decay_curve: EnvelopeCurve::Linear,
            release_curve: EnvelopeCurve::Linear,
            pace: EnvelopePace::FullScale,
        }
    }

    /// Silence for `delay` before the attack.
    #[must_use]
    pub const fn delay(mut self, delay: Seconds) -> Self {
        self.delay = delay;
        self
    }

    /// Unity for `hold` between the attack and the decay.
    #[must_use]
    pub const fn hold(mut self, hold: Seconds) -> Self {
        self.hold = hold;
        self
    }

    /// The decay's curve.
    #[must_use]
    pub const fn decay_curve(mut self, curve: EnvelopeCurve) -> Self {
        self.decay_curve = curve;
        self
    }

    /// The release's curve.
    #[must_use]
    pub const fn release_curve(mut self, curve: EnvelopeCurve) -> Self {
        self.release_curve = curve;
        self
    }

    /// What the decay and release times measure.
    #[must_use]
    pub const fn pace(mut self, pace: EnvelopePace) -> Self {
        self.pace = pace;
        self
    }
}

impl From<Adsr> for EnvelopeStages {
    /// The four stages of an [`Adsr`], linear and at the full-scale pace.
    fn from(adsr: Adsr) -> Self {
        Self::new(adsr.attack, adsr.decay, adsr.sustain, adsr.release)
    }
}

/// Where an envelope is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnvelopeStage {
    /// Silence before the attack.
    Delay,
    /// The rise to unity.
    Attack,
    /// Unity, held.
    Hold,
    /// The fall from unity to the sustain level.
    Decay,
    /// The sustain level, held until the release.
    Sustain,
    /// The fall to silence.
    Release,
    /// Silence after the release.
    Idle,
}

impl EnvelopeStage {
    /// The stage after this one when its leg ends; the sustain and the idle stage have none
    /// and stay.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Delay => Self::Attack,
            Self::Attack => Self::Hold,
            Self::Hold => Self::Decay,
            Self::Decay => Self::Sustain,
            Self::Release => Self::Idle,
            Self::Sustain => Self::Sustain,
            Self::Idle => Self::Idle,
        }
    }
}

/// How a leg's level moves from one frame to the next, in `f64`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Law {
    Hold,
    /// `level += per_frame`.
    Linear {
        per_frame: f64,
    },
    /// `level *= exp(ln_per_frame)`.
    DbLinear {
        ln_per_frame: f64,
    },
}

/// One frame's move of a leg's level in the float `F`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EnvelopeStep<F> {
    /// The level stays.
    Hold,
    /// The level gains this each frame.
    Add(F),
    /// The level is multiplied by this each frame.
    Mul(F),
}

/// What one stage does from its start: the level it starts at and ends at, how many frames it
/// lasts, and the law between.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeLeg {
    stage: EnvelopeStage,
    from: f64,
    to: f64,
    law: Law,
    frames: f64,
}

impl EnvelopeLeg {
    fn hold(stage: EnvelopeStage, level: f64, frames: f64) -> Self {
        Self {
            stage,
            from: level,
            to: level,
            law: Law::Hold,
            frames,
        }
    }

    /// The stage the leg is.
    #[must_use]
    pub const fn stage(&self) -> EnvelopeStage {
        self.stage
    }

    /// The level the leg starts at.
    #[must_use]
    pub fn from(&self) -> Amplitude {
        Amplitude::new(self.from)
    }

    /// The level the leg ends at, and the next leg starts from.
    #[must_use]
    pub fn to(&self) -> Amplitude {
        Amplitude::new(self.to)
    }

    /// The time the leg lasts at `rate`; a sustain or an idle leg never ends.
    #[must_use]
    pub fn duration(&self, rate: SampleRate) -> Seconds {
        Seconds::new(self.frames / rate.get())
    }

    /// Whether `elapsed` frames end the leg.
    fn ended_by(&self, elapsed: f64) -> bool {
        elapsed >= self.frames * (1.0 - END_TOLERANCE)
    }

    /// Whether the level rises over the leg.
    #[must_use]
    pub fn rises(&self) -> bool {
        self.to > self.from
    }

    /// Whether the level falls over the leg.
    #[must_use]
    pub fn falls(&self) -> bool {
        self.to < self.from
    }

    /// The level `elapsed` frames into the leg, in closed form; the leg's end level from
    /// its last frame on.
    #[must_use]
    pub fn level_at(&self, elapsed: Samples) -> Amplitude {
        Amplitude::new(self.level_after(elapsed.get() as f64))
    }

    fn level_after(&self, elapsed: f64) -> f64 {
        if self.ended_by(elapsed) {
            return self.to;
        }
        match self.law {
            Law::Hold => self.from,
            Law::Linear { per_frame } => self.from + per_frame * elapsed,
            Law::DbLinear { ln_per_frame } => self.from * libm::exp(ln_per_frame * elapsed),
        }
    }

    /// The law as one per-frame move in the float `F`, designed in `f64` and crossed once.
    #[must_use]
    pub fn step<F: Float>(&self) -> EnvelopeStep<F> {
        match self.law {
            Law::Hold => EnvelopeStep::Hold,
            Law::Linear { per_frame } => EnvelopeStep::Add(F::from_design(per_frame)),
            Law::DbLinear { ln_per_frame } => {
                EnvelopeStep::Mul(F::from_design(libm::exp(ln_per_frame)))
            }
        }
    }
}

/// A decay or release resolved to frames: its curve and the frames a full-scale fall takes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fall {
    /// Frames of a fall over the whole scale.
    frames: f64,
    /// The decibels of the whole scale, or `None` for a straight line in amplitude.
    depth: Option<f64>,
}

impl Fall {
    fn new(time: Seconds, curve: EnvelopeCurve, rate: SampleRate) -> Self {
        Self {
            frames: frames(time, rate),
            depth: match curve {
                EnvelopeCurve::Linear => None,
                EnvelopeCurve::DbLinear { depth } => Some(depth.get().abs()),
            },
        }
    }

    /// The lowest level the fall reaches: silence, or `depth` below unity.
    fn floor(&self) -> f64 {
        self.depth
            .map_or(0.0, |depth| libm::exp(-depth * NEPERS_PER_DB))
    }

    /// The leg of `stage` from `from` down to `to`, where `to` is at or above the floor.
    fn leg(&self, stage: EnvelopeStage, from: f64, to: f64, pace: EnvelopePace) -> EnvelopeLeg {
        let hold = |level| EnvelopeLeg::hold(stage, level, 0.0);
        let (span, whole) = match self.depth {
            None => (from - to, 1.0),
            Some(depth) => {
                if depth == 0.0 || to <= 0.0 {
                    return hold(to);
                }
                (libm::log(from / to), depth * NEPERS_PER_DB)
            }
        };
        if span <= 0.0 || self.frames < END_TOLERANCE {
            return hold(to);
        }
        let (frames, per_frame) = match pace {
            EnvelopePace::FullScale => (span / whole * self.frames, whole / self.frames),
            EnvelopePace::Stage => (self.frames, span / self.frames),
        };
        let law = match self.depth {
            None => Law::Linear {
                per_frame: -per_frame,
            },
            Some(_) => Law::DbLinear {
                ln_per_frame: -per_frame,
            },
        };
        EnvelopeLeg {
            stage,
            from,
            to,
            law,
            frames,
        }
    }
}

/// `time` in frames at `rate`, exactly; a negative or non-finite time is none.
fn frames(time: Seconds, rate: SampleRate) -> f64 {
    let frames = time.get() * rate.get();
    if frames.is_finite() {
        frames.max(0.0)
    } else {
        0.0
    }
}

/// The stages of an envelope at a sample rate, in frames and `f64` levels: the one place the
/// timing and the curves are decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeDesign {
    rate: SampleRate,
    delay: f64,
    attack: f64,
    hold: f64,
    decay: Fall,
    release: Fall,
    sustain: f64,
    pace: EnvelopePace,
}

impl EnvelopeDesign {
    /// The stages at `rate`.
    #[must_use]
    pub fn new(stages: &EnvelopeStages, rate: SampleRate) -> Self {
        Self {
            rate,
            delay: frames(stages.delay, rate),
            attack: frames(stages.attack, rate),
            hold: frames(stages.hold, rate),
            decay: Fall::new(stages.decay, stages.decay_curve, rate),
            release: Fall::new(stages.release, stages.release_curve, rate),
            sustain: stages.sustain.get().clamp(0.0, 1.0),
            pace: stages.pace,
        }
    }

    /// This design with its release no longer than `time`: a release already shorter is
    /// kept, so it never slows a fall.
    #[must_use]
    pub fn release_within(mut self, time: Seconds) -> Self {
        self.release.frames = self.release.frames.min(frames(time, self.rate));
        self
    }

    /// The leg `stage` runs when entered at `level` (a level within `0..=1`): a stage that
    /// lasts no frame is passed over, so the leg returned is the first that lasts one. The
    /// level is read by the attack and the release, which start from it, and by no other
    /// stage.
    #[must_use]
    pub fn enter(&self, mut stage: EnvelopeStage, level: Amplitude) -> EnvelopeLeg {
        let level = level.get().clamp(0.0, 1.0);
        loop {
            let leg = match stage {
                EnvelopeStage::Delay => EnvelopeLeg::hold(stage, 0.0, self.delay),
                EnvelopeStage::Attack => self.attack_leg(level),
                EnvelopeStage::Hold => EnvelopeLeg::hold(stage, 1.0, self.hold),
                EnvelopeStage::Decay => self.decay_leg(),
                EnvelopeStage::Sustain => EnvelopeLeg::hold(stage, self.sustain, f64::INFINITY),
                EnvelopeStage::Release => self.release_leg(level),
                EnvelopeStage::Idle => EnvelopeLeg::hold(stage, 0.0, f64::INFINITY),
            };
            if leg.frames > 0.0 {
                return leg;
            }
            stage = stage.next();
        }
    }

    fn attack_leg(&self, from: f64) -> EnvelopeLeg {
        let span = 1.0 - from;
        if span <= 0.0 || self.attack < END_TOLERANCE {
            return EnvelopeLeg::hold(EnvelopeStage::Attack, 1.0, 0.0);
        }
        EnvelopeLeg {
            stage: EnvelopeStage::Attack,
            from,
            to: 1.0,
            law: Law::Linear {
                per_frame: 1.0 / self.attack,
            },
            frames: span * self.attack,
        }
    }

    fn decay_leg(&self) -> EnvelopeLeg {
        let to = self.sustain.max(self.decay.floor());
        let leg = self.decay.leg(EnvelopeStage::Decay, 1.0, to, self.pace);
        if leg.frames == 0.0 {
            return EnvelopeLeg::hold(EnvelopeStage::Decay, self.sustain, 0.0);
        }
        leg
    }

    fn release_leg(&self, from: f64) -> EnvelopeLeg {
        let to = self.release.floor();
        if from <= to {
            return EnvelopeLeg::hold(EnvelopeStage::Release, 0.0, 0.0);
        }
        self.release
            .leg(EnvelopeStage::Release, from, to, self.pace)
    }
}

/// An envelope read a chunk at a time: the stage machine over an [`EnvelopeDesign`].
///
/// The level is the closed form of the frames elapsed in the current leg, so any chunking of
/// the same frames reads the same levels.
#[derive(Clone, Debug)]
pub struct Envelope {
    design: EnvelopeDesign,
    leg: EnvelopeLeg,
    elapsed: f64,
}

impl Envelope {
    /// An idle envelope on `design`.
    #[must_use]
    pub fn new(design: EnvelopeDesign) -> Self {
        Self {
            design,
            leg: design.enter(EnvelopeStage::Idle, Amplitude::SILENT),
            elapsed: 0.0,
        }
    }

    /// Begins at silence in the delay stage, on `design`.
    pub fn start(&mut self, design: EnvelopeDesign) {
        self.design = design;
        self.enter(EnvelopeStage::Delay, Amplitude::SILENT);
    }

    /// Attacks from the current level.
    pub fn retrigger(&mut self) {
        self.enter(EnvelopeStage::Attack, self.level());
    }

    /// Releases from the current level. An idle envelope stays idle.
    pub fn release(&mut self) {
        self.enter(EnvelopeStage::Release, self.level());
    }

    /// Releases within `time` at the longest: the release to come, or the one running, is
    /// made no longer than `time`. A release running continues from its level and never
    /// slows.
    pub fn release_within(&mut self, time: Seconds) {
        self.design = self.design.release_within(time);
        if self.leg.stage() == EnvelopeStage::Release {
            self.enter(EnvelopeStage::Release, self.level());
        }
    }

    fn enter(&mut self, stage: EnvelopeStage, level: Amplitude) {
        self.leg = self.design.enter(stage, level);
        self.elapsed = 0.0;
    }

    /// Moves on by `frames`, through every stage whose leg ends in them.
    pub fn advance(&mut self, frames: Samples) {
        self.elapsed += frames.get() as f64;
        while self.leg.ended_by(self.elapsed) {
            self.elapsed = (self.elapsed - self.leg.frames).max(0.0);
            self.leg = self.design.enter(self.leg.stage.next(), self.leg.to());
        }
    }

    /// The level now.
    #[must_use]
    pub fn level(&self) -> Amplitude {
        Amplitude::new(self.leg.level_after(self.elapsed))
    }

    /// The stage now.
    #[must_use]
    pub const fn stage(&self) -> EnvelopeStage {
        self.leg.stage
    }

    /// Whether the release has ended.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self.leg.stage, EnvelopeStage::Idle)
    }
}

#[cfg(test)]
mod tests;
