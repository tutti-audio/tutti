//! The chorus and the flanger: a swept delay of any width, [`ModDelayBank`], and what it is
//! built to ([`ModDelayLayout`]) and told ([`ModDelayControls`]).
//!
//! One [`DelayLine`] per channel, each read at a delay an LFO moves around a base, with
//! feedback. The output is the delayed signal alone, the **wet** signal: the blend with the
//! dry input is the graph's mix. A [`Sweep`] says what it is built to (a chorus sits at a
//! long base delay, a flanger at a short one), a [`Stagger`] how the channels' sweeps are
//! offset, and the controls are three: the LFO's rate, the sweep's depth and the feedback.
//!
//! The bank is the kernel: it runs a block through sweep excursions that are [`Ramp`]s, one
//! per channel, around its base delay (a [`ReadPosition`], so a long base keeps its fraction),
//! and a feedback ramp. **The LFO is the caller's**, on the stream's clock: it solves the
//! corners of a segment (each channel's excursion at the segment's start and at its end, [`COEFF_INTERVAL`](crate::COEFF_INTERVAL) frames on, [`swept`](ModDelayBank::swept))
//! and retargets each channel's ramp across it, so the cut of the blocks does not move the
//! render. The chord of a sine over a step of `h` radians misses it by at most `h^2 / 8`,
//! which at 5 Hz and 48 kHz is 1.4e-5 of the depth (0.03 samples against the deepest chorus,
//! 40 ms), and it kinks the delay's slope at each corner, a small ripple on the pitch
//! modulation.
//!
//! **A moving depth is a pitch bend**, so its speed is bounded, not its duration: the caller
//! ramps a depth at most [`DELAY_SLEW`](crate::DELAY_SLEW) frames of delay per frame
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
//! depth_max` fits in it by [`SweepBuilder::build`](crate::SweepBuilder::build). The state is the bank's float `F`.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelTopology, MAX_CHANNELS};
use tutti_buffers::{Float, InputOutput};
use tutti_units::{
    Bipolar, Feedback, Hz, PhaseIncrement, ReadPosition, SampleRate, SampleSpan, Samples, Seconds,
};

use super::sweep::{Stagger, StaggerError, Sweep};
use crate::{DelayLine, InterpolationMode, Lerp, MaxDelay, Ramp, delay_step};

/// The LFO's slowest rate: 0.01 Hz, a control signal's value and a cell's alike. A rate below it
/// (or negative) reads as it.
pub const MIN_RATE: Hz = Hz::new(0.01);

tutti_units::ranged! {
    /// An LFO rate a sweep is built at: finite and at least [`MIN_RATE`].
    ModulationRate(Hz), ModulationRateError, "a finite rate of at least 0.01 Hz",
    |raw| raw.get().is_finite() && raw.get() >= MIN_RATE.get()
}

/// The most channels a swept delay has: a bus's.
pub const MAX_MOD_DELAY_CHANNELS: usize = MAX_CHANNELS;

/// The reads are linear: no stepping under modulation, at the cost of a little damping of
/// the highs (the sweep's interpolation is not a control).
const READ: InterpolationMode = InterpolationMode::Linear;

/// The controls of a swept delay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayControls {
    /// The LFO's rate: how fast the delay sweeps. Chorus and flanger rates are slow,
    /// typically 0.1 to 2 Hz; faster reads as vibrato. **Floored at 0.01 Hz**, a control signal's
    /// value and a cell's alike ([`MIN_RATE`]). Not finite reads as the default's.
    pub rate: Hz,
    /// How far the delay moves either side of the base, in seconds of delay (not a
    /// fraction: it is added to the base delay, unlike the phaser's unitless depth).
    /// Clamped into the sweep's range. Not finite reads as the default's.
    /// [`Sweep::vibrato_controls`] refuses a depth the sweep cannot reach instead.
    pub depth: Seconds,
    /// How much of the delayed signal recirculates; on a flanger the most characterful
    /// control, since more sharpens the comb notches into a resonant sweep. Clamped into
    /// `0..=`[`feedback_max`](Sweep::feedback_max). Not finite reads as the default's.
    pub feedback: Feedback,
}

