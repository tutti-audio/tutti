//! The chorus and the flanger: a swept delay of any width, [`ModDelayBank`], and what it is
//! built to ([`ModDelayLayout`]) and told ([`ModDelayControls`]).
//!
//! One [`DelayLine`] per channel, each read at a delay an LFO moves around a base, with
//! feedback. The output is the delayed signal alone, the **wet** signal: the blend with the
//! dry input is the graph's mix. A [`Sweep`] says what it is built to (a chorus sits at a
//! long base delay, a flanger at a short one), a [`Stagger`] how the channels' sweeps are
//! offset, and the controls are three: the LFO's rate, the sweep's depth and the feedback.
//!
//! The bank is the kernel: it runs a block through sweep excursions that are [`Glide`]s, one
//! per channel, around its base delay (a [`ReadPosition`], so a long base keeps its fraction),
//! and a feedback glide. **The LFO is the caller's**, on the stream's clock: it solves the
//! corners of a segment (each channel's excursion at the segment's start and at its end, [`COEFF_INTERVAL`](crate::COEFF_INTERVAL) frames on, [`swept`](ModDelayBank::swept))
//! and aims each channel's glide across it, so the cut of the blocks does not move the
//! render. The chord of a sine over a step of `h` radians misses it by at most `h^2 / 8`,
//! which at 5 Hz and 48 kHz is 1.4e-5 of the depth (0.03 samples against the deepest chorus,
//! 40 ms), and it kinks the delay's slope at each corner, a small ripple on the pitch
//! modulation.
//!
//! **A moving depth is a pitch bend**, so its speed is bounded, not its duration: the caller
//! glides a depth at most [`DELAY_SLEW`](crate::DELAY_SLEW) frames of delay per frame
//! ([`design`](ModDelayBank::design) gives the depth as a span of frames).
//!
//! The delay has **no latency**: the sweep is the effect, not processing delay, and PDC must
//! not delay the rest of the mix to line up with it.
//!
//! One frame of a channel is [`delay_step`], the delay's loop: the feedback tap is read one
//! frame nearer than the delay **before** the push and the output at the delay **after** it,
//! so the loop's period is exactly the delay and the first echo is at the delay too: a comb
//! `1 / (1 - g z^-d)`.
//!
//! The memory is the sweep's own [`MaxDelay`]: a line holds that long, and `base +
//! depth_max` fits in it by [`Sweep::new`]. The state is the bank's float `F`; a view of
//! another [`Float`] is converted at the edges.

use tutti_buffers::{BusTopology, MAX_CHANNELS};
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{
    Feedback, Gain, Hz, PhaseIncrement, ReadPosition, SampleRate, SampleSpan, Samples, Seconds,
};

use super::sweep::{Stagger, StaggerError, Sweep};
use crate::finite::finite_or;
use crate::{DelayLine, Glide, InterpolationMode, Lerp, MaxDelay, delay_step};

/// The LFO's slowest rate: 0.01 Hz, a lane's value and a cell's alike. A rate below it
/// (or negative) reads as it.
pub const MIN_RATE: Hz = Hz(0.01);

/// The most channels a swept delay has: a bus's.
pub const MAX_MOD_DELAY_CHANNELS: usize = MAX_CHANNELS;

/// The reads are linear: no stepping under modulation, at the cost of a little damping of
/// the highs (the sweep's interpolation is not a control).
const READ: InterpolationMode = InterpolationMode::Linear;

/// The controls of a swept delay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayControls {
    /// The LFO's rate: how fast the delay sweeps. Chorus and flanger rates are slow,
    /// typically 0.1 to 2 Hz; faster reads as vibrato. **Floored at 0.01 Hz**, a lane's
    /// value and a cell's alike ([`MIN_RATE`]). Not finite reads as the default's.
    pub rate: Hz,
    /// How far the delay moves either side of the base, in seconds of delay (not a
    /// fraction: it is added to the base delay, unlike the phaser's unitless depth).
    /// Clamped into the sweep's range. Not finite reads as the default's.
    pub depth: Seconds,
    /// How much of the delayed signal recirculates; on a flanger the most characterful
    /// control, since more sharpens the comb notches into a resonant sweep. Clamped into
    /// `0..=`[`feedback_max`](Sweep::feedback_max). Not finite reads as the default's.
    pub feedback: Feedback,
}

