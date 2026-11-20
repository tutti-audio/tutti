//! [`Sweep`] and [`Stagger`]: the construction constants of a chorus or flanger, each
//! checked where it is made.

use tutti_buffers::{ChannelIndex, MAX_CHANNELS};

use tutti_units::{Feedback, Phase, PhaseIncrement, Seconds};

use crate::MaxDelay;

/// What a swept delay is built to: the delay its LFO sweeps around, how far the sweep may
/// reach, the line it must stay inside, and the most feedback it applies.
///
/// A chorus sits at a long base delay (10 ms) so the copy reads as a second voice, a
/// flanger at a short one (1 ms) so the copy combs against the original and the sweep
/// moves the notches.
///
/// A range-checked value: [`SweepBuilder::build`] refuses
/// what the line could not hold, and never clamps. The longest delay the sweep reaches is
/// `base + depth_max`, which must fit in `max_delay`, the size of the lines.
///
/// The durations are [`Seconds`], and none is a control but the depth the sweep is told
/// ([`ModDelayControls::depth`](crate::ModDelayControls::depth)), which the sweep clamps
/// into `depth_min..=depth_max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sweep {
    base: Seconds,
    max_delay: MaxDelay,
    depth_min: Seconds,
    depth_max: Seconds,
    feedback_max: Feedback,
}

/// A refused [`Sweep`]: the raws it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SweepError {
    /// The base delay is not finite or is negative.
    #[error("the base delay is {0:?}: it must be finite and not negative")]
    Base(Seconds),
    /// The depth range is not finite, has a negative end, or its minimum is above its
    /// maximum.
    #[error("the depth range {min:?}..={max:?} is not finite, not negative and in order")]
    Depth {
        /// The refused least depth.
        min: Seconds,
        /// The refused greatest depth.
        max: Seconds,
    },
    /// The sweep's longest delay is past the line: `base + depth_max > max_delay`.
    #[error("the sweep reaches {base:?} + {depth_max:?}, past the longest delay {max_delay:?}")]
    Reach {
        /// The base delay.
        base: Seconds,
        /// The greatest depth.
        depth_max: Seconds,
        /// The line's longest delay.
        max_delay: Seconds,
    },
    /// The most feedback is not in `0..=`[`Feedback::MAX_STABLE`].
    #[error("the most feedback is {0:?}: it must be finite, in 0..=MAX_STABLE")]
    FeedbackMax(Feedback),
}

impl Sweep {
    /// A [`SweepBuilder`] for a sweep around `base`, held in a line of `max_delay`: no depth
    /// and no feedback until it says so.
    #[must_use]
    pub const fn builder(base: Seconds, max_delay: MaxDelay) -> SweepBuilder {
        SweepBuilder {
            base,
            max_delay,
            depth_min: Seconds::ZERO,
            depth_max: Seconds::ZERO,
            feedback_max: Feedback::NONE,
        }
    }

    const fn checked(
        base: Seconds,
        max_delay: MaxDelay,
        depth_min: Seconds,
        depth_max: Seconds,
        feedback_max: Feedback,
    ) -> Result<Self, SweepError> {
        let (b, lo, hi) = (base.get(), depth_min.get(), depth_max.get());
        if !(b.is_finite() && b >= 0.0) {
            return Err(SweepError::Base(base));
        }
        if !(lo.is_finite() && lo >= 0.0 && hi.is_finite() && lo <= hi) {
            return Err(SweepError::Depth {
                min: depth_min,
                max: depth_max,
            });
        }
        if b + hi > max_delay.get().get() {
            return Err(SweepError::Reach {
                base,
                depth_max,
                max_delay: max_delay.get(),
            });
        }
        let f = feedback_max.get();
        if !(f.is_finite() && f >= 0.0 && f <= Feedback::MAX_STABLE.get()) {
            return Err(SweepError::FeedbackMax(feedback_max));
        }
        Ok(Self {
            base,
            max_delay,
            depth_min,
            depth_max,
            feedback_max,
        })
    }

    /// A sweep for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`SweepBuilder::build`] refuses it.
    #[must_use]
    pub const fn of(
        base: Seconds,
        max_delay: MaxDelay,
        depth_min: Seconds,
        depth_max: Seconds,
        feedback_max: Feedback,
    ) -> Self {
        match Self::checked(base, max_delay, depth_min, depth_max, feedback_max) {
            Ok(s) => s,
            Err(_) => panic!("a sweep stays inside its line"),
        }
    }

