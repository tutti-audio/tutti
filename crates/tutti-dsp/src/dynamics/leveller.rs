//! [`LevellerBank`]: a real-time loudness leveller (automatic gain control on the short-term
//! loudness) over buffers views.
//!
//! The leveller measures the short-term loudness of its input as EBU Tech 3341 defines it,
//! moves one gain toward the one that would put that loudness at a target, and multiplies
//! **every** channel by it, so the image does not move.
//!
//! # The measurement
//!
//! Each channel runs through the [`KWeighting`](crate::KWeighting) of ITU-R BS.1770 at the bank's rate. The
//! weighted mean squares of the channels are summed with the [`ChannelWeight`](crate::ChannelWeight) of each, and
//! the sum, per frame, is accumulated over hops of 100 ms. The short-term window is the last
//! 30 hops (3 s, Tech 3341's short-term window), a ring of the hops' sums that holds one
//! float a hop and never allocates. Its loudness is
//!
//! ```text
//!   L = -0.691 + 10 log10( sum of the window's hop sums / frames in the window )   LUFS
//! ```
//!
//! and is read each time a hop completes, so it updates ten times a second. Until the window
//! has filled it is the loudness of the hops it holds, from the first complete hop on; before
//! that there is no reading. The input is measured **before** the gain, so the leveller is
//! feed-forward and cannot oscillate.
//!
//! # The gain
//!
//! The wanted gain is `target - L`, held within `[-max_cut, max_boost]`. It is smoothed in dB
//! by a one-pole follower: a gain that falls (a loud input being cut) follows the `attack`
//! time and one that rises (a quiet input being brought up) follows the `release`, the
//! words of a compressor. In steady state the output sits at the target to within the
//! measurement, and an input outside the gain range is left that far from it.
//!
//! The gain never rises while the short-term loudness is under the `gate`, or before the
//! first reading: silence and the noise floor are not brought up. It may still fall there. A
//! bank starts at 0 dB.
//!
//! # Latency
//!
//! None. The gain applied to a frame comes from the frames before it, so a sudden loud onset
//! is heard at the old gain until the window and the attack follow it: a peak limiter after
//! the leveller catches that, as it does after any gain stage. Looking ahead would
//! add latency without making the 3 s measurement any more causal. The leveller has no
//! tail: it only scales.

use core::ops::Range;

use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Db, SampleRate, Samples, Seconds};

use super::curves::db_gain;
use super::gain_pass::{CHUNK, floored};
use crate::follower::{Follower, Smoothed};
use crate::{ChannelWeights, Ramp, SimdLanes, Sos, SosBank, SosCoefficients};

/// The controls of a [`LevellerBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevellerControls {
    /// The short-term loudness the output is moved to, in LUFS.
    pub target: Db,
    /// The most the gain adds, in dB. Read within `[0, `[`MAX_RANGE`](Self::MAX_RANGE)`]`.
    pub max_boost: Db,
    /// The most the gain removes, in dB. Read as `max_boost` is.
    pub max_cut: Db,
    /// The short-term loudness, in LUFS, under which the gain does not rise.
    pub gate: Db,
    /// How fast the gain falls toward a smaller one: the time constant of a one-pole
    /// smoother. Below 0 reads as 0 (instant); one that is not finite is the default's.
    pub attack: Seconds,
    /// How fast the gain rises toward a larger one. Read as `attack` is.
    pub release: Seconds,
}

impl LevellerControls {
    /// The most the boost or the cut reaches: 60 dB, a gain of 1000.
    pub const MAX_RANGE: Db = Db::new(60.0);

