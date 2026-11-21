//! [`DelayBank`]: one feedback delay line per channel, over buffers views, with the
//! delay and the feedbacks it reads as [`Ramp`]s.
//!
//! One frame of an uncoupled channel is [`delay_step`]: the feedback tap is read at
//! `max(d, 1) - 1` before the push, the line takes `x + fb * tap`, and the output is read
//! at `d` after it, so the loop's period is the output delay `d` and the impulse response
//! is `g^k` at `(k + 1) d` frames (an integer `d`). A coupled frame reads every channel's
//! tap before any line is written, adds the routed taps scaled by the cross-feedback to
//! each line's input, then reads the outputs.
//!
//! **Time** is channel 0's control, and the channels past the first follow it
//! ([`ChannelTimes::linked`](crate::ChannelTimes::linked)) or hold a time of their own
//! ([`ChannelTimes`](crate::ChannelTimes) from a slice of times). A synced layout
//! ([`DelayTiming::Synced`]) has no time control: each channel's time is its note's seconds
//! at a tempo ([`DelayBank::retarget_tempo`]), and a tempo that moves moves the time through
//! the same ramp. A
//! control signal drives the time and the feedback **per frame** ([`DelaySignals`]): a delay time
//! moved at audio rate is a flanger, a control signal frame that is not finite falls back to the
//! ramped value, and a control signal frame is the trajectory itself, not ramped. The time ramps at
//! most [`DELAY_SLEW`] frames of delay per frame (a moving delay is a pitch bend, so its
//! speed is bounded and not its duration), the feedback and the cross-feedback over a
//! length the caller sets.
//!
//! The delay has **no latency**: the echo is the effect, not processing delay.

use tutti_buffers::{Float, Mono, Planes, PlanesMut};
use tutti_units::{Bpm, Feedback, ReadPosition, SampleRate, Samples, Seconds};

use super::layout::{DELAY_SLEW, DelayControls, DelayLayout, DelayTiming, MAX_DELAY_CHANNELS};
use super::notes::NoteTimes;
use super::routing::FeedbackRouting;
use crate::{DelayLine, InterpolationMode, Lerp, Ramp, Steps, delay_step};

/// What a delay reads while it runs: the delay as a [`ReadPosition`], and the two feedbacks
/// in the bank's float `F`, each retargeted by [`DelayBank::retarget`] and stepped by
/// [`DelayBank::process`].
///
/// Each channel's time ramps at [`DELAY_SLEW`] frames of delay per frame; a channel that
/// follows channel 0 reads its ramp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayRamps<F: Float> {
    times: [Ramp<ReadPosition>; MAX_DELAY_CHANNELS],
    feedback: Ramp<F>,
    cross: Ramp<F>,
    /// The steps a feedback ramp takes.
    steps: u32,
}

impl<F: Float + Lerp> DelayRamps<F> {
    /// A delay at rest on no delay and no feedback, whose feedbacks take `gain_steps`
    /// steps to a new value.
    #[must_use]
    pub fn new(gain_steps: u32) -> Self {
        Self {
            times: [Ramp::new(ReadPosition::START); MAX_DELAY_CHANNELS],
            feedback: Ramp::new(F::ZERO),
            cross: Ramp::new(F::ZERO),
            steps: gain_steps,
        }
    }

    /// Jumps to the targets now.
    pub fn jump_to_target(&mut self) {
        for time in &mut self.times {
            time.jump_to_target();
        }
        self.feedback.jump_to_target();
        self.cross.jump_to_target();
    }

    /// Takes `frames` steps of each ramp.
    fn advance(&mut self, frames: u32) {
        for time in &mut self.times {
            time.advance(frames);
        }
        self.feedback.advance(frames);
        self.cross.advance(frames);
    }
}

/// `secs` as a delay position: clamped into `0..=max` and into the line of `cap` frames. A
/// NaN time is no delay.
#[inline(always)]
fn position_of(secs: Seconds, max: Seconds, rate: SampleRate, cap: usize) -> ReadPosition {
    let secs = Seconds::new(secs.get().clamp(0.0, max.get()));
    ReadPosition::of(secs, rate).min(ReadPosition::from_frames(Samples::new(cap)))
}

