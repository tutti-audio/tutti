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
//! rounding; it is not a true-peak limit, which would need an oversampled detector. The
//! lookahead is the bank's own constant, fixed when it is built, and **is its latency**
//! ([`latency`](LimiterBank::latency)): the graph's PDC compensates for it. It has no tail,
//! which excludes latency: what the delay holds comes out within the latency (the output is
//! exactly silent that many frames after the input is), and gain on silence is silence.
//!
//! The threshold and the ceiling are the two controls read **per frame** when the graph
//! drives them ([`LimiterLanes`]); a frame that is not finite falls back to the aimed
//! value. Every coefficient lands whole: a glided ceiling would let a sample past the new
//! one.
//!
//! A reset leaves the envelope **unset** (`+inf`, no history), and the first frame takes
//! its target at once, whatever it is: the first audio is limited from its first sample,
//! as a limiter that had heard silence for ever would limit it (on silence the envelope
//! settles at the drive, the target of a zero peak, which is where an unset envelope
//! meets its first quiet frame).

/// The envelope with no history: the next frame takes its target at once.
const UNSET: f32 = f32::INFINITY;

/// The most a lane's level may say, in dB either way: past it a lane value reads as this.
/// 300 dB is 1e15 as an amplitude, so a gain from a lane's ceiling and threshold, and from
/// the peak (floored at -144 dB), stays finite in `f32`.
const LANE_DB_LIMIT: f32 = 300.0;

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_types::{Amplitude, ChannelLayout, Db, SampleRate, Samples, Seconds};

use super::curves::{LimiterCurve, amplitude_to_db, compute_limiter_gain};
use super::lookahead::{LookaheadRing, fold_peak};
use super::shared::CHUNK;
use crate::smoother::Smoother;

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
        threshold: Db(-6.0),
        ceiling: Db(-0.3),
        release: Seconds(0.1),
    };

    /// The shortest release a limiter has: 1 ms.
    pub const MIN_RELEASE: Seconds = Seconds(0.001);
}

impl Default for LimiterControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

tutti_types::ranged! {
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
    pub const DEFAULT: Self = Self(Seconds(0.005));
    /// The longest lookahead [`new`](Self::new) accepts: one second, far past any
    /// limiter's (a mastering limiter looks a few to a few tens of milliseconds ahead).
    pub const CAP: Seconds = Seconds(1.0);
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

/// What aiming a limiter at a set of controls designs: the curve, the levels a frame nobody
/// drives falls back to, and the release as a one-pole coefficient.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Design {
    /// The transfer curve at the aimed threshold and ceiling, for a block no lane drives.
    curve: LimiterCurve,
    /// What a frame nobody drives (or a non-finite one) is: the aimed threshold.
    threshold: Db,
    /// And the aimed ceiling.
    ceiling: Db,
    /// The one-pole coefficient the envelope releases at.
    release: f32,
}

impl Design {
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
            curve: LimiterCurve::new(threshold, ceiling),
            threshold,
            ceiling,
            release: Seconds(release.get().max(LimiterControls::MIN_RELEASE.get()))
                .one_pole_coeff(rate),
        }
    }
}

/// The per-frame lanes a limiter reads beside its audio, in the audio's sample format, each
/// `None` where nothing drives it. A lane is at least as long as the frames a call renders;
/// a frame past its end is not driven. A frame that is not finite is the aimed value (a
/// lane value of `+-inf` is not a level to divide by: the curve in dB would make a NaN of
/// it, which the envelope would keep); a finite one is held within 300 dB either way. The
/// aimed value a frame falls back to keeps its meaning (`-inf` ceiling mutes, `+inf`
/// threshold is a gain of 0, `-inf` threshold limits everything) but for a ceiling above
/// 300 dB, which is held at 300.
#[derive(Clone, Copy, Debug, Default)]
pub struct LimiterLanes<'a, S: Float = f32> {
    /// The threshold in dB.
    pub threshold: Option<Mono<'a, S>>,
    /// The ceiling in dB.
    pub ceiling: Option<Mono<'a, S>>,
}

/// A lookahead limiter of any width: one linked gain from the loudest channel, a delay per
/// channel, and the envelope and window minimum that decide the gain.
///
/// Built on the control side for one rate, where it allocates the rings; a clone keeps
/// their size, and [`reset`](Self::reset) empties them without allocating. The state is
/// `f32`; a view of another sample format is converted at the edges.
#[derive(Clone, Debug)]
pub struct LimiterBank {
    design: Design,
    rate: SampleRate,
    /// The lookahead in frames at `rate`: the bank's latency.
    lookahead: usize,
    /// The envelope gain (linear): instant attack, one-pole release. `+inf` is **unset**
    /// (after a reset): the first frame replaces it, through the attack branch, before any
    /// arithmetic reads it (a target is finite, so it is under `+inf`), and it never leaves
    /// the bank: the window minimum is pushed the replaced value.
    envelope: f32,
    ring: LookaheadRing,
}