    /// EBU R128's programme loudness, -23 LUFS, with up to 12 dB of boost and 12 dB of cut, a gate
    /// at -50 LUFS, a 0.5 s attack and a 2 s release. What a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        target: Db::new(-23.0),
        max_boost: Db::new(12.0),
        max_cut: Db::new(12.0),
        gate: Db::new(-50.0),
        attack: Seconds::new(0.5),
        release: Seconds::new(2.0),
    };

    /// These controls within range, a non-finite control as the default's.
    fn designed(&self) -> Settings {
        let d = Self::DEFAULT;
        let range = |db: Db, default: Db| {
            db.finite_or(default)
                .clamp(Db::UNITY, LevellerControls::MAX_RANGE)
        };
        Settings {
            target: self.target.finite_or(d.target),
            max_boost: range(self.max_boost, d.max_boost),
            max_cut: range(self.max_cut, d.max_cut),
            gate: self.gate.finite_or(d.gate),
            attack: self.attack.finite_or(d.attack),
            release: self.release.finite_or(d.release),
        }
    }
}

impl Default for LevellerControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls in range.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Settings {
    target: Db,
    max_boost: Db,
    max_cut: Db,
    gate: Db,
    attack: Seconds,
    release: Seconds,
}

/// The constant of BS.1770's loudness: the K-weighting's gain at 997 Hz (+0.691 dB) taken out,
/// so a sine of that frequency in one channel reads its RMS level in dBFS.
const LOUDNESS_OFFSET: Db = Db::new(-0.691);

/// The hop the short-term window advances by: 100 ms, as Tech 3341 steps its 400 ms momentary window.
const HOP: Seconds = Seconds::new(0.1);

/// The hops in the short-term window: 3 s of 100 ms.
const WINDOW_HOPS: usize = 30;

/// The sliding short-term window: the weighted power summed over each 100 ms hop, in a ring of
/// the last [`WINDOW_HOPS`].
#[derive(Clone, Debug)]
struct ShortTerm {
    /// The frames a hop spans.
    hop: usize,
    /// The sum of weighted mean squares over each hop; the ring's slot `head` is the oldest
    /// once it is full.
    sums: [f64; WINDOW_HOPS],
    /// The hops the ring holds, up to [`WINDOW_HOPS`].
    held: usize,
    head: usize,
    /// The frames of the hop in progress, and their summed power.
    frames: usize,
    power: f64,
}

impl ShortTerm {
    /// An empty window at `rate`: the hop is 100 ms rounded up to a frame.
    fn new(rate: SampleRate) -> Self {
        Self {
            hop: HOP.to_samples_ceil(rate).get().max(1),
            sums: [0.0; WINDOW_HOPS],
            held: 0,
            head: 0,
            frames: 0,
            power: 0.0,
        }
    }

    fn reset(&mut self) {
        self.sums = [0.0; WINDOW_HOPS];
        self.held = 0;
        self.head = 0;
        self.frames = 0;
        self.power = 0.0;
    }

    /// Takes one frame's weighted power. Returns the window's loudness when this frame
    /// completes a hop.
    #[inline]
    fn push(&mut self, power: f64) -> Option<Db> {
        self.power += power;
        self.frames += 1;
        if self.frames < self.hop {
            return None;
        }
        let slot = (self.head + self.held) % WINDOW_HOPS;
        self.sums[slot] = self.power;
        if self.held < WINDOW_HOPS {
            self.held += 1;
        } else {
            self.head = (self.head + 1) % WINDOW_HOPS;
        }
        self.frames = 0;
        self.power = 0.0;
        Some(self.loudness())
    }

    /// The loudness of the hops held (at least one), their sums added afresh each hop so no
    /// error accumulates.
    fn loudness(&self) -> Db {
        let sum: f64 = self.sums[..self.held].iter().sum();
        Db::from_power(sum / (self.held * self.hop) as f64) + LOUDNESS_OFFSET
    }
}

/// What the gain is doing: the dB it moves toward, as a per-sample level in `F`, and whether it
/// may rise there (it may not before the first reading or under the gate).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Aim<F> {
    to: F,
    may_rise: bool,
}

