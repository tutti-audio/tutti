//! [`LimiterBank`]: the lookahead limiter over buffers views.
//!
//! A limiter delays its audio by the **lookahead** and runs its gain decision ahead of
//! the delayed signal, so a peak is already reduced when it arrives. The decision is
//! made on the **linked peak** (the loudest channel at each frame, a NaN ignored), in
//! four steps:
//!
//! 1. the curve turns the peak into a target gain, `min(drive, ceiling / peak)` with
//!    `drive = ceiling - threshold` (in dB a difference; [`LimiterCurve`]): the threshold
//!    drives the input, a signal under it is moved by the drive (a boost when the
//!    ceiling is above the threshold, a cut when below) and a louder one settles at the
//!    ceiling;
//! 2. the envelope takes that target **at once** when it is lower (an instant attack) and
//!    releases toward it by a one-pole otherwise, `e += c * (target - e)` with
//!    `c = 1 - exp(-1 / (release * rate))`;
//! 3. the minimum of the envelope over the window, the delayed frame and the lookahead
//!    after it, is the gain (so the gain that scales a sample is never later than its own
//!    target);
//! 4. every channel is that gain times its signal, delayed by the lookahead.
//!
//! The ceiling bounds the **sample** peak of the output to within a few ulps of
//! rounding. That is not a true-peak limit: the signal between two samples can pass the
//! ceiling (a sine at a quarter of the rate, with samples at 0.707 of its amplitude, is 3 dB
//! over it). [`PeakDetection::TruePeak`] reads the peak from the signal oversampled 4x
//! instead; see [there](PeakDetection::TruePeak) for what it bounds. The lookahead is the bank's own constant, fixed when it is built, and **is its latency**
//! ([`latency`](LimiterBank::latency)): the graph's PDC compensates for it. It has no tail,
//! which excludes latency: what the delay holds comes out within the latency (the output is
//! exactly silent that many frames after the input is), and gain on silence is silence.
//!
//! The threshold and the ceiling are the two controls read **per frame** when the graph
//! drives them ([`LimiterSignals`]); a frame that is not finite falls back to the set
//! value. Every coefficient lands whole: a ramped ceiling would let a sample past the new
//! one.
//!
//! A reset leaves the envelope **unset** (`+inf`, no history), and the first frame takes
//! its target at once, whatever it is: the first audio is limited from its first sample,
//! as a limiter that had heard silence for ever would limit it (on silence the envelope
//! settles at the drive, the target of a zero peak, which is where an unset envelope
//! meets its first quiet frame).

/// The most a control signal's level may say, in dB either way: past it a control signal value reads as this.
/// 300 dB is 1e15 as an amplitude, so a gain from a control signal's ceiling and threshold, and from
/// the peak (floored at -144 dB), stays finite in `f32`, and so at any width.
const SIGNAL_DB_LIMIT: Db = Db::new(300.0);

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_units::{Amplitude, Db, SampleRate, Samples, Seconds};

use super::curves::{FloatCurve, LimiterCurve, level_db, limiter_gain};
use super::gain_pass::{CHUNK, level};
use super::lookahead::{LookaheadRing, fold_peak};
use crate::follower::{Pole, Smoothed};
use crate::{FirError, Oversampler, Oversampling, OversamplingFactor, OversamplingPlan};

