//! The phaser's pieces: a swept all-pass chain of any width, [`PhaserChain`], and what it
//! is built to ([`Stages`], [`SweepRange`]) and told ([`PhaserControls`]).
//!
//! A chain of `stages` first-order all-passes per channel, whose centre frequency an LFO
//! moves across a [`SweepRange`], with the chain's output fed back into its input. The
//! output is the chain's, the **wet** signal: an all-pass chain alone barely changes the
//! magnitude, and the cancellation notches appear where it is blended against the dry
//! input, which is the graph's mix. It holds no delay line, which is why a phaser's
//! notches are fewer and unevenly spaced where a flanger's are a comb.
//!
//! The chain is the kernel: it runs a block through complements that are
//! [`Ramp`]s, one per channel, and a feedback ramp. The LFO and the solve of the
//! complements are the caller's: it solves the corners of a segment (the coefficient at
//! the segment's start and at its end, [`COEFF_INTERVAL`](crate::COEFF_INTERVAL) frames on)
//! and retargets each channel's ramp across it, so the cut of the blocks does not move the
//! render.

use core::ops::RangeInclusive;

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelTopology, MAX_CHANNELS};
use tutti_buffers::{Float, InputOutput};
use tutti_units::{Bipolar, Feedback, Hz};

use crate::{Lerp, Ramp};

/// The most channels a phaser has: a bus's.
pub const MAX_PHASER_CHANNELS: usize = MAX_CHANNELS;

tutti_units::ranged! {
    /// How many first-order all-pass sections a phaser's chain has, per channel: 2 to 12.
    ///
    /// Stages come in pairs, each pair making one notch: 4 gives the classic two-notch phaser
    /// and higher counts thicken the effect.
    ///
    /// A range-checked value: [`new`](Self::new) refuses a
    /// count the chain is not defined for and never clamps.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    Stages(usize), StagesError, "within 2..=12 stages",
    |raw| raw >= 2 && raw <= 12
}

impl Stages {
    /// The fewest stages: 2, one notch.
    pub const MIN: usize = 2;
    /// The most stages: 12, six notches.
    pub const MAX: usize = 12;
}

/// The band the all-pass centre sweeps across: its trough `min` and its peak `max`, in
/// [`Hz`].
///
/// A range-checked value: [`TryFrom`] a range of [`Hz`] refuses a bound that is not finite, a `min`
/// under [`FLOOR`](Self::FLOOR) (20 Hz) and a `min` above the `max`, and never clamps. What
/// depends on the **rate** stays the member's: the sweep's top is held at 0.90 of Nyquist
/// there, a wider margin than the filters take because several all-pass stages compound
/// their phase error near the limit, and the authored top is kept, so a later, higher rate
/// restores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepRange {
    min: Hz,
    max: Hz,
}

/// A refused [`SweepRange`]: the raws it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SweepRangeError {
    /// A bound is not finite.
    #[error("the sweep range {min:?}..{max:?} is not finite")]
    NotFinite {
        /// The refused bottom.
        min: Hz,
        /// The refused top.
        max: Hz,
    },
    /// The bottom is under [`SweepRange::FLOOR`].
    #[error("the sweep bottom {0:?} is under the {floor:?} floor", floor = SweepRange::FLOOR)]
    BelowFloor(Hz),
    /// The bottom is above the top.
    #[error("the sweep bottom {min:?} is above its top {max:?}")]
    Order {
        /// The refused bottom.
        min: Hz,
        /// The refused top.
        max: Hz,
    },
}

impl SweepRange {
    /// The lowest bottom a sweep has: 20 Hz.
    pub const FLOOR: Hz = Hz::new(20.0);

    /// The default sweep: 200 to 4000 Hz.
    pub const DEFAULT: Self = Self::of(Hz::new(200.0), Hz::new(4_000.0));

    const fn checked(min: Hz, max: Hz) -> Result<Self, SweepRangeError> {
        let (lo, hi) = (min.get(), max.get());
        if !(lo.is_finite() && hi.is_finite()) {
            return Err(SweepRangeError::NotFinite { min, max });
        }
        if lo < Self::FLOOR.get() {
            return Err(SweepRangeError::BelowFloor(min));
        }
        if lo > hi {
            return Err(SweepRangeError::Order { min, max });
        }
        Ok(Self { min, max })
    }