impl ModDelayControls {
    /// No sweep, no recirculation, at 1 Hz: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        rate: Hz::new(1.0),
        depth: Seconds::ZERO,
        feedback: Feedback::NONE,
    };

    /// Chorus: 1 Hz, a 5 ms sweep, 0.3 feedback (for [`Sweep::CHORUS`]).
    pub const CHORUS: Self = Self {
        rate: Hz::new(1.0),
        depth: Seconds::new(0.005),
        feedback: Feedback::new(0.3),
    };

    /// Flanger: 0.5 Hz, a 2 ms sweep, 0.7 feedback (for [`Sweep::FLANGER`]).
    pub const FLANGER: Self = Self {
        rate: Hz::new(0.5),
        depth: Seconds::new(0.002),
        feedback: Feedback::new(0.7),
    };

    /// The General MIDI chorus: 0.4 Hz, a 1.9 ms sweep, no feedback (for
    /// [`Sweep::GENERAL_MIDI_CHORUS`]).
    pub const GENERAL_MIDI_CHORUS: Self = Self {
        rate: Hz::new(0.4),
        depth: Seconds::new(0.0019),
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayLayout {
    input: ChannelTopology,
    sweep: Sweep,
    stagger: Stagger,
}

impl ModDelayLayout {
    /// A [`ModDelayLayoutBuilder`] for a swept delay on the bus `input`, built to `sweep`, its
    /// channels' sweeps in step.
    #[must_use]
    pub const fn builder(input: ChannelTopology, sweep: Sweep) -> ModDelayLayoutBuilder {
        ModDelayLayoutBuilder {
            input,
            sweep,
            stagger: Stagger::NONE,
        }
    }

    /// A chorus: [`Sweep::CHORUS`], channel `c` a quarter cycle of the LFO behind channel
    /// `c - 1`. Sweeping the channels out of step is what makes it feel wide.
    #[must_use]
    pub fn chorus(input: ChannelTopology) -> Self {
        Self::preset(input, Sweep::CHORUS, PhaseIncrement::new(0.25))
    }

    /// A vibrato: [`Sweep::VIBRATO`], every channel's sweep in step (a pitch swing that moved
    /// between channels would be a chorus).
    #[must_use]
    pub fn vibrato(input: ChannelTopology) -> Self {
        Self::preset(input, Sweep::VIBRATO, PhaseIncrement::new(0.0))
    }

    /// A flanger: [`Sweep::FLANGER`], channels a half cycle apart.
    #[must_use]
    pub fn flanger(input: ChannelTopology) -> Self {
        Self::preset(input, Sweep::FLANGER, PhaseIncrement::new(0.5))
    }

    /// The General MIDI chorus: [`Sweep::GENERAL_MIDI_CHORUS`], channels a quarter cycle
    /// apart, no feedback.
    #[must_use]
    pub fn general_midi_chorus(input: ChannelTopology) -> Self {
        Self::preset(input, Sweep::GENERAL_MIDI_CHORUS, PhaseIncrement::new(0.25))
    }

    fn preset(input: ChannelTopology, sweep: Sweep, step: PhaseIncrement) -> Self {
        Self {
            input,
            sweep,
            stagger: Stagger::of_step(step),
        }
    }

    /// The bus it runs on.
    #[must_use]
    pub const fn input(&self) -> ChannelTopology {
        self.input
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.width().count())
    }

    /// What the delay is swept around and how far.
    #[must_use]
    pub const fn sweep(&self) -> &Sweep {
        &self.sweep
    }

    /// How the channels' sweeps are offset from channel 0's.
    #[must_use]
    pub const fn stagger(&self) -> &Stagger {
        &self.stagger
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
        let mut offsets = [PhaseIncrement::new(0.0); MAX_MOD_DELAY_CHANNELS];
        let layout = self.input.width();
        for (c, o) in (0..self.width())
            .filter_map(|c| layout.index(c))
            .zip(&mut offsets)
        {
            *o = self.stagger.offset(c);
        }
        offsets
    }
}

/// The terminal step of a [`ModDelayLayout`] of your own: [`build`](Self::build) checks the
/// stagger against the bus, which the option cannot see.
#[derive(Clone, Copy, Debug)]
pub struct ModDelayLayoutBuilder {
    input: ChannelTopology,
    sweep: Sweep,
    stagger: Stagger,
}

impl ModDelayLayoutBuilder {
    /// Offsets the channels' sweeps by `stagger`.
    #[must_use]
    pub const fn stagger(mut self, stagger: Stagger) -> Self {
        self.stagger = stagger;
        self
    }