    /// Chorus: a 10 ms base delay in a 50 ms line, the depth clamped to `0..=40 ms`.
    /// The long base delay is what separates chorus from flanger: the copy sits far
    /// enough behind to read as a second voice.
    pub const CHORUS: Self = Self::of(
        Seconds::new(0.01),
        MaxDelay::of(Seconds::new(0.05)),
        Seconds::ZERO,
        Seconds::new(0.04),
        Feedback::MAX_STABLE,
    );

    /// Flanger: a 1 ms base delay in a 20 ms line, the depth clamped to `0.1..=10 ms`.
    /// The short base delay lands the copy close enough to interfere with the original,
    /// producing the comb notches whose sweep is the flanger's signature.
    pub const FLANGER: Self = Self::of(
        Seconds::new(0.001),
        MaxDelay::of(Seconds::new(0.02)),
        Seconds::new(0.0001),
        Seconds::new(0.01),
        Feedback::MAX_STABLE,
    );

    /// The chorus of a General MIDI synthesizer's chorus send, as SoundFont players have
    /// it: a 2 ms centre delay in a 5 ms line, the depth up to 1.9 ms either side, and
    /// **no feedback at all** (`feedback_max` is [`Feedback::NONE`]), which is what lets
    /// its node declare a finite ceiling: what it outputs was in its input at most
    /// `max_delay` ago.
    pub const GENERAL_MIDI_CHORUS: Self = Self::of(
        Seconds::new(0.002),
        MaxDelay::of(Seconds::new(0.005)),
        Seconds::ZERO,
        Seconds::new(0.0019),
        Feedback::NONE,
    );

    /// The centre delay the LFO sweeps around.
    #[must_use]
    pub const fn base(&self) -> Seconds {
        self.base
    }

    /// The line's longest delay: the size of the lines the sweep is run in.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.max_delay
    }

    /// The least depth the sweep takes: a smaller one reads as it.
    #[must_use]
    pub(crate) const fn depth_min(&self) -> Seconds {
        self.depth_min
    }

    /// The greatest depth the sweep takes: a larger one reads as it, which is what keeps
    /// the sweep inside the line (chorus) or inside comb-filter range (flanger).
    #[must_use]
    pub(crate) const fn depth_max(&self) -> Seconds {
        self.depth_max
    }

    /// The most feedback the sweep applies, whatever it is asked for: a larger one reads as
    /// it and one below zero as zero. [`Feedback::NONE`] makes the line a pure delay, with
    /// no recirculation.
    #[must_use]
    pub const fn feedback_max(&self) -> Feedback {
        self.feedback_max
    }
}

/// The terminal step of a [`Sweep`] of your own numbers: [`build`](Self::build) checks that
/// they fit each other and the line.
#[derive(Clone, Copy, Debug)]
pub struct SweepBuilder {
    base: Seconds,
    max_delay: MaxDelay,
    depth_min: Seconds,
    depth_max: Seconds,
    feedback_max: Feedback,
}

impl SweepBuilder {
    /// The depth the sweep takes, `min..=max`: a depth asked for outside it reads as the
    /// nearer end.
    #[must_use]
    pub const fn depth(mut self, min: Seconds, max: Seconds) -> Self {
        self.depth_min = min;
        self.depth_max = max;
        self
    }

    /// The most feedback the sweep applies.
    #[must_use]
    pub const fn feedback_max(mut self, most: Feedback) -> Self {
        self.feedback_max = most;
        self
    }

    /// The sweep.
    ///
    /// # Errors
    ///
    /// [`SweepError::Base`], [`Depth`](SweepError::Depth), [`Reach`](SweepError::Reach) for
    /// a sweep past the line, and [`FeedbackMax`](SweepError::FeedbackMax).
    pub const fn build(self) -> Result<Sweep, SweepError> {
        Sweep::checked(
            self.base,
            self.max_delay,
            self.depth_min,
            self.depth_max,
            self.feedback_max,
        )
    }
}

