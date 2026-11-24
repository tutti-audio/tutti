//! [`CompressorBank`]: the feed-forward compressor over buffers views.
//!
//! A compressor reads a **key**: the side-chain as wide as the signal (an unwired one is
//! silence and compresses nothing) or the signal itself, as the caller hands the bank its
//! [`KeySource`](super::KeySource). It finds its level (the loudest key channel at
//! each frame), turns the level over the threshold into a reduction by the static
//! curve, smooths that reduction with an attack and a release, and multiplies **every**
//! channel by one gain: the reduction's, plus the makeup. The gain is linked, so the
//! stereo image does not move when one side is louder.
//!
//! The threshold is the one control read **per frame** when the graph drives it
//! ([`CompressorSignals`]): it is a level to compare against, not a coefficient. The makeup
//! multiplies the output directly, so a step in it is a click: it is a ramp, a control moving
//! to a target, and ramps over [`GAIN_RAMP`] as a [`Ramp`] read on the stream. The knee,
//! the ratio and the times only move what the follower is smoothing toward, and the
//! follower is the smoother: they land whole.
//!
//! The compressor has no latency (the detector is on the signal and the key, never ahead
//! of them) and no tail (the gain only scales, so a silent signal is a silent output
//! whatever the follower is doing).

use crate::BlockMismatch;
use crate::block::{expect_key_frames, expect_same_width};
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Float, InputOutput, Mono, Planes};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds};

use super::curves::{db_gain, gain_reduction, level_db};
use super::gain_pass::{CHUNK, GAIN_RAMP, MAX_KNEE, floored};
use super::key_filter::{KeyFilter, KeyPath};
use crate::follower::{Follower, Smoothed};
use crate::{Lerp, Ramp, SimdLanes, Steps};

/// The controls of a [`CompressorBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorControls {
    /// The level above which reduction begins, in dB.
    pub threshold: Db,
    /// The width of the soft knee in dB, centred on the threshold; 0 is a hard knee.
    /// Below 0 reads as 0, and above 400 dB as 400: the curve's knee term is at most
    /// `knee / 2` dB, so a wider knee changes nothing a finite follower can use, and a knee
    /// of 1e20 or 3e38 would overflow its square.
    pub knee: Db,
    /// dB in per dB out above the threshold. Below 1 reads as 1: a compressor does not
    /// expand.
    pub ratio: CompressionRatio,
    /// How fast the reduction rises toward a louder level: the time constant of a
    /// one-pole smoother. Below 0 reads as 0 (instant).
    pub attack: Seconds,
    /// How fast the reduction falls once the level drops. Below 0 reads as 0. A release of any
    /// length keeps its time constant at every float, `f32` included: 10 s at 192 kHz holds it to
    /// a few millionths of a dB, and 300 s still falls.
    pub release: Seconds,
    /// Gain added after the reduction, in dB.
    pub makeup: Db,
    /// The filter the detector hears the key through. A running bank switches: from
    /// [`Off`](KeyFilter::Off) the filter starts from silence at the cutoff and Q below, and
    /// between its two responses it keeps its state.
    pub key_filter: KeyFilter,
    /// The cutoff of the key filter, held in the SVF's range. Not in use while the filter is
    /// off, and kept: a switch in lands on it.
    pub key_cutoff: Hz,
    /// The Q of the key filter, kept as `key_cutoff` is.
    pub key_q: Q,
}

impl CompressorControls {
    /// A compressor that does nothing: threshold at 0 dB, a hard knee, no ratio, no
    /// times and no makeup. What a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db::UNITY,
        knee: Db::UNITY,
        ratio: CompressionRatio::UNITY,
        attack: Seconds::ZERO,
        release: Seconds::ZERO,
        makeup: Db::UNITY,
        key_filter: KeyFilter::Off,
        key_cutoff: KeyFilter::DEFAULT_CUTOFF,
        key_q: KeyFilter::DEFAULT_Q,
    };
}

impl Default for CompressorControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The most makeup, in dB, a compressor applies: 10^(400/20) is 1e20, so the gain of a
/// makeup and a reduction stays finite whatever either is.
const MAX_MAKEUP: Db = Db::new(400.0);

