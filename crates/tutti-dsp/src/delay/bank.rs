//! [`DelayBank`]: one feedback delay line per channel, over buffers views, with the
//! delay and the feedbacks it reads as [`Glide`]s.
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
//! ([`ChannelTimes::each`](crate::ChannelTimes::each)). A
//! lane drives the time and the feedback **per frame** ([`DelayLanes`]): a delay time
//! moved at audio rate is a flanger, a lane frame that is not finite falls back to the
//! glided value, and a lane frame is the trajectory itself, not glided. The time glides at
//! most [`DELAY_SLEW`] frames of delay per frame (a moving delay is a pitch bend, so its
//! speed is bounded and not its duration), the feedback and the cross-feedback over a
//! length the caller sets.
//!
//! The delay has **no latency**: the echo is the effect, not processing delay.

use tutti_buffers::{Float, Mono, Planes, PlanesMut, Sample};
use tutti_types::{Feedback, Gain, SampleRate, SampleSpan, Samples, Seconds};

use super::layout::{DELAY_SLEW, DelayControls, DelayLayout, MAX_DELAY_CHANNELS};
use super::routing::Routing;
use super::taps::finite_or;
use crate::{DelayLine, Glide, InterpolationMode, Steps, delay_step};

/// What a delay reads while it runs: the delay in frames, and the two feedbacks, each
/// aimed by [`DelayBank::aim`] and stepped by [`DelayBank::process`].
///
/// The time glides at [`DELAY_SLEW`] frames of delay per frame; the channels past the
/// first that hold a time of their own hold it whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayGlides {
    time: Glide<f32>,
    own: [f32; MAX_DELAY_CHANNELS],
    feedback: Glide<f32>,
    cross: Glide<f32>,
    /// The steps a feedback glide takes.
    steps: u32,
}

impl DelayGlides {
    /// A delay at rest on no delay and no feedback, whose feedbacks take `gain_steps`
    /// steps to a new value.
    #[must_use]
    pub fn landed(gain_steps: u32) -> Self {
        Self {
            time: Glide::landed(0.0),
            own: [0.0; MAX_DELAY_CHANNELS],
            feedback: Glide::landed(0.0),
            cross: Glide::landed(0.0),
            steps: gain_steps,
        }
    }

    /// Lands on the targets now.
    pub fn settle(&mut self) {
        self.time.settle();
        self.feedback.settle();
        self.cross.settle();
    }

    /// Takes `frames` steps of each glide.
    fn advance(&mut self, frames: u32) {
        self.time.advance(frames);
        self.feedback.advance(frames);
        self.cross.advance(frames);
    }
}

/// `secs` as a delay in frames: clamped into `0..=max` and into the line of `cap`
/// frames. The delay kernel reads an `f32` position per frame, so the span narrows here,
/// once per frame, after the clamp.
#[inline(always)]
fn frames_of(secs: f32, max: Seconds, rate: SampleRate, cap: usize) -> f32 {
    let secs = Seconds::from_f32(secs.clamp(0.0, max.to_f32()));
    // The clamp leaves `0..=max` (finite), so a span exists; a NaN time, which
    // the callers never pass, is no delay.
    let Some(span) = SampleSpan::from_seconds(secs, rate) else {
        return 0.0;
    };
    span.min(SampleSpan::from_samples(Samples::new(cap))).get() as f32
}

/// The per-frame lanes a delay reads beside its audio, in the audio's sample format, each
/// `None` where nothing drives it. A lane is at least as long as the frames a call renders;
/// a frame past its end is not driven.
#[derive(Clone, Copy, Debug, Default)]
pub struct DelayLanes<'a, S: Float = f32> {
    /// The delay time in seconds, of every channel.
    pub time: Option<Mono<'a, S>>,
    /// The feedback.
    pub feedback: Option<Mono<'a, S>>,
}