/// How a [`LimiterBank`] reads the peak it limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PeakDetection {
    /// The loudest sample of the frame: the output's samples stay within the ceiling, and
    /// what lies between them may not.
    #[default]
    Sample,
    /// The loudest point of the frame's group of four, read from the signal oversampled 4x
    /// by the half-band cascade of [`Oversampling`] (default attenuation and passband).
    /// The audio is not oversampled: the gain is applied at the base rate, to the delayed
    /// input.
    ///
    /// The gain a frame needs is the minimum of the curve's gain over its four points, which
    /// is the gain at their largest magnitude (the curve never rises with the peak). The
    /// frame's own sample is read too, so the sample peak is held as in
    /// [`Sample`](Self::Sample) for every finite input, however large.
    ///
    /// **The bound.** Points a quarter of a frame apart are at most an eighth of a frame
    /// from any instant, and a signal band-limited to `p` of the Nyquist rate (the
    /// [`Passband`](crate::Passband), 0.9) moves by at most the factor `cos(pi * p / 8)`
    /// over that offset, so the largest point reads at least `0.9382` of the true peak: the
    /// input's true peak is over what the detector read by at most
    /// [`TRUE_PEAK_MARGIN`](LimiterBank::TRUE_PEAK_MARGIN). The bound is for the input's
    /// band-limited content; the modulation the gain itself adds to the output is not
    /// covered. Content above the passband is attenuated by the filter's transition band,
    /// which the bound does not cover either. The detector reads each sample at most 2^100
    /// in magnitude and a NaN as 0.
    ///
    /// **The alignment.** The up-sampler delays by `U` frames of the 4x rate, so the group
    /// that leaves with frame `n` is the signal around `n - U/4`, and the gain of audio
    /// frame `m` takes the groups that cover the instants within a frame of it: groups
    /// `m + ceil((U - 7) / 4)` through `m + 1 + floor(U / 4)`, the newest of which must
    /// have been seen by the time the audio leaves, `lookahead` frames after it. (A
    /// frame's reach of one frame either side is an approximation, valid while the gain
    /// varies slowly across it.) [`LimiterBank::detecting`] refuses a lookahead under
    /// `1 + floor(U / 4)` frames, about 20. The latency stays the lookahead; only the
    /// window narrows.
    TruePeak,
}

/// The input of the true-peak detector, held to this magnitude (2^100): the half-band sums
/// of a sample this large stay finite, and a larger one reads as it.
const TRUE_PEAK_INPUT_LIMIT: Amplitude = Amplitude::new(1.267_650_600_228_229_4e30);

/// What [`LimiterBank::detecting`] refused.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum TruePeakError {
    /// The lookahead is shorter than the up-sampler's delay lets the detection line up.
    #[error(
        "a true-peak limiter at this rate looks at least {needed} frames ({ms:.2} ms) ahead, not {lookahead} frames",
        needed = needed.get(),
        lookahead = lookahead.get(),
        ms = shortest.get().get() * 1e3
    )]
    Lookahead {
        /// The frames the lookahead came to at the rate.
        lookahead: Samples,
        /// The fewest frames that line up.
        needed: Samples,
        /// The shortest lookahead that lines up at the rate.
        shortest: Lookahead,
    },
    /// The up-sampler cannot be designed at the rate.
    #[error("the true-peak oversampler cannot be designed: {0}")]
    Design(#[from] FirError),
}

/// The controls of a [`LimiterBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterControls {
    /// The input level, in dB, at which the curve reaches the ceiling: where reduction
    /// begins. `-inf` is a threshold (every signal is over it, so the gain is always the
    /// ceiling over the peak) and `+inf` is one (every signal is under it, so a gain of
    /// 0); only a NaN has no level and reads as the default's.
    pub threshold: Db,
    /// The level the output is bounded by, in dB. `+-inf` are ceilings (`-inf` mutes);
    /// only a NaN has no level and reads as the default's.
    pub ceiling: Db,
    /// How fast the gain recovers after a reduction: the time constant of a one-pole
    /// smoother toward the curve's gain. Floored at 1 ms (a limiter that recovers in no
    /// time is a clipper); not finite reads as the default's.
    pub release: Seconds,
}

impl LimiterControls {
    /// The usual mastering limiter: a -6 dB threshold, a -0.3 dB ceiling (the headroom
    /// an inter-sample peak needs) and a release of 100 ms. A quiet signal is raised by
    /// 5.7 dB, and the output reaches -0.3 dB when the input reaches -6 dB. What a
    /// non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db::new(-6.0),
        ceiling: Db::new(-0.3),
        release: Seconds::new(0.1),
    };

    /// The shortest release a limiter has: 1 ms.
    pub const MIN_RELEASE: Seconds = Seconds::new(0.001);
}