impl LimiterBank {
    /// A silent limiter `width` channels wide at `rate`, looking `lookahead` ahead, aimed
    /// at [`LimiterControls::DEFAULT`]. Allocates: build it on the control side.
    #[must_use]
    pub fn new(width: ChannelLayout, lookahead: Lookahead, rate: SampleRate) -> Self {
        let frames = lookahead_frames(lookahead, rate);
        Self {
            design: Design::of(&LimiterControls::DEFAULT, rate),
            rate,
            lookahead: frames,
            envelope: UNSET,
            ring: LookaheadRing::new(usize::from(width.count()), frames),
        }
    }

    /// The processing latency: the lookahead in frames, at least one.
    #[must_use]
    pub fn latency(&self) -> Samples {
        Samples(self.lookahead)
    }

    /// Back to silence: the rings empty and the envelope without history. No allocation.
    pub fn reset(&mut self) {
        self.envelope = UNSET;
        self.ring.clear();
    }

    /// Designs `controls` and lands them whole. No allocation.
    pub fn aim(&mut self, controls: &LimiterControls) {
        self.design = Design::of(controls, self.rate);
    }

    /// The target gain of frame `i`, whose linked peak is `peak`.
    #[inline]
    fn target_gain<S: Float>(&self, lanes: &LimiterLanes<'_, S>, i: usize, peak: f32) -> f32 {
        if lanes.threshold.is_none() && lanes.ceiling.is_none() {
            return self.design.curve.gain(Amplitude::new(peak)).get();
        }
        // A lane's finite value, held within `LANE_DB_LIMIT` either way; else the aimed
        // value, held only where it can overflow or cancel: a ceiling at most
        // `LANE_DB_LIMIT` (a `+inf` or 1e30 dB one would make an infinite gain), and a
        // threshold as it is (`-inf` leaves every signal over it, `+inf` none, a gain of 0).
        // A ceiling of `-inf` is a gain of 0 and stays one: only an infinity can make the
        // envelope NaN.
        let lane_or = |lane: Option<Mono<'_, S>>, base: f32, held: fn(f32) -> f32| {
            lane.and_then(|l| l.samples().get(i))
                .map(|x| x.convert_to::<f32>())
                .filter(|x| x.is_finite())
                .map_or_else(|| held(base), |x| x.clamp(-LANE_DB_LIMIT, LANE_DB_LIMIT))
        };
        let peak_db = amplitude_to_db(Amplitude::new(peak));
        let threshold = Db(lane_or(lanes.threshold, self.design.threshold.get(), |t| t));
        let ceiling = Db(lane_or(lanes.ceiling, self.design.ceiling.get(), |c| {
            c.min(LANE_DB_LIMIT)
        }));
        compute_limiter_gain(peak_db, threshold, ceiling).get()
    }

    /// Runs `input` into `output`, delayed by the lookahead and scaled by the linked gain.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's
    /// width, or past the narrower of the two views, is not output (the linked peak is the
    /// loudest of every input channel). No allocation.
    pub fn process<S: Float>(
        &mut self,
        lanes: LimiterLanes<'_, S>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .ring
            .width()
            .min(input.channels())
            .min(output.channels());
        let mut gains = [0.0f32; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];

            // The linked peak: the loudest channel at each frame, taken a channel at a time
            // (`max` is exact, so the order does not matter).
            gains.fill(0.0);
            for plane in input.iter() {
                for (p, x) in gains.iter_mut().zip(&plane.samples()[start..end]) {
                    *p = fold_peak(*p, x.convert_to::<f32>());
                }
            }

            // The recursive half: the envelope and the sliding minimum, one frame at a
            // time; each frame's peak becomes its window-minimum gain.
            for (k, g) in gains.iter_mut().enumerate() {
                let target = self.target_gain(&lanes, start + k, *g);
                // The instant attack: an unset envelope (`+inf`) takes its first target here.
                if target < self.envelope {
                    self.envelope = target;
                } else {
                    self.envelope = Smoother::toward(self.envelope, target, self.design.release);
                }
                *g = self.ring.push_gain(self.envelope);
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

#[cfg(test)]
mod tests;