impl ModDelayControls {
    /// No sweep, no recirculation, at 1 Hz: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        rate: Hz(1.0),
        depth: Seconds::ZERO,
        feedback: Feedback::NONE,
    };

    /// Chorus: 1 Hz, a 5 ms sweep, 0.3 feedback (for [`Sweep::CHORUS`]).
    pub const CHORUS: Self = Self {
        rate: Hz(1.0),
        depth: Seconds(0.005),
        feedback: Feedback(0.3),
    };

    /// Flanger: 0.5 Hz, a 2 ms sweep, 0.7 feedback (for [`Sweep::FLANGER`]).
    pub const FLANGER: Self = Self {
        rate: Hz(0.5),
        depth: Seconds(0.002),
        feedback: Feedback(0.7),
    };

    /// The General MIDI chorus: 0.4 Hz, a 1.9 ms sweep, no feedback (for
    /// [`Sweep::GENERAL_MIDI_CHORUS`]).
    pub const GENERAL_MIDI_CHORUS: Self = Self {
        rate: Hz(0.4),
        depth: Seconds(0.0019),
        feedback: Feedback::NONE,
    };
}

impl Default for ModDelayControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What [`ModDelayBank::design`] makes of the controls at a rate: the values the sweep runs
/// at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepDesign {
    /// The LFO's step per frame, in cycles.
    pub inc: PhaseIncrement,
    /// How far the delay moves either side of the base, in frames, inside the sweep's
    /// range.
    pub depth: SampleSpan,
    /// The feedback, inside `0..=`[`feedback_max`](Sweep::feedback_max).
    pub feedback: Feedback,
}

/// What a swept delay is built to: its bus, the [`Sweep`] and the [`Stagger`] of its
/// channels.
///
/// A chorus is [`ModDelayLayout::chorus`] and a flanger [`flanger`](Self::flanger): the same
/// layout at different numbers. [`general_midi_chorus`](Self::general_midi_chorus) is the
/// chorus send of a General MIDI synthesizer, with no feedback.
#[derive(Clone, Copy, Debug)]
pub struct ModDelayLayout {
    input: BusTopology,
    sweep: Sweep,
    stagger: Stagger,
}

impl ModDelayLayout {
    /// A swept delay on the bus `input`, built to `sweep`, its channels' sweeps offset by
    /// `stagger`.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel
    /// ([`Stagger::each`]) and not for the `input`'s.
    pub fn new(input: BusTopology, sweep: Sweep, stagger: Stagger) -> Result<Self, StaggerError> {
        let width = usize::from(input.layout().count());
        match stagger.channels() {
            Some(got) if got != width => Err(StaggerError::Length { width, got }),
            _ => Ok(Self {
                input,
                sweep,
                stagger,
            }),
        }
    }

    /// A chorus: [`Sweep::CHORUS`], channel `c` a quarter cycle of the LFO behind channel
    /// `c - 1`. Sweeping the channels out of step is what makes it feel wide.
    #[must_use]
    pub fn chorus(input: BusTopology) -> Self {
        Self::preset(input, Sweep::CHORUS, PhaseIncrement(0.25))
    }

    /// A flanger: [`Sweep::FLANGER`], channels a half cycle apart.
    #[must_use]
    pub fn flanger(input: BusTopology) -> Self {
        Self::preset(input, Sweep::FLANGER, PhaseIncrement(0.5))
    }

