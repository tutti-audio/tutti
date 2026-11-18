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
//! ([`CompressorLanes`]): it is a level to compare against, not a coefficient. The makeup
//! multiplies the output directly, so a step in it is a click: it is a ramp, a control moving
//! to a target, and glides over [`GAIN_GLIDE`] as a [`Glide`] read on the stream. The knee,
//! the ratio and the times only move what the follower is smoothing toward, and the
//! follower is the smoother: they land whole.
//!
//! The compressor has no latency (the detector is on the signal and the key, never ahead
//! of them) and no tail (the gain only scales, so a silent signal is a silent output
//! whatever the follower is doing).

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_types::ChannelLayout;
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds};

use super::curves::{db_gain, gain_reduction, level_db};
use super::key_filter::{KeyFilter, KeyStage};
use super::shared::{CHUNK, GAIN_GLIDE, MAX_KNEE, floored, key_levels};
use crate::finite::finite_or;
use crate::follower::Follower;
use crate::{Glide, Lanes, Lerp, Steps};

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
    /// How fast the reduction falls once the level drops. Below 0 reads as 0.
    pub release: Seconds,
    /// Gain added after the reduction, in dB.
    pub makeup: Db,
    /// The cutoff of the key filter: read only by a bank built with
    /// [`key_filter`](CompressorBank::key_filter). Held in the SVF's range.
    pub key_cutoff: Hz,
    /// The Q of the key filter, read as `key_cutoff` is.
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
const MAX_MAKEUP: Db = Db(400.0);

/// A compressor at one set of controls and one rate: the controls made runnable.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CompressorMember {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    attack: Seconds,
    release: Seconds,
    makeup: Db,
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
            threshold: Db(finite_or(c.threshold.get(), d.threshold.get())),
            knee: Db(finite_or(c.knee.get(), d.knee.get()).clamp(0.0, MAX_KNEE.get())),
            ratio: CompressionRatio::new_clamped(finite_or(c.ratio.get(), d.ratio.get())),
            attack: c.attack,
            release: c.release,
            makeup: Db(finite_or(c.makeup.get(), d.makeup.get())
                .clamp(-MAX_MAKEUP.get(), MAX_MAKEUP.get())),
            key_cutoff: Hz(finite_or(c.key_cutoff.get(), d.key_cutoff.get())),
            key_q: Q(finite_or(c.key_q.get(), d.key_q.get())),
            rate,
        }
    }
}

/// The figures the detector runs on: the controls it follows, with the times as one-pole
/// coefficients at the rate in the working precision `F`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Detector<F> {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    /// The follower on the reduction: the attack while it rises, the release while it falls.
    ballistics: Follower<F>,
}

impl<F: Float> Detector<F> {
    /// The figures member `m` asks for: its controls widened to `F`, and the times as
    /// coefficients at its rate.
    fn of(m: &CompressorMember) -> Self {
        Self {
            threshold: m.threshold,
            knee: m.knee,
            ratio: m.ratio,
            ballistics: Follower::of(floored(m.attack, m.release), m.rate),
        }
    }

    /// The figures of a compressor that does nothing, for a target to start from.
    const NEUTRAL: Self = Self {
        threshold: Db::UNITY,
        knee: Db::UNITY,
        ratio: CompressionRatio::UNITY,
        ballistics: Follower::NEUTRAL,
    };

    /// One frame of the compressor: the key's `level` (an amplitude) in dB, the static
    /// curve at `threshold`, the one-pole follower `reduction` on it (the attack
    /// coefficient while it rises, the release while it falls) and the linear gain
    /// `-reduction + makeup`.
    #[inline]
    fn frame(&self, reduction: &mut F, level: F, threshold: F, makeup: F) -> F {
        // The knee and the ratio cross into `F` here.
        let target = gain_reduction(
            level_db(level),
            threshold,
            F::from_unit(self.ratio),
            F::from_f64(self.knee.get()),
        );
        *reduction = self.ballistics.follow_raw(*reduction, target);
        db_gain(makeup - *reduction)
    }
}

/// The per-frame lane a compressor reads beside its audio, in the audio's sample format,
/// `None` where nothing drives it. A lane is at least as long as the frames a call renders;
/// a frame past its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompressorLanes<'a, S: Float = f32> {
    /// The threshold in dB. A frame that is not finite is the aimed threshold.
    pub threshold: Option<Mono<'a, S>>,
}

/// A feed-forward compressor of any width: one gain from the loudest key channel, applied
/// to every channel, with the follower that smooths it and the makeup that glides.
///
/// Built on the control side for one rate; the state is `F`, the graph's float, and a view of
/// another sample format is converted at the edges. Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct CompressorBank<F: Float + Lanes> {
    detector: Detector<F>,
    /// The makeup in dB, gliding over [`GAIN_GLIDE`].
    makeup: Glide<F>,
    /// The steps a makeup glide takes: [`GAIN_GLIDE`] at the rate.
    steps: u32,
    rate: SampleRate,
    /// The smoothed reduction, in dB and positive: a per-sample level in `F`; becomes `Db` once
    /// that unit stores `f64`.
    reduction: F,
    /// The deepest reduction since the last [`take_deepest`](Self::take_deepest), in dB
    /// and positive: a per-sample level in `F`, as `reduction` is.
    deepest: F,
    /// The filter on the key, if the bank has one.
    key: Option<KeyStage<F>>,
}