/// A compressor at one set of controls and one rate: the controls made runnable.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CompressorMember {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    attack: Seconds,
    release: Seconds,
    makeup: Db,
    key_filter: KeyFilter,
    key_cutoff: Hz,
    key_q: Q,
    rate: SampleRate,
}

impl CompressorMember {
    /// The member for `c` at `rate`: a non-finite control is the default's, a knee is
    /// floored at 0 and a ratio at 1; the times are floored at 0 (an infinite one holds).
    fn designed(c: &CompressorControls, rate: SampleRate) -> Self {
        let d = CompressorControls::DEFAULT;
        Self {
            threshold: c.threshold.finite_or(d.threshold),
            knee: c.knee.finite_or(d.knee).clamp(Db::UNITY, MAX_KNEE),
            ratio: CompressionRatio::new_clamped(c.ratio.finite_or(d.ratio).get()),
            attack: c.attack,
            release: c.release,
            makeup: c.makeup.finite_or(d.makeup).clamp(-MAX_MAKEUP, MAX_MAKEUP),
            key_filter: c.key_filter,
            key_cutoff: c.key_cutoff.finite_or(d.key_cutoff),
            key_q: c.key_q.finite_or(d.key_q),
            rate,
        }
    }
}

/// The figures the detector runs on: the controls it follows, with the times as one-pole
/// poles at the rate in the bank's float `F`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CompressorStage<F> {
    threshold: Db,
    /// The knee in dB, crossed once.
    knee: F,
    /// The ratio, crossed once.
    ratio: F,
    /// The follower on the reduction: the attack while it rises, the release while it falls.
    ballistics: Follower<F>,
}

impl<F: Float> CompressorStage<F> {
    /// The figures member `m` asks for: its controls and the times as poles at its rate.
    fn of(m: &CompressorMember) -> Self {
        Self {
            threshold: m.threshold,
            knee: F::from_unit(m.knee),
            ratio: F::from_unit(m.ratio),
            ballistics: Follower::of(floored(m.attack, m.release), m.rate),
        }
    }

    /// The figures of a compressor that does nothing, for a target to start from.
    const NEUTRAL: Self = Self {
        threshold: Db::UNITY,
        knee: F::ZERO,
        ratio: F::ONE,
        ballistics: Follower::NEUTRAL,
    };

    /// One frame of the compressor: the key's `level` (an amplitude) in dB, the static
    /// curve at `threshold`, the one-pole follower `reduction` on it (the attack
    /// coefficient while it rises, the release while it falls) and the linear gain
    /// `-reduction + makeup`.
    #[inline]
    fn frame(&self, reduction: &mut Smoothed<F>, level: F, threshold: F, makeup: F) -> F {
        let target = gain_reduction(level_db(level), threshold, self.ratio, self.knee);
        let reduced = self.ballistics.follow_raw(reduction, target);
        db_gain(makeup - reduced)
    }
}

/// The per-frame control signal a compressor reads beside its audio, in the bank's float, `None` where
/// nothing drives it. A control signal is at least as long as the frames a call renders; a frame past
/// its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompressorSignals<'a, F: Float> {
    /// The threshold in dB. A frame that is not finite is the set threshold.
    pub threshold: Option<Mono<'a, F>>,
}

/// A feed-forward compressor of any width: one gain from the loudest key channel, applied
/// to every channel, with the follower that smooths it and the makeup that ramps.
///
/// Built on the control side for one rate; the state, the views and the control signal are `F`, the
/// graph's float. Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct CompressorBank<F: Float + SimdLanes> {
    detector: CompressorStage<F>,
    /// The makeup in dB, ramping over [`GAIN_RAMP`].
    makeup: Ramp<F>,
    /// The steps a makeup ramp takes: [`GAIN_RAMP`] at the rate.
    steps: u32,
    rate: SampleRate,
    /// The smoothed reduction, in dB and positive: a per-sample level in `F`.
    reduction: Smoothed<F>,
    /// The deepest reduction since the last [`take_deepest`](Self::take_deepest), in dB
    /// and positive: a per-sample level in `F`, as `reduction` is.
    deepest: F,
    /// The path the key takes to the detector, through its filter while the filter is on.
    key: KeyPath<F>,
}