/// A feedback delay of any width: a [`DelayLine`] per channel, and the loop that
/// recirculates them.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps
/// their size, and [`reset`](Self::reset) zeroes them without allocating.
#[derive(Clone, Debug)]
pub struct DelayBank {
    lines: Box<[DelayLine]>,
    /// Each channel's feedback tap of the frame, read before any line is written (the
    /// coupled loop's).
    taps: Box<[f32]>,
    interpolation: InterpolationMode,
    routing: Routing,
    linked: bool,
    rate: SampleRate,
    /// The line's capacity in frames: the delay is clamped into it.
    cap: usize,
    /// The longest delay.
    max: Seconds,
    /// The times of the channels past the first that hold their own, in seconds.
    own: [Seconds; MAX_DELAY_CHANNELS],
}

impl DelayBank {
    /// A silent delay of `layout` at `rate`: a line per channel of `ceil(max_delay *
    /// rate)` frames. Allocates: build it on the control side.
    #[must_use]
    pub fn new(layout: &DelayLayout, rate: SampleRate) -> Self {
        let width = layout.width();
        let cap = layout.max_delay().frames(rate);
        let mut own = [Seconds::ZERO; MAX_DELAY_CHANNELS];
        for (slot, t) in own.iter_mut().skip(1).zip(layout.times().fixed()) {
            *slot = *t;
        }
        Self {
            lines: (0..width).map(|_| DelayLine::new(cap)).collect(),
            taps: vec![0.0; width].into_boxed_slice(),
            interpolation: layout.interpolation(),
            routing: *layout.routing(),
            linked: layout.times().is_linked(),
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
        self.taps.fill(0.0);
    }

    /// `secs` as a delay in frames: clamped into `0..=max_delay` and into the line. A time
    /// that is not finite is no delay.
    fn frames(&self, secs: f32) -> f32 {
        frames_of(finite_or(secs, 0.0), self.max, self.rate, self.cap)
    }

    /// Aims `glides` at `controls`: a time past the longest reads as the longest and one that
    /// is not finite as none, and a feedback that is not finite as none (the pair is
    /// bounded where it is read, [`Feedback::stable_pair`]). No allocation.
    pub fn aim(&self, glides: &mut DelayGlides, controls: &DelayControls) {
        let first = self.frames(controls.time.to_f32());
        glides.time.aim_by(first, Steps::Rate(DELAY_SLEW));
        for (c, own) in glides.own.iter_mut().enumerate() {
            *own = if c == 0 || self.linked {
                first
            } else {
                self.frames(self.own[c].to_f32())
            };
        }
        let steps = Steps::Fixed(glides.steps);
        let fb = finite_or(controls.feedback.get(), 0.0);
        let cross = if self.routing.is_none() {
            0.0
        } else {
            finite_or(controls.cross_feedback.get(), 0.0)
        };
        glides.feedback.aim_by(fb, steps);
        glides.cross.aim_by(cross, steps);
    }

    /// Runs `input` into `output`, frame `i` at the glides' step `taken + 1 + i`, then
    /// takes the block's steps from them.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the bank's width,
    /// or past the narrower of the two views, is not touched. A time lane frame is clamped
    /// into the line, and one that is not finite is the glided delay; a feedback lane frame
    /// is clamped into the stable range, and one that is not finite is the glided feedback.
    /// No allocation.
    pub fn process<S: Float>(
        &mut self,
        glides: &mut DelayGlides,
        lanes: DelayLanes<'_, S>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = self
            .lines
            .len()
            .min(input.channels())
            .min(output.channels());
        let xs: [&[S]; MAX_DELAY_CHANNELS] = core::array::from_fn(|c| {
            if c < width {
                &input.plane(c).samples()[..frames]
            } else {
                &[]
            }
        });
        let mut outs = output.iter_mut();
        let mut ys: [&mut [S]; MAX_DELAY_CHANNELS] = core::array::from_fn(|c| {
            let plane = if c < width { outs.next() } else { None };
            plane.map_or_else(Default::default, |p| &mut p.into_samples_mut()[..frames])
        });
        let g = *glides;
        let (time, feedback) = (
            lanes.time.map(|l| l.samples()),
            lanes.feedback.map(|l| l.samples()),
        );
        let cf = g.cross.now();
        let coupled = !self.routing.is_none() && (cf != 0.0 || g.cross.target() != 0.0);
        let moving = g.time.blended(frames) > 0
            || g.feedback.blended(frames) > 0
            || g.cross.blended(frames) > 0;
        // The glided value at frame `i` of the block: the step after the ones taken.
        let at = |glide: &Glide<f32>, i: usize| {
            let k = glide
                .taken()
                .saturating_add(1)
                .saturating_add(u32::try_from(i).unwrap_or(u32::MAX));
            glide.at(k)
        };
        let linked = self.linked;
        let (rate, max, cap) = (self.rate, self.max, self.cap);
        let lane_frames = |secs: f32| frames_of(secs, max, rate, cap);
        let base = |c: usize, delay: f32| {
            if c == 0 || linked { delay } else { g.own[c] }
        };
        if time.is_none() && feedback.is_none() && !moving {
            // Nothing moves and nothing is driven: every control is a constant of the loop,
            // the common case and the one worth keeping tight.
            let (fb, cf) = Feedback::stable_pair(g.feedback.target(), g.cross.target());
            let (fb, cf) = (fb.get(), cf.get());
            let delay = g.time.target();
            self.run(
                &xs[..width],
                &mut ys[..width],
                frames,
                coupled,
                |_| (fb, cf),
                |c, _| base(c, delay),
            );
        } else {
            // A feedback lane is clamped, and a frame that is not finite is the glided
            // feedback; self- and cross-feedback feed the same loop, so they are bounded as
            // a pair whatever drives them.
            let gain = |i: usize| -> (f32, f32) {
                let glided = at(&g.feedback, i);
                let fb = feedback.and_then(|s| s.get(i)).map_or(glided, |x| {
                    Feedback::new_clamped(finite_or(x.convert_to::<f32>(), glided)).get()
                });
                let (fb, cf) = Feedback::stable_pair(fb, at(&g.cross, i));
                (fb.get(), cf.get())
            };
            let delay_at = |c: usize, i: usize| -> f32 {
                match time.and_then(|s| s.get(i)).map(|x| x.convert_to::<f32>()) {
                    Some(s) if s.is_finite() => lane_frames(s),
                    _ => base(c, at(&g.time, i)),
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
        glides.advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }

    /// The block, `gain(i)` the bounded `(fb, cf)` pair at frame `i` and `delay_at(c, i)`
    /// channel `c`'s delay in frames. Constant closures compile down to constants.
    #[inline(always)]
    fn run<S: Float>(
        &mut self,
        xs: &[&[S]],
        ys: &mut [&mut [S]],
        frames: usize,
        coupled: bool,
        gain: impl Fn(usize) -> (f32, f32),
        delay_at: impl Fn(usize, usize) -> f32,
    ) {
        let interp = self.interpolation;
        let width = xs.len();
        if !coupled {
            // Independent channels: channel-outer, one line at a time.
            for (c, line) in self.lines.iter_mut().enumerate().take(width) {
                for (i, (y, x)) in ys[c][..frames].iter_mut().zip(xs[c]).enumerate() {
                    let (fb, _) = gain(i);
                    let out = delay_step(line, x.convert_to::<f32>(), delay_at(c, i), fb, interp);
                    *y = out.convert_to::<S>();
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
                self.taps[c] = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
            }
            for (c, line) in self.lines.iter_mut().enumerate().take(width) {
                let mut acc = xs[c][i].convert_to::<f32>() + self.taps[c] * fb;
                for j in 0..width {
                    let w = self.routing.weight(c, j);
                    if w != Gain::SILENT {
                        acc += w.apply(cf) * self.taps[j];
                    }
                }
                line.push_sample(acc);
            }
            for (c, line) in self.lines.iter().enumerate().take(width) {
                ys[c][i] = line.read_sample(delay_at(c, i), interp).convert_to::<S>();
            }
        }
    }
}

#[cfg(test)]
mod tests;