/// How each channel's sweep is staggered from channel 0's, in LFO cycles.
///
/// A range-checked value: every offset is finite, and
/// every channel's offset is wrapped into one cycle ([`Phase`]'s own wrap): at construction
/// for [`try_from`](TryFrom) a slice of offsets, and in [`offset`](Self::offset) for a [`step`](Self::step), whose
/// channel `c` is `c` times it. A stagger that names
/// one offset per channel (a slice of offsets) is checked against the bus it is built for
/// by [`ModDelayLayoutBuilder::build`](crate::ModDelayLayoutBuilder::build).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stagger {
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// Channel `c` runs `c` times this ahead of channel 0, wrapped.
    Step(PhaseIncrement),
    /// One wrapped offset for each of the first `count` channels.
    Each {
        count: usize,
        offsets: [PhaseIncrement; MAX_CHANNELS],
    },
}

/// A refused [`Stagger`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum StaggerError {
    /// An offset is not finite.
    #[error("the offset of channel {channel} is {offset:?}, not finite")]
    NotFinite {
        /// The channel.
        channel: usize,
        /// The refused offset.
        offset: PhaseIncrement,
    },
    /// More offsets than a bus has channels.
    #[error("{count} offsets: a bus has at most {max} channels")]
    TooMany {
        /// What was given.
        count: usize,
        /// The most a bus holds.
        max: usize,
    },
    /// The offsets are not one for each channel.
    #[error("{got} offsets on a {width}-channel sweep, which takes {width}")]
    Length {
        /// The bus's width.
        width: usize,
        /// What was given.
        got: usize,
    },
}

impl Stagger {
    /// No stagger: every channel sweeps in step with channel 0.
    pub const NONE: Self = Self {
        kind: Kind::Step(PhaseIncrement::new(0.0)),
    };

    /// Channel `c` runs `c` times `step` ahead of channel 0, wrapped into one cycle: at
    /// width 2 the step is the right channel's offset. `None` when `step` is not finite.
    #[must_use]
    pub fn step(step: PhaseIncrement) -> Option<Self> {
        if !step.get().is_finite() {
            return None;
        }
        Some(Self {
            kind: Kind::Step(step),
        })
    }

    /// A step for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `step` is not finite.
    #[must_use]
    pub fn of_step(step: PhaseIncrement) -> Self {
        match Self::step(step) {
            Some(s) => s,
            None => panic!("a stagger step is finite"),
        }
    }

    /// How many channels it names an offset for, or `None` for a [`step`](Self::step),
    /// which fits any width.
    #[must_use]
    pub const fn channels(&self) -> Option<usize> {
        match self.kind {
            Kind::Step(_) => None,
            Kind::Each { count, .. } => Some(count),
        }
    }

    /// Channel `c`'s offset in cycles, wrapped into `[0, 1)`: `c` times the step, or the
    /// offset it was given (zero for a channel a slice of offsets names none for).
    #[must_use]
    pub fn offset(&self, channel: ChannelIndex) -> PhaseIncrement {
        match self.kind {
            Kind::Step(step) => wrap(step * channel.get() as f64),
            Kind::Each { offsets, .. } => offsets
                .get(channel.get())
                .copied()
                .unwrap_or(PhaseIncrement::new(0.0)),
        }
    }
}

/// One offset for each channel, channel 0's first (usually zero).
impl TryFrom<&[PhaseIncrement]> for Stagger {
    type Error = StaggerError;

    /// # Errors
    ///
    /// [`StaggerError::NotFinite`] or [`TooMany`](StaggerError::TooMany).
    fn try_from(offsets: &[PhaseIncrement]) -> Result<Self, StaggerError> {
        if offsets.len() > MAX_CHANNELS {
            return Err(StaggerError::TooMany {
                count: offsets.len(),
                max: MAX_CHANNELS,
            });
        }
        let mut all = [PhaseIncrement::new(0.0); MAX_CHANNELS];
        for (channel, &offset) in offsets.iter().enumerate() {
            if !offset.get().is_finite() {
                return Err(StaggerError::NotFinite { channel, offset });
            }
            all[channel] = wrap(offset);
        }
        Ok(Self {
            kind: Kind::Each {
                count: offsets.len(),
                offsets: all,
            },
        })
    }
}

/// `x` wrapped into one cycle by [`Phase`]'s own wrap.
fn wrap(x: PhaseIncrement) -> PhaseIncrement {
    PhaseIncrement::new(Phase::START.advance(x).get())
}

#[cfg(test)]
mod tests;