/// A loudness leveller of any width: one gain from the short-term loudness of the K-weighted,
/// channel-weighted sum, applied to every channel.
///
/// Built on the control side for one rate and one set of channel weights; the state and the
/// views are `F`, the graph's float, and the loudness is summed in `f64`. Nothing it holds
/// grows.
#[derive(Clone, Debug)]
pub struct LevellerBank<F: Float + SimdLanes> {
    /// The weight `G` each channel counts for.
    weights: Box<[f64]>,
    /// The K-weighting, one design at the rate for every channel.
    filter: SosBank<F>,
    coefficients: Ramp<SosCoefficients<F>>,
    window: ShortTerm,
    settings: Settings,
    rate: SampleRate,
    /// The follower on the gain: the rising pole is the release.
    follower: Follower<F>,
    /// The gain in dB: a per-sample level in `F`.
    gain: Smoothed<F>,
    /// The gain the last frame applied, in dB and as an amplitude: a held gain reuses it.
    applied: (F, F),
    aim: Aim<F>,
    /// The short-term loudness at the last hop, or `None` before the first.
    loudness: Option<Db>,
}

impl<F: Float + SimdLanes> LevellerBank<F> {
    /// A leveller measuring through `weighting` (its rate is the bank's: a
    /// [`KWeighting::design`](crate::KWeighting::design), which fails at a rate with no design, so a bank has one) over
    /// channels weighted by `weights` (at most [`MAX_CHANNELS`]; a channel past that is not
    /// heard), at [`LevellerControls::DEFAULT`] and 0 dB. A cascade of another shape measures
    /// another loudness: no section measures the unweighted signal. Allocates: build it on
    /// the control side.
    ///
    /// # Panics
    ///
    /// Never for a K-weighting design or a cascade of no section; a cascade of more than
    /// [`MAX_SECTIONS`](crate::MAX_SECTIONS) sections does.
    #[must_use]
    pub fn new(weighting: &Sos, weights: &ChannelWeights) -> Self {
        let rate = weighting.rate();
        let weights: Box<[f64]> = weights
            .as_slice()
            .iter()
            .take(MAX_CHANNELS)
            .map(|w| w.power())
            .collect();
        let width = ChannelLayout::clamped(weights.len());
        let settings = LevellerControls::DEFAULT.designed();
        // `SosBank::process` takes its coefficients as a `Ramp`: this one holds the cascade
        // and is never retargeted, so it stays constant.
        let coefficients = SosCoefficients::new(weighting)
            .expect("a measurement cascade is at most a bank's sections");
        Self {
            weights,
            filter: SosBank::new(width),
            coefficients: Ramp::new(coefficients),
            window: ShortTerm::new(rate),
            settings,
            rate,
            follower: Self::follower(&settings, rate),
            gain: Smoothed::ZERO,
            applied: (F::ZERO, F::ONE),
            aim: Aim {
                to: F::ZERO,
                may_rise: false,
            },
            loudness: None,
        }
    }

    /// The follower for the times of `settings`: its rising pole is the gain recovering, the
    /// `release`.
    fn follower(settings: &Settings, rate: SampleRate) -> Follower<F> {
        Follower::of(floored(settings.release, settings.attack), rate)
    }
}

impl<F: Float + SimdLanes> LevellerBank<F> {
    /// Back to the start: the K-weighting's state and the window empty, the gain at 0 dB, no
    /// reading. No allocation.
    pub fn reset(&mut self) {
        self.filter.reset();
        self.coefficients.jump_to_target();
        self.window.reset();
        self.gain = Smoothed::ZERO;
        self.applied = (F::ZERO, F::ONE);
        self.loudness = None;
        self.aim = self.aimed();
    }

    /// Aims at `controls`: a target or a range that moves takes effect at the next frame, and
    /// the gain follows it at the new times. No allocation.
    pub fn retarget(&mut self, controls: &LevellerControls) {
        self.settings = controls.designed();
        self.follower = Self::follower(&self.settings, self.rate);
        self.aim = self.aimed();
    }