/// The loop's feedback and cross-feedback bounded as a pair ([`Feedback::stable_pair`]), in
/// the state float. A pair already inside `bound` (`Feedback::MAX_STABLE` in `F`; this mirrors
/// `Feedback::stable_pair`'s own in-bound test) is returned as it is, in `F`; one outside it is
/// scaled by `Feedback`'s rule in `f64`.
#[inline(always)]
fn stable_pair<F: Float>(fb: F, cross: F, bound: F) -> (F, F) {
    if fb >= F::ZERO && cross >= F::ZERO && fb + cross <= bound {
        return (fb, cross);
    }
    let (fb, cross) = Feedback::stable_pair(fb.into(), cross.into());
    (F::from_unit(fb), F::from_unit(cross))
}

/// The per-frame control signals a delay reads beside its audio, in the audio's float, each
/// `None` where nothing drives it. A control signal is at least as long as the frames a call renders;
/// a frame past its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct DelaySignals<'a, F: Float> {
    /// The delay time in seconds, of every channel.
    pub time: Option<Mono<'a, F>>,
    /// The feedback.
    pub feedback: Option<Mono<'a, F>>,
}

/// A feedback delay of any width: a [`DelayLine`] per channel, and the loop that
/// recirculates them, all in the state float `F`.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps
/// their size, and [`reset`](Self::reset) zeroes them without allocating.
#[derive(Clone, Debug)]
pub struct DelayBank<F: Float> {
    lines: Box<[DelayLine<F>]>,
    /// Each channel's feedback tap of the frame, read before any line is written (the
    /// coupled loop's).
    taps: Box<[F]>,
    interpolation: InterpolationMode,
    routing: FeedbackRouting,
    /// The routing's weights at `F`, row-major by the line fed: widened once at build.
    weights: Box<[F]>,
    linked: bool,
    /// The notes of a synced delay, `None` for one timed in seconds.
    synced: Option<NoteTimes>,
    rate: SampleRate,
    /// The line's capacity in frames: the delay is clamped into it.
    cap: usize,
    /// The longest delay.
    max: Seconds,
    /// The times of the channels past the first that hold their own, in seconds.
    own: [Seconds; MAX_DELAY_CHANNELS],
}