    /// A sweep for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If the range is refused ([`TryFrom`]).
    #[must_use]
    pub const fn of(min: Hz, max: Hz) -> Self {
        match Self::checked(min, max) {
            Ok(r) => r,
            Err(_) => panic!("a sweep range is finite, from 20 Hz up and in order"),
        }
    }

    /// The bottom: where the all-pass centre sits at the LFO's trough.
    #[must_use]
    pub const fn min(&self) -> Hz {
        self.min
    }

    /// The authored top: where the centre sits at the LFO's peak, before the rate holds it
    /// at 0.90 of Nyquist.
    #[must_use]
    pub const fn max(&self) -> Hz {
        self.max
    }
}

/// A sweep from the start to the end of `range`, in [`Hz`].
impl TryFrom<RangeInclusive<Hz>> for SweepRange {
    type Error = SweepRangeError;

    /// # Errors
    ///
    /// [`SweepRangeError::NotFinite`], [`BelowFloor`](SweepRangeError::BelowFloor) and
    /// [`Order`](SweepRangeError::Order).
    fn try_from(range: RangeInclusive<Hz>) -> Result<Self, SweepRangeError> {
        Self::checked(*range.start(), *range.end())
    }
}

impl Default for SweepRange {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls of a phaser.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserControls {
    /// The LFO's rate: how fast the notches sweep. Phaser rates are slow, typically 0.1 to
    /// 2 Hz. **Floored at 0.01 Hz**, a control signal's value and a cell's alike ([`MIN_RATE`](crate::MIN_RATE)): a
    /// negative rate does not run the sweep backwards. A rate that is not finite is not
    /// taken: the node holds the last finite one (the default's before any).
    pub rate: Hz,
    /// How much of the [`SweepRange`] the sweep covers: **unitless**, `0..=1` (a chorus's
    /// depth is seconds of delay). 1 sweeps the whole range, 0 parks the notches at its
    /// bottom. A negative depth sweeps down from the bottom (held above 1 Hz). A
    /// [`Bipolar`] is in `-1..=1` and finite by its type.
    pub depth: Bipolar,
    /// How much of the chain's output recirculates into its input: deepens and sharpens the
    /// notches. Clamped into `0..=`[`Feedback::MAX_STABLE`]; one that is not finite is not
    /// taken: the node holds the last finite one (the default's before any).
    pub feedback: Feedback,
}

impl PhaserControls {
    /// 0.3 Hz, half depth, half feedback: where a phaser starts, and what a control holds
    /// until it has been given a finite value.
    pub const DEFAULT: Self = Self {
        rate: Hz::new(0.3),
        depth: Bipolar::of(0.5),
        feedback: Feedback::new(0.5),
    };
}

impl Default for PhaserControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The state of an all-pass chain in the float `F`: `stages` first-order all-pass sections
/// per channel and each channel's last output, which feeds back into its input.
///
/// A section with the coefficient `c` (`y = c (x - y1) + x1`, `c` within `2 t` of -1 at a low
/// centre) runs on its complement `a = 1 + c` ([`allpass_complement`](crate::allpass_complement)):
/// `y = y1 + ((x1 - x) + a (x - y1))`, whose small terms add first. The ramps carry `a`.
///
/// The width is fixed when it is built. [`process`](Self::process) runs a block through it
/// while each channel's coefficient and the feedback ramp toward their targets.
#[derive(Clone, Debug, PartialEq)]
pub struct PhaserChain<F: Float> {
    stages: Stages,
    width: usize,
    /// Stage input history, stage-major: `x1[s * width + c]`.
    x1: Box<[F]>,
    /// Stage output history, stage-major: `y1[s * width + c]`.
    y1: Box<[F]>,
    /// Each channel's last chain output, fed back into its input.
    last: Box<[F]>,
}

impl<F: Float + Lerp> PhaserChain<F> {
    /// A silent chain of `stages` sections on the channels of `bus`. A bus has at most
    /// [`MAX_PHASER_CHANNELS`], so no width is refused: the type holds the bound. Allocates:
    /// build it on the control side.
    #[must_use]
    pub fn new(stages: Stages, bus: ChannelTopology) -> Self {
        let width = usize::from(bus.width().count());
        Self {
            stages,
            width,
            x1: vec![F::ZERO; stages.get() * width].into_boxed_slice(),
            y1: vec![F::ZERO; stages.get() * width].into_boxed_slice(),
            last: vec![F::ZERO; width].into_boxed_slice(),
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.x1.fill(F::ZERO);
        self.y1.fill(F::ZERO);
        self.last.fill(F::ZERO);
    }

    /// Runs `input` into `output`, frame `i` at `complements[c].at(taken + 1 + i)` for
    /// channel `c` and `feedback.at(taken + 1 + i)`, then takes the block's steps from the
    /// ramps.
    ///
    /// Each channel's input plus its last output scaled by the feedback goes through the
    /// stages in turn, a frame at a time and each stage across every channel, and the chain's
    /// output is the frame's and the next frame's feedback. Channel `c` of `input` feeds
    /// channel `c` of `output`. No allocation.
    ///
    /// The loop is `g z^-1 A(z)` with `A` the all-pass chain, `|A| = 1` on the circle, so it
    /// is stable for any feedback `g` below 1 and rings for about `(1 + N tau) / (1 - g)`
    /// frames at the slowest pole, `tau` a stage's greatest group delay, `(1 + |c|) / (1 - |c|)`
    /// for a coefficient `c`. The feedback ramp's values must stay below
    /// 1 ([`Feedback::MAX_STABLE`] is the bound a caller holds them to).
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the chain's width in channels.
    ///
    /// # Panics
    ///
    /// If `complements` has fewer ramps than the chain has channels.
    pub fn process(
        &mut self,
        complements: &mut [Ramp<F>],
        feedback: &mut Ramp<F>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let run = self.width;
        expect_widths(block, run, run)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let xs: [&[F]; MAX_PHASER_CHANNELS] = core::array::from_fn(|c| {
            if c < run {
                &input.plane(c).samples()[..frames]
            } else {
                &[]
            }
        });
        let mut outs = output.iter_mut();
        let ys: [&mut [F]; MAX_PHASER_CHANNELS] = core::array::from_fn(|c| {
            let plane = if c < run { outs.next() } else { None };
            plane.map_or_else(Default::default, |p| &mut p.into_samples_mut()[..frames])
        });
        let complements = &mut complements[..self.width];
        // The step each ramp's first frame reads.
        let first: [u32; MAX_PHASER_CHANNELS] = core::array::from_fn(|c| {
            complements
                .get(c)
                .map_or(0, |g| g.steps_taken().saturating_add(1))
        });
        let fb_first = feedback.steps_taken().saturating_add(1);
        let mut complement = [F::ZERO; MAX_PHASER_CHANNELS];
        let mut frame = [F::ZERO; MAX_PHASER_CHANNELS];
        let mut step = 0u32;
        for i in 0..frames {
            let fb = feedback.at(fb_first.saturating_add(step));
            for c in 0..run {
                complement[c] = complements[c].at(first[c].saturating_add(step));
                frame[c] = xs[c][i] + self.last[c] * fb;
            }
            self.chain(&complement[..run], &mut frame[..run]);
            for c in 0..run {
                self.last[c] = frame[c];
                ys[c][i] = frame[c];
            }
            step = step.saturating_add(1);
        }
        let taken = u32::try_from(frames).unwrap_or(u32::MAX);
        for ramp in complements {
            ramp.advance(taken);
        }
        feedback.advance(taken);
        Ok(())
    }

    /// Runs `frame` (one sample per channel) through the stages in turn, each stage across
    /// every channel in one inner loop, `coeffs` one coefficient per channel. Each stage's
    /// output is the next stage's input, and the last is left in `frame`.
    #[inline(always)]
    fn chain(&mut self, coeffs: &[F], frame: &mut [F]) {
        let w = self.width;
        for s in 0..self.stages.get() {
            let x1 = &mut self.x1[s * w..(s + 1) * w];
            let y1 = &mut self.y1[s * w..(s + 1) * w];
            for c in 0..coeffs.len() {
                let input = frame[c];
                // `c (x - y1) + x1` with `c = a - 1`, the stage held as its complement
                // `a`: the small terms add first, so a low centre (`a` near 0) keeps its
                // precision.
                let out = y1[c] + ((x1[c] - input) + coeffs[c] * (input - y1[c]));
                x1[c] = input;
                y1[c] = out;
                frame[c] = out;
            }
        }
    }
}

#[cfg(test)]
mod tests;