    /// The short-term loudness, in LUFS, as of the last completed hop of 100 ms; `None` until
    /// the first. It is the loudness of the input, before the gain.
    #[must_use]
    pub fn short_term(&self) -> Option<Db> {
        self.loudness
    }

    /// The gain in force, in dB.
    #[must_use]
    pub fn gain(&self) -> Db {
        Db::new(self.gain.value().into())
    }

    /// Where the gain aims for the loudness read and the settings: `target - L` within the
    /// range, and not rising before a reading or under the gate.
    fn aimed(&self) -> Aim<F> {
        let s = &self.settings;
        let Some(loudness) = self.loudness else {
            return Aim {
                to: F::ZERO,
                may_rise: false,
            };
        };
        let wanted = (s.target - loudness).clamp(-s.max_cut, s.max_boost);
        Aim {
            to: F::from_unit(wanted),
            may_rise: loudness >= s.gate,
        }
    }

    /// One frame of the gain, as an amplitude: the follower's step toward the aim, none where
    /// the aim is above the gain and may not be risen to.
    #[inline]
    fn gain_frame(&mut self) -> F {
        let value = self.gain.value();
        let climbs = self.aim.to > value;
        if (climbs && self.aim.may_rise) || self.aim.to < value {
            self.follower.follow_raw(&mut self.gain, self.aim.to);
        }
        let db = self.gain.value();
        if db != self.applied.0 {
            self.applied = (db, db_gain(db));
        }
        self.applied.1
    }

    /// The weighted power of each frame of `frames` of `input`, into `out` (as long as `frames`,
    /// at most [`CHUNK`]): the sum over the channels of `G` times the K-weighted square. A
    /// frame whose filtered value is not finite adds nothing, and the filter's state resets
    /// after the chunk, so a non-finite input is heard for at most one chunk. No allocation.
    fn powers(&mut self, input: &Planes<'_, F>, frames: Range<usize>, out: &mut [f64]) {
        out.fill(0.0);
        let n = frames.len().min(CHUNK);
        let heard = input.slice(Samples::new(frames.start)..Samples::new(frames.start + n));
        let width = heard.channels().min(self.weights.len());
        let mut scratch = [[F::ZERO; CHUNK]; MAX_CHANNELS];
        let mut outs: [&mut [F]; MAX_CHANNELS] = scratch.each_mut().map(|p| &mut p[..n]);
        // Every plane is `n` frames long and `width` is within `MAX_CHANNELS`.
        let Ok(mut filtered) = PlanesMut::new(&mut outs[..width]) else {
            debug_assert!(false, "the scratch planes are one length");
            return;
        };
        self.filter
            .process(&mut self.coefficients, heard, filtered.reborrow());
        let mut non_finite = false;
        for (plane, &weight) in filtered.iter().zip(self.weights.iter()) {
            for (p, &y) in out.iter_mut().zip(plane.samples()) {
                if y.is_finite() {
                    let y: f64 = y.into();
                    *p += weight * y * y;
                } else {
                    non_finite = true;
                }
            }
        }
        if non_finite {
            self.filter.reset();
        }
    }

    /// Runs `input` into `output`, scaled by one gain from the short-term loudness of `input`.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the narrower of the
    /// two views is not touched, and the frames rendered are the fewer of the two views'. A
    /// channel of the bank that `input` does not have is silent. The result does not depend on
    /// how the stream is cut into calls. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let mut powers = [0.0f64; CHUNK];
        let mut gains = [F::ZERO; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            self.powers(&input, start..end, &mut powers[..n]);
            for (g, &power) in gains[..n].iter_mut().zip(&powers) {
                if let Some(loudness) = self.window.push(power) {
                    self.loudness = Some(loudness);
                    self.aim = self.aimed();
                }
                *g = self.gain_frame();
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, &x), &g) in ys.iter_mut().zip(xs).zip(&gains[..n]) {
                    *y = x * g;
                }
            }
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;