impl Default for LimiterControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

tutti_units::ranged! {
    /// How far ahead a limiter looks: the delay it runs its gain decision ahead of the
    /// audio by, and so the processing latency it declares to PDC.
    ///
    /// A range-checked value: finite, above zero and at most [`CAP`](Self::CAP).
    /// [`new`](Self::new) refuses anything else and never clamps, since the lookahead is the
    /// latency the graph compensates, and a silently different figure would misalign
    /// everything downstream.
    #[derive(PartialOrd)]
    Lookahead(Seconds), LookaheadError, "a lookahead, above zero and at most one second",
    |raw| raw.get().is_finite() && raw.get() > 0.0 && raw.get() <= Lookahead::CAP.get()
}

impl Lookahead {
    /// The lookahead of a limiter built without one: 5 ms, which catches peaks
    /// transparently for a delay a mix tolerates.
    pub const DEFAULT: Self = Self(Seconds::new(0.005));
    /// The longest lookahead [`new`](Self::new) accepts: one second, far past any
    /// limiter's (a mastering limiter looks a few to a few tens of milliseconds ahead).
    pub const CAP: Seconds = Seconds::new(1.0);
}

impl Default for Lookahead {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The longest window a limiter holds, in frames: 16.7 million, 87 s at 192 kHz. A
/// [`Lookahead`] is at most one second, so this only bounds the window at an absurd rate
/// (a rate no device has), where the count would otherwise be as large as the rate.
const MAX_LOOKAHEAD_FRAMES: usize = 1 << 24;

/// The lookahead window in frames at `rate`: `ceil(lookahead * rate)`, at least one (a
/// lookahead is above zero, so its ceiling is). The ring must hold at least the window, so
/// it rounds up; it is the one figure the latency and the delay both are.
fn lookahead_frames(lookahead: Lookahead, rate: SampleRate) -> usize {
    lookahead
        .get()
        .to_samples_ceil(rate)
        .get()
        .clamp(1, MAX_LOOKAHEAD_FRAMES)
}

/// What retargeting a limiter to a set of controls designs: the curve, the levels a frame nobody
/// drives falls back to, and the release as a one-pole pole.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Design<F: Float> {
    /// The transfer curve at the set threshold and ceiling, for a block no control signal drives.
    curve: FloatCurve<F>,
    /// What a frame nobody drives (or a non-finite one) is: the set threshold in dB.
    threshold: F,
    /// And the set ceiling in dB, held within `SIGNAL_DB_LIMIT`.
    ceiling: F,
    /// `SIGNAL_DB_LIMIT` in dB.
    signal_limit: F,
    /// The one-pole the envelope releases at.
    release: Pole<F>,
}

impl<F: Float> Design<F> {
    /// The design of `controls` at `rate`: a NaN threshold or ceiling and a non-finite
    /// release are the default's (`+-inf` are thresholds and ceilings; only a NaN has no
    /// level), and the release is floored at 1 ms.
    fn of(controls: &LimiterControls, rate: SampleRate) -> Self {
        let d = LimiterControls::DEFAULT;
        let threshold = if controls.threshold.get().is_nan() {
            d.threshold
        } else {
            controls.threshold
        };
        let ceiling = if controls.ceiling.get().is_nan() {
            d.ceiling
        } else {
            controls.ceiling
        };
        let release = if controls.release.get().is_finite() {
            controls.release
        } else {
            d.release
        };
        Self {
            curve: LimiterCurve::new(threshold, ceiling).in_float(),
            threshold: F::from_unit(threshold),
            ceiling: F::from_unit(ceiling),
            signal_limit: F::from_unit(SIGNAL_DB_LIMIT),
            release: Pole::of(
                Seconds::new(release.get().max(LimiterControls::MIN_RELEASE.get())),
                rate,
            ),
        }
    }
}