impl<F: Float + Lerp> DelayBank<F> {
    /// A silent delay of `layout` at `rate`: a line per channel of `ceil(max_delay *
    /// rate)` frames. Allocates: build it on the control side.
    #[must_use]
    pub fn new(layout: &DelayLayout, rate: SampleRate) -> Self {
        let width = layout.width();
        let cap = layout.max_delay().frames(rate);
        let mut own = [Seconds::ZERO; MAX_DELAY_CHANNELS];
        let (linked, synced) = match layout.timing() {
            DelayTiming::Free(times) => {
                for (slot, t) in own.iter_mut().skip(1).zip(times.fixed()) {
                    *slot = *t;
                }
                (times.is_linked(), None)
            }
            DelayTiming::Synced(notes) => (false, Some(*notes)),
        };
        Self {
            lines: (0..width).map(|_| DelayLine::new(cap)).collect(),
            taps: vec![F::ZERO; width].into_boxed_slice(),
            interpolation: layout.interpolation(),
            routing: *layout.routing(),
            weights: (0..width * width)
                .map(|k| F::from_unit(layout.routing().weight(k / width, k % width)))
                .collect(),
            linked,
            synced,
            rate,
            cap: cap.get(),
            max: layout.max_delay().get(),
            own,
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        for line in &mut self.lines {
            line.reset();
        }
        self.taps.fill(F::ZERO);
    }

    /// `secs` as a delay position: clamped into `0..=max_delay` and into the line. A time
    /// that is not finite is no delay.
    fn position(&self, secs: Seconds) -> ReadPosition {
        position_of(secs.finite_or(Seconds::ZERO), self.max, self.rate, self.cap)
    }

    /// Aims `ramps` at `controls`: a time past the longest reads as the longest and one that
    /// is not finite as none, and a feedback that is not finite as none (the pair is
    /// bounded where it is read, [`Feedback::stable_pair`]). A synced delay has no time
    /// control: its times are aimed by [`retarget_tempo`](Self::retarget_tempo). No allocation.
    pub fn retarget(&self, ramps: &mut DelayRamps<F>, controls: &DelayControls) {
        if self.synced.is_none() {
            let first = self.position(controls.time);
            for (c, time) in ramps.times.iter_mut().enumerate().take(self.lines.len()) {
                let to = if c == 0 || self.linked {
                    first
                } else {
                    self.position(self.own[c])
                };
                time.retarget_position(to, Steps::Rate(DELAY_SLEW));
            }
        }
        let steps = Steps::Fixed(ramps.steps);
        let fb = F::from_unit(controls.feedback.finite_or(Feedback::NONE));
        let cross = if self.routing.is_none() {
            F::ZERO
        } else {
            F::from_unit(controls.cross_feedback.finite_or(Feedback::NONE))
        };
        ramps.feedback.retarget_by(fb, steps);
        ramps.cross.retarget_by(cross, steps);
    }

    /// Aims the time of each channel of a synced delay at its note's seconds at `tempo`:
    /// `beats * 60 / bpm`, clamped into the line, reached at no more than [`DELAY_SLEW`]
    /// frames of delay per frame from where the time is. A delay timed in seconds is not
    /// touched. A tempo sent again leaves the ramps as they are. No allocation.
    pub fn retarget_tempo(&self, ramps: &mut DelayRamps<F>, tempo: Bpm) {
        let Some(notes) = &self.synced else { return };
        for (c, time) in ramps.times.iter_mut().enumerate().take(self.lines.len()) {
            let secs = notes.of_channel(c).to_beat_duration().to_seconds(tempo);
            time.retarget_position(self.position(secs), Steps::Rate(DELAY_SLEW));
        }
    }

    /// Runs `input` into `output`, frame `i` at the ramps' step `taken + 1 + i`, then
    /// takes the block's steps from them.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's width,
    /// or past the narrower of the two views, is not touched. A time control signal frame is clamped
    /// into the line, and one that is not finite is the ramped delay; a feedback control signal frame
    /// is clamped into the stable range, and one that is not finite is the ramped feedback.
    /// No allocation.
    pub fn process(
        &mut self,
        ramps: &mut DelayRamps<F>,
        signals: DelaySignals<'_, F>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .lines
            .len()
            .min(input.channels())
            .min(output.channels());
        let xs: [&[F]; MAX_DELAY_CHANNELS] = core::array::from_fn(|c| {
            if c < width {
                &input.plane(c).samples()[..frames]
            } else {
                &[]
            }
        });
        let mut outs = output.iter_mut();
        let mut ys: [&mut [F]; MAX_DELAY_CHANNELS] = core::array::from_fn(|c| {
            let plane = if c < width { outs.next() } else { None };
            plane.map_or_else(Default::default, |p| &mut p.into_samples_mut()[..frames])
        });
        let g = *ramps;
        let bound = F::from_unit(Feedback::MAX_STABLE);
        // A synced delay has no time control: a driven one is not read.
        let time = signals
            .time
            .filter(|_| self.synced.is_none())
            .map(|l| l.samples());
        let feedback = signals.feedback.map(|l| l.samples());
        let cf = g.cross.now();
        let coupled = !self.routing.is_none() && (cf != F::ZERO || g.cross.target() != F::ZERO);
        let moving = g.times[..width]
            .iter()
            .any(|time| time.blended_frames(frames) > 0)
            || g.feedback.blended_frames(frames) > 0
            || g.cross.blended_frames(frames) > 0;
        // The step of frame `i` of the block: the one after the steps taken.
        let step = |taken: u32, i: usize| {
            taken
                .saturating_add(1)
                .saturating_add(u32::try_from(i).unwrap_or(u32::MAX))
        };
        // The ramped value at frame `i` of the block.
        let at = |ramp: &Ramp<F>, i: usize| ramp.at(step(ramp.steps_taken(), i));
        let at_step = |ramp: &Ramp<ReadPosition>, i: usize| ramp.at(step(ramp.steps_taken(), i));
        let linked = self.linked;
        // The ramp channel `c` reads: channel 0's, for a channel that follows it.
        let ramp_of = |c: usize| &g.times[if c == 0 || linked { 0 } else { c }];
        let (rate, cap) = (self.rate, self.cap);
        // The rate crosses into the data's float once per block.
        let signal_rate = F::from_unit(rate);
        let line_end = ReadPosition::from_frames(Samples::new(cap));
        if time.is_none() && feedback.is_none() && !moving {
            // Nothing moves and nothing is driven: every control is a constant of the loop,
            // the common case and the one worth keeping tight.
            let (fb, cf) = stable_pair(g.feedback.target(), g.cross.target(), bound);
            self.run(
                &xs[..width],
                &mut ys[..width],
                frames,
                coupled,
                |_| (fb, cf),
                |c, _| ramp_of(c).target(),
            );
        } else {
            // A feedback control signal is clamped, and a frame that is not finite is the ramped
            // feedback; self- and cross-feedback feed the same loop, so they are bounded as
            // a pair whatever drives them.
            let gain = |i: usize| -> (F, F) {
                let ramped = at(&g.feedback, i);
                let fb = feedback.and_then(|s| s.get(i)).map_or(ramped, |&x| {
                    if x.is_finite() { x } else { ramped }.clamp(F::ZERO, bound)
                });
                stable_pair(fb, at(&g.cross, i), bound)
            };
            let delay_at = |c: usize, i: usize| -> ReadPosition {
                // A control signal value is seconds in the audio's own float: frames in that float, then
                // a position by the one data-to-position direction. A time driven by a control signal has the
                // control signal's own precision; a held time keeps the exact `ReadPosition::of` path.
                match time.and_then(|s| s.get(i)) {
                    Some(&x) if x.is_finite() => {
                        F::offset_position(ReadPosition::START, x * signal_rate).min(line_end)
                    }
                    _ => at_step(ramp_of(c), i),
                }
            };
            self.run(
                &xs[..width],
                &mut ys[..width],
                frames,
                coupled,
                gain,
                delay_at,
            );
        }
        ramps.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }

    /// The block, `gain(i)` the bounded `(fb, cf)` pair at frame `i` and `delay_at(c, i)`
    /// channel `c`'s delay. Constant closures compile down to constants.
    #[inline(always)]
    fn run(
        &mut self,
        xs: &[&[F]],
        ys: &mut [&mut [F]],
        frames: usize,
        coupled: bool,
        gain: impl Fn(usize) -> (F, F),
        delay_at: impl Fn(usize, usize) -> ReadPosition,
    ) {
        let interp = self.interpolation;
        let width = xs.len();
        if !coupled {
            // Independent channels: channel-outer, one line at a time.
            for (c, line) in self.lines.iter_mut().enumerate().take(width) {
                for (i, (y, x)) in ys[c][..frames].iter_mut().zip(xs[c]).enumerate() {
                    let (fb, _) = gain(i);
                    *y = delay_step(line, *x, delay_at(c, i), fb, interp);
                }
            }
            return;
        }
        // Cross-fed: every channel's feedback tap is read before any line is written, so the
        // loop is frame-outer.
        for i in 0..frames {
            let (fb, cf) = gain(i);
            for (c, line) in self.lines.iter().enumerate().take(width) {
                let d = delay_at(c, i);
                self.taps[c] = line.read_sample(d.saturating_sub(Samples::new(1)), interp);
            }
            for (c, line) in self.lines.iter_mut().enumerate().take(width) {
                let mut acc = xs[c][i] + self.taps[c] * fb;
                for (&w, &tap) in self.weights[c * width..(c + 1) * width]
                    .iter()
                    .zip(&self.taps)
                {
                    if w != F::ZERO {
                        acc += w * cf * tap;
                    }
                }
                line.push_sample(acc);
            }
            for (c, line) in self.lines.iter().enumerate().take(width) {
                ys[c][i] = line.read_sample(delay_at(c, i), interp);
            }
        }
    }
}

#[cfg(test)]
mod tests;