    /// The General MIDI chorus: [`Sweep::GENERAL_MIDI_CHORUS`], channels a quarter cycle
    /// apart, no feedback.
    #[must_use]
    pub fn general_midi_chorus(input: BusTopology) -> Self {
        Self::preset(input, Sweep::GENERAL_MIDI_CHORUS, PhaseIncrement(0.25))
    }

    fn preset(input: BusTopology, sweep: Sweep, step: PhaseIncrement) -> Self {
        Self {
            input,
            sweep,
            stagger: Stagger::of_step(step),
        }
    }

    /// The bus it runs on.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.layout().count())
    }

    /// What the delay is swept around and how far.
    #[must_use]
    pub const fn sweep(&self) -> &Sweep {
        &self.sweep
    }

    /// The longest delay a line holds.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.sweep.max_delay()
    }

    /// Each channel's LFO phase offset (channel 0's is zero), wrapped, for the first
    /// [`width`](Self::width) channels and zero after.
    #[must_use]
    pub fn offsets(&self) -> [PhaseIncrement; MAX_MOD_DELAY_CHANNELS] {
        let mut offsets = [PhaseIncrement(0.0); MAX_MOD_DELAY_CHANNELS];
        let layout = self.input.layout();
        for (c, o) in (0..self.width())
            .filter_map(|c| layout.index(c))
            .zip(&mut offsets)
        {
            *o = self.stagger.offset(c);
        }
        offsets
    }
}

/// A swept delay of any width: a [`DelayLine`] per channel in the state float `F`, and the
/// loop that recirculates each.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps their
/// size, and [`reset`](Self::reset) zeroes them without allocating.
#[derive(Clone, Debug)]
pub struct ModDelayBank<F: Float> {
    lines: Box<[DelayLine<F>]>,
    sweep: Sweep,
    rate: SampleRate,
    /// The line's capacity in frames: no delay is past it.
    cap: ReadPosition,
    /// The centre delay, inside the line.
    base: ReadPosition,
}