impl<F: Float + Lanes + Lerp> CompressorBank<F> {
    /// A compressor that does nothing at `rate`, to be aimed.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates rather
        // than panic, since a prepare must not.
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            detector: Detector::NEUTRAL,
            makeup: Glide::landed(F::ZERO),
            steps,
            rate,
            reduction: F::ZERO,
            deepest: F::ZERO,
            key: None,
        }
    }

    /// This bank with the detector hearing its key through `filter`, over `width` key
    /// channels at the cutoff and Q [`aim`](Self::aim) gives it. [`KeyFilter::Off`] is no
    /// filter; key channels past `width` are not heard. Allocates: build it on the control
    /// side.
    #[must_use]
    pub fn key_filter(mut self, filter: KeyFilter, width: ChannelLayout) -> Self {
        self.key = filter
            .response()
            .map(|ty| KeyStage::new(ty, width, self.rate));
        self
    }

    /// Back to silence: no reduction, nothing metered, and the makeup landed on its target.
    /// No allocation.
    pub fn reset(&mut self) {
        self.reduction = F::ZERO;
        self.deepest = F::ZERO;
        self.makeup.settle();
        if let Some(key) = &mut self.key {
            key.reset();
        }
    }

    /// Lands the makeup on its target now.
    pub fn settle(&mut self) {
        self.makeup.settle();
        if let Some(key) = &mut self.key {
            key.settle();
        }
    }

    /// Aims at `controls`: the detector's figures land whole (the follower smooths them) and
    /// the makeup glides over [`GAIN_GLIDE`]. A non-finite control is the default's, a knee
    /// and the times at 0 (through `floored`) and a ratio at 1. The key filter, if any, glides to the
    /// cutoff and Q over one grid period ([`COEFF_INTERVAL`](crate::COEFF_INTERVAL)). No
    /// allocation.
    pub fn aim(&mut self, controls: &CompressorControls) {
        let member = CompressorMember::designed(controls, self.rate);
        self.detector = Detector::of(&member);
        if let Some(key) = &mut self.key {
            key.aim(member.key_cutoff, member.key_q);
        }
        self.makeup
            .aim_by(F::from_f64(member.makeup.get()), Steps::Fixed(self.steps));
    }

    /// The deepest smoothed reduction any frame reached since the last call (or since
    /// [`reset`](Self::reset)), in dB and positive; [`Db::UNITY`] is none. A **measurement**,
    /// for a reduction meter. No allocation.
    pub fn take_deepest(&mut self) -> Db {
        Db(core::mem::replace(&mut self.deepest, F::ZERO).to_f64())
    }

    /// Runs `input` into `output`, scaled by one gain from the loudest channel of `key`.
    ///
    /// `key` is the signal the detector listens to: the side-chain bus, or `input` itself
    /// for a compressor on its own signal, through the key filter if the bank has one (the
    /// signal it gains is never filtered). Channel `c` of `input` feeds channel `c` of
    /// `output`; a channel past the narrower of the two views is not touched, and the frames
    /// rendered are the fewest of the three views'. A NaN key sample reads as quiet and an
    /// infinite one as the largest finite level. Frame `i` reads the makeup at the glide's
    /// step `taken + 1 + i`, then the block's steps are taken. No allocation.
    pub fn process<S: Float>(
        &mut self,
        lanes: CompressorLanes<'_, S>,
        key: Planes<'_, S>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input
            .frames()
            .get()
            .min(output.frames().get())
            .min(key.frames().get());
        let width = input.channels().min(output.channels());
        let d = self.detector;
        let aimed = F::from_f64(d.threshold.get());
        let blended = self.makeup.blended(frames);
        let taken = self.makeup.taken();
        let mut gains = [F::ZERO; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];
            // The key's level: the loudest channel at each frame, after the key filter.
            match &mut self.key {
                Some(stage) => stage.levels(&key, start..end, gains),
                None => key_levels(&key, start..end, gains),
            }
            for (k, g) in gains.iter_mut().enumerate() {
                let i = start + k;
                let threshold = lanes
                    .threshold
                    .and_then(|l| l.samples().get(i))
                    .map(|x| x.convert_to::<F>())
                    .filter(|t| t.is_finite())
                    .unwrap_or(aimed);
                let makeup = if i < blended {
                    self.makeup
                        .at(taken.saturating_add(1).saturating_add(i as u32))
                } else {
                    self.makeup.target()
                };
                *g = d.frame(&mut self.reduction, *g, threshold, makeup);
                self.deepest = self.deepest.max(self.reduction);
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, x), &g) in ys.iter_mut().zip(xs).zip(gains.iter()) {
                    *y = (x.convert_to::<F>() * g).convert_to::<S>();
                }
            }
            start = end;
        }
        self.makeup
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