/// The per-frame control signals a limiter reads beside its audio, in the bank's float, each `None`
/// where nothing drives it. A control signal is at least as long as the frames a call renders;
/// a frame past its end is not driven. A frame that is not finite is the set value (a
/// control signal value of `+-inf` is not a level to divide by: the curve in dB would make a NaN of
/// it, which the envelope would keep); a finite one is held within 300 dB either way. The
/// set value a frame falls back to keeps its meaning (`-inf` ceiling mutes, `+inf`
/// threshold is a gain of 0, `-inf` threshold limits everything) but for a ceiling above
/// 300 dB, which is held at 300.
#[derive(Clone, Copy, Debug, Default)]
pub struct LimiterSignals<'a, F: Float> {
    /// The threshold in dB.
    pub threshold: Option<Mono<'a, F>>,
    /// The ceiling in dB.
    pub ceiling: Option<Mono<'a, F>>,
}

/// A lookahead limiter of any width: one linked gain from the loudest channel, a delay per
/// channel, and the envelope and window minimum that decide the gain.
///
/// Built on the control side for one rate, where it allocates the rings; a clone keeps
/// their size, and [`reset`](Self::reset) empties them without allocating. The state, the views
/// and the control signals are `F`, the graph's float.
#[derive(Clone, Debug)]
pub struct LimiterBank<F: Float> {
    design: Design<F>,
    rate: SampleRate,
    /// The lookahead in frames at `rate`: the bank's latency.
    lookahead: usize,
    /// The envelope gain (linear): instant attack, one-pole release. `+inf` is **unset**
    /// (after a reset): the first frame replaces it, through the attack branch, before any
    /// arithmetic reads it (a target is finite, so it is under `+inf`), and it never leaves
    /// the bank: the window minimum is pushed the replaced value. A per-sample level in `F`.
    envelope: Smoothed<F>,
    ring: LookaheadRing<F>,
    /// The true-peak detector; `None` reads the sample peak.
    detector: Option<Detector<F>>,
}

/// The state of a true-peak bank's detection.
#[derive(Clone, Debug)]
struct Detector<F: Float> {
    /// One 4x up-sampler per channel.
    states: Vec<Oversampler<F>>,
    /// The linked sample peaks of the last frames, as many as the groups a frame's window
    /// starts after it: a frame's own sample joins the groups of its window.
    lag: Vec<F>,
    /// Where `lag` is read and written next.
    at: usize,
}

impl<F: Float> Detector<F> {
    /// `peak` of the newest frame in, the peak of the frame `lag.len()` before it out.
    #[inline]
    fn delayed(&mut self, peak: F) -> F {
        if self.lag.is_empty() {
            return peak;
        }
        let out = std::mem::replace(&mut self.lag[self.at], peak);
        self.at = (self.at + 1) % self.lag.len();
        out
    }
}

impl<F: Float> LimiterBank<F> {
    /// How far over the ceiling a [`TruePeak`](PeakDetection::TruePeak) bank's true peak may
    /// reach for input within the oversampler's passband: `20 log10(1 / cos(pi * 0.9 / 8))`
    /// is 0.5539 dB, from the 4x grid's largest offset of an eighth of a frame. The two up
    /// stages' passband ripple, each about `10^(-100 / 20)` (the oversampler's documented
    /// figure for its default attenuation), adds 0.0002 dB; the sum rounds up to 0.56 dB.
    pub const TRUE_PEAK_MARGIN: Db = Db::new(0.56);