impl<F: Float + Lerp> ModDelayBank<F> {
    /// A silent swept delay of `layout` at `rate`: a line per channel of
    /// `ceil(max_delay * rate)` frames. Allocates: build it on the control side.
    #[must_use]
    pub fn new(layout: &ModDelayLayout, rate: SampleRate) -> Self {
        let cap = layout.max_delay().frames(rate);
        let cap_position = ReadPosition::from_frames(cap);
        let base = ReadPosition::of(layout.sweep.base(), rate).min(cap_position);
        Self {
            lines: (0..layout.width()).map(|_| DelayLine::new(cap)).collect(),
            sweep: layout.sweep,
            rate,
            cap: cap_position,
            base,
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lines.len()
    }

    /// The line's longest delay at the prepared rate.
    #[must_use]
    pub fn max_delay(&self) -> Samples {
        self.cap.frames()
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        for line in &mut self.lines {
            line.reset();
        }
    }

    /// The controls as the sweep runs them: a rate below 0.01 Hz is 0.01 Hz, a depth outside
    /// the sweep's range is clamped into it and a feedback into `0..=feedback_max`; a control
    /// that is not finite is the default's.
    #[must_use]
    pub fn design(&self, controls: &ModDelayControls) -> SweepDesign {
        let d = ModDelayControls::DEFAULT;
        let hz = finite_or(controls.rate.get(), d.rate.get()).max(MIN_RATE.get());
        let depth = if controls.depth.get().is_finite() {
            controls.depth
        } else {
            d.depth
        };
        let depth = depth
            .get()
            .clamp(self.sweep.depth_min().get(), self.sweep.depth_max().get());
        let depth = SampleSpan::from_seconds(Seconds(depth), self.rate)
            .unwrap_or(SampleSpan::ZERO)
            .min(SampleSpan::from_samples(self.cap.frames()));
        let feedback = finite_or(controls.feedback.get(), d.feedback.get())
            .max(0.0)
            .min(self.sweep.feedback_max().get());
        SweepDesign {
            inc: PhaseIncrement::per_sample(Hz(hz), self.rate),
            depth,
            feedback: Feedback(feedback),
        }
    }

    /// How far the read is from the base with the LFO at `lfo` (in `-1..=1`) and the sweep
    /// `depth` frames deep: `lfo * depth` frames, held so that the read stays inside the line.
    /// An excursion, not a delay: it can reach behind the newest write (a depth past the
    /// base), and [`position`](Self::position) floors the delay a frame reads at one frame (the
    /// feedback tap is read one before it), so a sweep that bottoms out flat-bottoms there
    /// whatever the chord between corners does. It is small beside the delay, so it is held
    /// in the bank's float and the base stays the exact [`ReadPosition`] it is added to.
    #[must_use]
    pub fn swept(&self, lfo: Gain, depth: SampleSpan) -> F {
        let room = self.cap.span().get() - self.base.span().get();
        F::from_f64((lfo.get() * depth.get()).min(room))
    }

    /// The delay a read takes with the sweep `excursion` frames from the base (what
    /// [`swept`](Self::swept) gives, glided between corners): the base moved by it exactly
    /// ([`Float::offset_position`]), held between one frame and the line's length.
    #[inline]
    #[must_use]
    pub fn position(&self, excursion: F) -> ReadPosition {
        read_at(self.base, self.cap, excursion)
    }

    /// Runs `input` into `output`, frame `i` at the excursion `delays[c].at(taken + 1 + i)` for
    /// channel `c` ([`position`](Self::position) of it) and `feedback.at(taken + 1 + i)`, then
    /// takes the block's steps from the glides. Each delay is floored at one frame where it is
    /// read.
    ///
    /// Each channel is [`delay_step`] at its delay: the line's own tap scaled by the feedback
    /// added to the input. Channel `c` of `input` feeds channel `c` of `output`; a channel
    /// past the bank's width, or past the narrower of the two views, is not touched. The
    /// feedback glide's values must stay below 1 ([`Feedback::MAX_STABLE`] is the bound
    /// [`design`](Self::design) holds them to). No allocation.
    pub fn process<S: Float>(
        &mut self,
        delays: &mut [Glide<F>],
        feedback: &mut Glide<F>,
        input: Planes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let run = self
            .lines
            .len()
            .min(input.channels())
            .min(output.channels())
            .min(delays.len());
        let fb_first = feedback.taken().saturating_add(1);
        let (base, cap) = (self.base, self.cap);
        for (c, ((line, delay), out)) in self
            .lines
            .iter_mut()
            .zip(delays.iter())
            .zip(output.iter_mut())
            .enumerate()
            .take(run)
        {
            let first = delay.taken().saturating_add(1);
            let xs = &input.plane(c).samples()[..frames];
            let ys = &mut out.into_samples_mut()[..frames];
            for (i, (y, x)) in ys.iter_mut().zip(xs).enumerate() {
                let step = u32::try_from(i).unwrap_or(u32::MAX);
                let d = read_at(base, cap, delay.at(first.saturating_add(step)));
                let g = feedback.at(fb_first.saturating_add(step));
                let wet = delay_step(line, x.convert_to::<F>(), d, g, READ);
                *y = wet.convert_to::<S>();
            }
        }
        let taken = u32::try_from(frames).unwrap_or(u32::MAX);
        for glide in delays.iter_mut().take(run) {
            glide.advance(taken);
        }
        feedback.advance(taken);
    }
}

/// The delay of a read `excursion` frames from `base`, held between one frame and `cap`.
#[inline(always)]
fn read_at<F: Float>(base: ReadPosition, cap: ReadPosition, excursion: F) -> ReadPosition {
    F::offset_position(base, excursion)
        .max(ReadPosition::from_frames(Samples::new(1)))
        .min(cap)
}

#[cfg(test)]
mod tests;
