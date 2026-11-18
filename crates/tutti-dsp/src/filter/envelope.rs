//! [`EnvelopeFilterBank`]: a state-variable filter whose cutoff follows the signal's level.
//!
//! The bank finds the level of its input (the loudest channel at each frame, held to full
//! scale 1), follows it with a one-pole attack and release to an envelope
//! `env` in `[0, 1]` (0 silent, 1 at full scale or above), and filters every channel through
//! one [`SvfBank`] whose cutoff is
//!
//! ```text
//!   cutoff = base * 2 ^ (depth_octaves * env)
//! ```
//!
//! with `depth_octaves = depth / 12` semitones. A positive depth opens the filter as the
//! level rises (an auto-wah on a band-pass, a swell on a low-pass); a negative one closes it.
//! The cutoff is held within 1 Hz and just under the Nyquist, as an
//! [`SvfCoeffs`](super::SvfCoeffs) holds it.
//!
//! # When it is designed
//!
//! The coefficients are designed once per grid period of [`COEFF_INTERVAL`] frames, counted on
//! the stream from the last [`reset`](EnvelopeFilterBank::reset) and not from a block, at the
//! envelope as it stands there, and the filter glides to the design over the period as a
//! [`SvfBank`] does ([`Glide`]). So the cutoff in force at frame `i` follows the envelope up
//! to the period's first frame, and the render does not depend on how the stream is cut into
//! blocks. Depth 0 never moves the cutoff: the bank is a fixed SVF at `base`.
//!
//! The bank has no latency. It bounds no ring-out (a resonant filter rings for as long as its Q
//! says, at the lowest cutoff the controls reach), so the node over it declares
//! [`Tail::Unknown`](tutti_units::Tail::Unknown).

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::ChannelLayout;
use tutti_units::{Db, Hz, Q, SampleRate, Samples, Seconds, Semitones};

use super::SvfBank;
use super::svf::{SvfCoefficients, SvfControls, SvfModel, SvfType};
use crate::dynamics::{floored, key_levels};
use crate::finite::finite_or;
use crate::follower::Follower;
use crate::{COEFF_INTERVAL, Glide, Lanes, Lerp};

/// The frames between designs: the grid period.
const GRID: Samples = Samples(COEFF_INTERVAL);

/// The controls of an [`EnvelopeFilterBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeFilterControls {
    /// The cutoff at an envelope of 0: a silent input.
    pub base: Hz,
    /// How far the cutoff moves at an envelope of 1, in semitones (12 to the octave), up when
    /// positive. Read within [`MAX_DEPTH`](Self::MAX_DEPTH) either way; a non-finite depth is 0.
    pub depth: Semitones,
    /// The resonance of the filter.
    pub q: Q,
    /// How fast the envelope rises toward a louder level: a one-pole time constant. Below 0
    /// reads as 0 (instant).
    pub attack: Seconds,
    /// How fast the envelope falls toward a quieter level. Below 0 reads as 0.
    pub release: Seconds,
}

impl EnvelopeFilterControls {
    /// The widest depth: 120 semitones, ten octaves either way.
    pub const MAX_DEPTH: Semitones = Semitones(120.0);

    /// A filter at the SVF's defaults (1 kHz, Butterworth) whose cutoff does not move, with a
    /// 10 ms attack and a 100 ms release.
    pub const DEFAULT: Self = Self {
        base: SvfControls::DEFAULT.freq,
        depth: Semitones::UNISON,
        q: SvfControls::DEFAULT.q,
        attack: Seconds(0.01),
        release: Seconds(0.1),
    };
}

impl Default for EnvelopeFilterControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One filter per channel at one cutoff that follows the loudest channel's level.
///
/// Built on the control side for one response and one rate; the envelope and the filter run in
/// `F`, the graph's float, and a view of another sample format is converted at the edges.
/// Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct EnvelopeFilterBank<F: Float + Lanes> {
    ty: SvfType,
    rate: SampleRate,
    follower: Follower<F>,
    /// The envelope, in `[0, 1]`: 0 until a level is heard. A per-sample level in `F`; becomes
    /// `Amplitude` once that unit stores `f64`.
    env: F,
    bank: SvfBank<F>,
    coefficients: Glide<SvfCoefficients<F>>,
    controls: EnvelopeFilterControls,
    /// The frames of the current grid period already run.
    phase: Samples,
}