    /// A silent limiter `width` channels wide at `rate`, looking `lookahead` ahead, set
    /// at [`LimiterControls::DEFAULT`]. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout, lookahead: Lookahead, rate: SampleRate) -> Self {
        let frames = lookahead_frames(lookahead, rate);
        Self::build(width, frames, frames + 1, rate, None)
    }

    /// A bank of `frames` of lookahead whose gain window holds the newest `window` gains,
    /// with the true-peak `detector` when there is one.
    fn build(
        width: ChannelLayout,
        frames: usize,
        window: usize,
        rate: SampleRate,
        detector: Option<Detector<F>>,
    ) -> Self {
        Self {
            design: Design::of(&LimiterControls::DEFAULT, rate),
            rate,
            lookahead: frames,
            envelope: Self::UNSET,
            ring: LookaheadRing::new(usize::from(width.count()), frames, window),
            detector,
        }
    }

    /// A limiter that reads its peak by `detection`: [`new`](Self::new) for
    /// [`Sample`](PeakDetection::Sample), a true-peak one for
    /// [`TruePeak`](PeakDetection::TruePeak). Allocates: build it on the control side.
    ///
    /// # Errors
    ///
    /// [`TruePeakError::Lookahead`] when a true-peak `lookahead` is under what the
    /// up-sampler's delay needs at `rate`: the two settings are valid only together.
    /// [`TruePeakError::Design`] when the up-sampler cannot be designed at `rate`.
    pub fn detecting(
        width: ChannelLayout,
        lookahead: Lookahead,
        rate: SampleRate,
        detection: PeakDetection,
    ) -> Result<Self, TruePeakError> {
        if detection == PeakDetection::Sample {
            return Ok(Self::new(width, lookahead, rate));
        }
        let plan: OversamplingPlan = Oversampling::new(OversamplingFactor::X4).plan(rate)?;
        let delay = plan.upsample_delay();
        let frames = lookahead_frames(lookahead, rate);
        let needed = 1 + delay / 4;
        if frames < needed {
            let shortest = Lookahead::new(Seconds::new(needed as f64 / rate.get()))
                .unwrap_or(Lookahead(Lookahead::CAP));
            return Err(TruePeakError::Lookahead {
                lookahead: Samples::new(frames),
                needed: Samples::new(needed),
                shortest,
            });
        }
        let first = delay.saturating_sub(7).div_ceil(4);
        let channels = usize::from(width.count());
        let block = MaxBlock::of(Samples::new(CHUNK));
        let detector = Detector {
            states: (0..channels.max(1)).map(|_| plan.state(block)).collect(),
            lag: vec![F::ZERO; first],
            at: 0,
        };
        Ok(Self::build(
            width,
            frames,
            frames - first + 1,
            rate,
            Some(detector),
        ))
    }

    /// The processing latency: the lookahead in frames, at least one.
    #[must_use]
    pub fn latency(&self) -> Samples {
        Samples::new(self.lookahead)
    }

    /// Back to silence: the rings empty and the envelope without history. No allocation.
    pub fn reset(&mut self) {
        self.envelope = Self::UNSET;
        self.ring.clear();
        if let Some(d) = &mut self.detector {
            d.states.iter_mut().for_each(Oversampler::reset);
            d.lag.fill(F::ZERO);
            d.at = 0;
        }
    }

    /// Designs `controls` and lands them whole. No allocation.
    pub fn retarget(&mut self, controls: &LimiterControls) {
        self.design = Design::of(controls, self.rate);
    }

    /// The envelope with no history: the next frame takes its target at once.
    const UNSET: Smoothed<F> = Smoothed::at(F::INFINITY);

    /// The target gain of frame `i`, whose linked peak is `peak`.
    #[inline]
    fn target_gain(&self, signals: &LimiterSignals<'_, F>, i: usize, peak: F) -> F {
        if signals.threshold.is_none() && signals.ceiling.is_none() {
            return self.design.curve.gain(peak);
        }
        // A control signal's finite value, held within `SIGNAL_DB_LIMIT` either way; else the set
        // value, held only where it can overflow or cancel: a ceiling at most
        // `SIGNAL_DB_LIMIT` (a `+inf` or 1e30 dB one would make an infinite gain), and a
        // threshold as it is (`-inf` leaves every signal over it, `+inf` none, a gain of 0).
        // A ceiling of `-inf` is a gain of 0 and stays one: only an infinity can make the
        // envelope NaN.
        let limit = self.design.signal_limit;
        let signal_or = |signal: Option<Mono<'_, F>>, base: F, held: fn(F, F) -> F| {
            signal
                .and_then(|l| l.samples().get(i))
                .copied()
                .filter(|x| x.is_finite())
                .map_or_else(|| held(base, limit), |x| x.clamp(-limit, limit))
        };
        let threshold = signal_or(signals.threshold, self.design.threshold, |t, _| t);
        let ceiling = signal_or(signals.ceiling, self.design.ceiling, F::min);
        limiter_gain(level_db(peak), threshold, ceiling)
    }

    /// Runs `input` into `output`, delayed by the lookahead and scaled by the linked gain.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's
    /// width, or past the narrower of the two views, is not output (the linked peak is the
    /// loudest of every input channel). No allocation.
    pub fn process(
        &mut self,
        signals: LimiterSignals<'_, F>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .ring
            .width()
            .min(input.channels())
            .min(output.channels());
        let mut gains = [F::ZERO; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];

            // The linked peak: the loudest channel at each frame, taken a channel at a time
            // (`max` is exact, so the order does not matter).
            gains.fill(F::ZERO);
            for plane in input.iter() {
                for (p, &x) in gains.iter_mut().zip(&plane.samples()[start..end]) {
                    *p = fold_peak(*p, x);
                }
            }
            if let Some(detector) = &mut self.detector {
                // The loudest point of each frame's group of four, over the channels, and
                // the sample peak of the frame the window starts after.
                for p in gains.iter_mut() {
                    *p = detector.delayed(*p);
                }
                for (state, plane) in detector.states.iter_mut().zip(input.iter()) {
                    group_peaks(state, &plane.samples()[start..end], gains);
                }
            }

            // The recursive half: the envelope and the sliding minimum, one frame at a
            // time; each frame's peak becomes its window-minimum gain.
            for (k, g) in gains.iter_mut().enumerate() {
                let target = self.target_gain(&signals, start + k, *g);
                // The instant attack: an unset envelope (`+inf`) takes its first target here.
                if target < self.envelope.value() {
                    self.envelope = Smoothed::at(target);
                } else {
                    self.design.release.step(&mut self.envelope, target);
                }
                *g = self.ring.push_gain(self.envelope.value());
            }

            for c in 0..width {
                self.ring.delay_scaled(
                    c,
                    &input.plane(c).samples()[start..end],
                    gains,
                    &mut output.plane_mut(c).samples_mut()[start..end],
                );
            }
            start = end;
        }
    }
}

/// Folds into `peaks` the loudest of the four 4x points of each frame of `samples`.
fn group_peaks<F: Float>(state: &mut Oversampler<F>, samples: &[F], peaks: &mut [F]) {
    let mut clean = [F::ZERO; CHUNK];
    let clean = &mut clean[..samples.len()];
    let limit = F::from_unit(TRUE_PEAK_INPUT_LIMIT);
    for (c, &x) in clean.iter_mut().zip(samples) {
        *c = if x.is_nan() {
            F::ZERO
        } else {
            x.clamp(-limit, limit)
        };
    }
    // `samples` is at most `CHUNK` frames, the `max_block` the true-peak state is built for.
    state
        .upsample(Mono::new(clean))
        .expect("a chunk is at most the state's max block");
    let (high, _) = state.high_rate();
    let (groups, _) = high.samples().as_chunks::<4>();
    for (p, group) in peaks.iter_mut().zip(groups) {
        *p = group.iter().fold(*p, |peak, &x| peak.max(level(x)));
    }
}

#[cfg(test)]
mod tests;