impl<F: Float + SimdLanes + Lerp> CompressorBank<F> {
    /// A compressor that does nothing at `rate`, to be set, whose key filter covers `width`
    /// key channels (key channels past `width` are not heard through it). Allocates: build it
    /// on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout, rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            detector: CompressorStage::NEUTRAL,
            makeup: Ramp::new(F::ZERO),
            steps,
            rate,
            reduction: Smoothed::ZERO,
            deepest: F::ZERO,
            key: KeyPath::new(width, rate),
        }
    }

    /// The bank [`new`](Self::new) makes, in place: it does nothing, the key filter is off at its
    /// defaults, and nothing is metered. No allocation.
    pub(super) fn clear(&mut self) {
        self.detector = CompressorStage::NEUTRAL;
        self.makeup = Ramp::new(F::ZERO);
        self.reduction = Smoothed::ZERO;
        self.deepest = F::ZERO;
        self.key.clear();
    }

    /// Back to silence: no reduction, nothing metered, and the makeup at rest on its target.
    /// No allocation.
    pub fn reset(&mut self) {
        self.reduction = Smoothed::ZERO;
        self.deepest = F::ZERO;
        self.makeup.jump_to_target();
        self.key.reset();
    }

    /// Lands the makeup on its target now.
    pub fn jump_to_target(&mut self) {
        self.makeup.jump_to_target();
        self.key.jump_to_target();
    }

    /// Aims at `controls`: the detector's figures land whole (the follower smooths them) and
    /// the makeup ramps over [`GAIN_RAMP`]. A non-finite control is the default's, a knee
    /// and the times at 0 (through `floored`) and a ratio at 1. The key filter, while on, ramps to
    /// the cutoff and Q over one grid period ([`COEFF_INTERVAL`](crate::COEFF_INTERVAL)); a
    /// switch in from off starts it from silence on them at once. No allocation.
    pub fn retarget(&mut self, controls: &CompressorControls) {
        let member = CompressorMember::designed(controls, self.rate);
        self.detector = CompressorStage::of(&member);
        self.key
            .retarget(member.key_filter, member.key_cutoff, member.key_q);
        self.makeup
            .retarget_by(F::from_unit(member.makeup), Steps::Fixed(self.steps));
    }

    /// The deepest smoothed reduction any frame reached since the last call (or since
    /// [`reset`](Self::reset)), in dB and positive; [`Db::UNITY`] is none. A **measurement**,
    /// for a reduction meter. No allocation.
    pub fn take_deepest(&mut self) -> Db {
        Db::new(core::mem::replace(&mut self.deepest, F::ZERO).into())
    }

    /// Runs `input` into `output`, scaled by one gain from the loudest channel of `key`.
    ///
    /// `key` is the signal the detector listens to: the side-chain bus, or `input` itself
    /// for a compressor on its own signal, through the key filter while it is on (the
    /// signal it gains is never filtered). Channel `c` of `input` feeds channel `c` of
    /// `output`. A NaN key sample reads as quiet and an infinite one as the largest finite
    /// level. Frame `i` reads the makeup at the ramp's step `taken + 1 + i`, then the block's
    /// steps are taken. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the output has other channels than the input, or the key is not at the block's frame count.
    pub fn process(
        &mut self,
        signals: CompressorSignals<'_, F>,
        key: Planes<'_, F>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let width = expect_same_width(block)?;
        expect_key_frames(block, &key)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let d = self.detector;
        let threshold_set = F::from_unit(d.threshold);
        let blended = self.makeup.blended_frames(frames);
        let taken = self.makeup.steps_taken();
        let mut gains = [F::ZERO; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];
            // The key's level: the loudest channel at each frame, after the key filter.
            self.key.levels(&key, start..end, gains);
            for (k, g) in gains.iter_mut().enumerate() {
                let i = start + k;
                let threshold = signals
                    .threshold
                    .and_then(|l| l.samples().get(i))
                    .copied()
                    .filter(|t| t.is_finite())
                    .unwrap_or(threshold_set);
                let makeup = if i < blended {
                    self.makeup
                        .at(taken.saturating_add(1).saturating_add(i as u32))
                } else {
                    self.makeup.target()
                };
                *g = d.frame(&mut self.reduction, *g, threshold, makeup);
                self.deepest = self.deepest.max(self.reduction.value());
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, &x), &g) in ys.iter_mut().zip(xs).zip(gains.iter()) {
                    *y = x * g;
                }
            }
            start = end;
        }
        self.makeup
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