impl<F: Float + Lanes + Lerp> EnvelopeFilterBank<F> {
    /// A bank `width` channels wide of the response `ty` at `rate`, at the default controls.
    /// Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout, ty: SvfType, rate: SampleRate) -> Self {
        let controls = EnvelopeFilterControls::DEFAULT;
        let mut bank = Self {
            ty,
            rate,
            follower: Follower::of(floored(controls.attack, controls.release), rate),
            env: F::ZERO,
            bank: SvfBank::new(width),
            coefficients: Glide::landed(SvfCoefficients::from(&SvfModel::new(
                SvfControls::DEFAULT,
                rate,
            )))
            .by(COEFF_INTERVAL as u32),
            controls,
            phase: Samples::ZERO,
        };
        bank.aim(&controls);
        bank.settle();
        bank
    }

    /// Back to silence: the envelope at 0, the filter cleared, the cutoff landed at the
    /// design for an envelope of 0, and the grid counted from here. No allocation.
    pub fn reset(&mut self) {
        self.env = F::ZERO;
        self.bank.reset();
        self.phase = Samples::ZERO;
        self.coefficients.aim(self.design(F::ZERO));
        self.coefficients.settle();
    }

    /// Lands the cutoff on its design for the current envelope now.
    pub fn settle(&mut self) {
        self.coefficients.aim(self.design(self.env));
        self.coefficients.settle();
    }

    /// Aims at `controls`. The envelope's times land whole, and the base, depth and Q reach the
    /// filter at the next grid period. A non-finite control is the default's, a time is floored
    /// at 0. No allocation.
    pub fn aim(&mut self, controls: &EnvelopeFilterControls) {
        let d = EnvelopeFilterControls::DEFAULT;
        let max = EnvelopeFilterControls::MAX_DEPTH.get();
        let times = floored(controls.attack, controls.release);
        self.controls = EnvelopeFilterControls {
            base: Hz(finite_or(controls.base.get(), d.base.get())),
            depth: Semitones(
                finite_or(controls.depth.get(), Semitones::UNISON.get()).clamp(-max, max),
            ),
            q: Q(finite_or(controls.q.get(), d.q.get())),
            attack: times.attack(),
            release: times.release(),
        };
        self.follower = Follower::of(times, self.rate);
    }

    /// The cutoff at the envelope `env` (0 to 1): the base moved by `depth * env`
    /// ([`Semitones::to_pitch_ratio`]), held within 1 Hz and 0.998 of the Nyquist. `Semitones`
    /// stores `f32`: the envelope narrows once, at the design.
    #[must_use]
    pub(crate) fn cutoff_at(&self, env: F) -> Hz {
        let ratio = (self.controls.depth * env.to_f32()).to_pitch_ratio();
        (self.controls.base * ratio).clamp(
            Hz(1.0),
            self.rate.nyquist_scaled(crate::filter::NYQUIST_MARGIN),
        )
    }

    /// The coefficients at the envelope `env`.
    fn design(&self, env: F) -> SvfCoefficients<F> {
        SvfCoefficients::from(&SvfModel::new(
            SvfControls {
                ty: self.ty,
                freq: self.cutoff_at(env),
                q: self.controls.q,
                gain: Db::UNITY,
            },
            self.rate,
        ))
    }

    /// Runs `input` into `output`, designing at each grid period's first frame and gliding to
    /// the design.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's width or
    /// the narrower of the two views is not touched, and the frames rendered are the fewer of
    /// the two views'. A NaN sample reads as quiet and an infinite one as full scale. No
    /// allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        let frames = input.frames().get().min(output.frames().get());
        let mut levels = [F::ZERO; GRID.get()];
        let mut start = 0;
        while start < frames {
            if self.phase == Samples::ZERO {
                let design = self.design(self.env);
                self.coefficients.aim(design);
            }
            let n = (frames - start).min(GRID.get() - self.phase.get());
            let end = start + n;
            let levels = &mut levels[..n];
            key_levels(&input, start..end, levels);
            for l in levels.iter() {
                self.env = self.follower.follow_raw(self.env, l.min(F::ONE));
            }
            let range = Samples(start)..Samples(end);
            self.bank.process(
                &mut self.coefficients,
                input.slice(range.clone()),
                output.slice_mut(range),
            );
            self.phase = (self.phase + Samples(n)) % GRID;
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;