    /// The layout.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when the stagger names an offset for each channel
    /// ([`Stagger::try_from`] a slice) and not for the bus's.
    pub fn build(self) -> Result<ModDelayLayout, StaggerError> {
        let Self {
            input,
            sweep,
            stagger,
        } = self;
        let width = usize::from(input.width().count());
        match stagger.channels() {
            Some(got) if got != width => Err(StaggerError::Length { width, got }),
            _ => Ok(ModDelayLayout {
                input,
                sweep,
                stagger,
            }),
        }
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
        let hz = controls.rate.finite_or(d.rate).max(MIN_RATE);
        let depth = if controls.depth.get().is_finite() {
            controls.depth
        } else {
            d.depth
        };
        let depth = depth
            .get()
            .clamp(self.sweep.depth_min().get(), self.sweep.depth_max().get());
        let depth = SampleSpan::from_seconds(Seconds::new(depth), self.rate)
            .unwrap_or(SampleSpan::ZERO)
            .min(SampleSpan::from_samples(self.cap.frames()));
        let feedback = controls
            .feedback
            .finite_or(d.feedback)
            .max(Feedback::NONE)
            .min(self.sweep.feedback_max());
        SweepDesign {
            inc: PhaseIncrement::per_sample(hz, self.rate),
            depth,
            feedback,
        }
    }

    /// How far the read is from the base with the LFO at `lfo` (in `-1..=1`) and the sweep
    /// `depth` frames deep: `lfo * depth` frames, held so that the read stays inside the line.
    /// An excursion, not a delay: it can reach behind the newest write (a depth past the
    /// base), and [`position`](Self::position) floors the delay a frame reads at one frame (the
    /// feedback tap is read one before it), so a sweep that bottoms out flat-bottoms there
    /// whatever the chord between corners does. It is small beside the delay, so it is held
    /// in the bank's float and the base stays the exact [`ReadPosition`] it is added to; the
    /// product is solved in `f64` and crosses once.
    #[must_use]
    pub fn swept(&self, lfo: Bipolar, depth: SampleSpan) -> F {
        let room = self.cap.span().get() - self.base.span().get();
        F::from_design((lfo.get() * depth.get()).min(room))
    }

    /// The delay a read takes with the sweep `excursion` frames from the base (what
    /// [`swept`](Self::swept) gives, ramped between corners): the base moved by it exactly
    /// ([`Float::offset_position`]), held between one frame and the line's length.
    #[inline]
    #[must_use]
    pub fn position(&self, excursion: F) -> ReadPosition {
        at_least_a_frame(self.base, excursion).min(self.cap)
    }

    /// Runs `input` into `output`, frame `i` at the excursion `delays[c].at(taken + 1 + i)` for
    /// channel `c` ([`position`](Self::position) of it) and `feedback.at(taken + 1 + i)`, then
    /// takes the block's steps from the ramps. Each frame's delay is driven, so it is held
    /// between one frame (the feedback tap is read one before it) and the line's length where it
    /// is read: a value past either end reads that end, with nothing to refuse it to.
    ///
    /// Each channel is a feedback delay at its delay: the line's own tap scaled by the
    /// feedback added to the input. Channel `c` of `input` feeds channel `c` of `output`. The
    /// feedback ramp's values must stay below 1 ([`Feedback::MAX_STABLE`] is the bound
    /// [`design`](Self::design) holds them to). No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in
    /// channels, or [`BlockMismatch::Ramps`] when `delays` has fewer ramps than the bank has
    /// channels (more are not read).
    pub fn process(
        &mut self,
        delays: &mut [Ramp<F>],
        feedback: &mut Ramp<F>,
        block: &mut InputOutput<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let run = self.lines.len();
        expect_widths(block, run, run)?;
        if delays.len() < run {
            return Err(BlockMismatch::Ramps {
                expected: run,
                found: delays.len(),
            });
        }
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let delays = &mut delays[..run];
        let fb_first = feedback.steps_taken().saturating_add(1);
        let base = self.base;
        for (c, ((line, delay), out)) in self
            .lines
            .iter_mut()
            .zip(delays.iter())
            .zip(output.iter_mut())
            .enumerate()
            .take(run)
        {
            let first = delay.steps_taken().saturating_add(1);
            let xs = &input.plane(c).samples()[..frames];
            let ys = &mut out.into_samples_mut()[..frames];
            for (i, (y, x)) in ys.iter_mut().zip(xs).enumerate() {
                let step = u32::try_from(i).unwrap_or(u32::MAX);
                let d = at_least_a_frame(base, delay.at(first.saturating_add(step)));
                let g = feedback.at(fb_first.saturating_add(step));
                *y = delay_step(line, *x, d, g, READ);
            }
        }
        let taken = u32::try_from(frames).unwrap_or(u32::MAX);
        for ramp in delays.iter_mut() {
            ramp.advance(taken);
        }
        feedback.advance(taken);
        Ok(())
    }
}

/// The delay of a read `excursion` frames from `base`, held at one frame at least: the line's
/// end holds it from the other side where it is read.
#[inline(always)]
fn at_least_a_frame<F: Float>(base: ReadPosition, excursion: F) -> ReadPosition {
    F::offset_position(base, excursion).max(ReadPosition::from_frames(Samples::new(1)))
}

#[cfg(test)]
mod tests;
