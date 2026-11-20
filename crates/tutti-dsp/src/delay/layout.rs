//! [`DelayLayout`]: what a delay is built to, and [`DelayControls`], what it is told.
//!
//! A delay of any width has one [`DelayLine`](crate::DelayLine) per channel, each
//! recirculating its own tap (the feedback) and, when the layout has a [`FeedbackRouting`], the
//! taps of the others (the cross-feedback, a stereo ping-pong's). The output is the delayed
//! signal alone, the **wet** signal: the blend with the dry input is the graph's mix.
//!
//! The longest delay, the interpolation, the routing and the channel times are
//! **construction constants**, not controls: the longest delay sizes the lines, and the
//! others are a build-time choice, not something to automate. A stereo delay is routed by
//! [`FeedbackRouting::swap`] (a ping-pong) and any other width by [`FeedbackRouting::none`], until
//! [`DelayLayoutBuilder::routing`] says otherwise.

use tutti_buffers::BusTopology;
use tutti_units::{Feedback, Seconds};

use super::max_delay::MaxDelay;
use super::routing::{FeedbackRouting, FeedbackRoutingError, MAX_WIDTH};
use super::times::{ChannelTimes, ChannelTimesError};
use crate::InterpolationMode;

/// The farthest a delay time moves in one frame: 1/8 of a frame of delay per frame.
///
/// A moving delay is a pitch shift: a delay that grows by `s` frames per frame lowers
/// the pitch by the factor `1 / (1 + s)`, so the ramp's speed bounds the bend, and a
/// time that jumps is a pitch bend as long as the jump is far. At 1/8 the bend is at
/// most 12.5%, about 2 semitones (`12 log2(1.125) = 2.04`), however far the time moves:
/// a jump of 0.5 s at 48 kHz ramps for 192,000 frames (4 s).
///
/// A plain number, frames of delay per frame, so no unit: a span of frames (`SampleSpan`) is
/// not a rate, and none of the units covers a dimensionless slope. It is the `Rate` of a
/// [`Steps::Rate`](crate::Steps::Rate), the ramp's own `f64`.
pub const DELAY_SLEW: f64 = 0.125;

/// The most channels a delay has: a bus's.
pub const MAX_DELAY_CHANNELS: usize = MAX_WIDTH;

/// The controls of a delay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayControls {
    /// The delay of channel 0, and of every channel that follows it
    /// ([`ChannelTimes::linked`]): clamped into `0..=max_delay`. A time that is not finite is not taken: the node holds the
    /// last finite one.
    pub time: Seconds,
    /// How much of a channel's own delayed tap recirculates into its line. Bounded, with
    /// the cross-feedback, as a pair
    /// ([`Feedback::stable_pair`]), so the loop gain stays under
    /// [`Feedback::MAX_STABLE`]. One that is not finite is not taken: the node holds the last finite one.
    pub feedback: Feedback,
    /// How much of the routed taps ([`FeedbackRouting`]) recirculates: a stereo ping-pong's
    /// cross-feed. Inert on a delay with no routing. One that is not finite is not taken: the node holds the last finite one.
    pub cross_feedback: Feedback,
}

impl DelayControls {
    /// No delay, no recirculation: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        time: Seconds::ZERO,
        feedback: Feedback::NONE,
        cross_feedback: Feedback::NONE,
    };
}

impl Default for DelayControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a delay is built to: `input` wide, each channel a line of up to a [`MaxDelay`],
/// read with an [`InterpolationMode`], recirculating through its own tap and, with a
/// [`FeedbackRouting`], the others', its channels past the first following the time control or
/// holding [`ChannelTimes`] of their own.
#[derive(Clone, Copy, Debug)]
pub struct DelayLayout {
    input: BusTopology,
    max_delay: MaxDelay,
    interpolation: InterpolationMode,
    routing: FeedbackRouting,
    times: ChannelTimes,
}

impl DelayLayout {
    /// A delay on the bus `input`, holding up to `max_delay`, reading with
    /// `interpolation`, its channels linked in time and routed by the default for its
    /// width.
    #[must_use]
    pub fn new(input: BusTopology, max_delay: MaxDelay, interpolation: InterpolationMode) -> Self {
        let routing = if input == BusTopology::STEREO {
            FeedbackRouting::swap()
        } else {
            FeedbackRouting::none(input)
        };
        Self {
            input,
            max_delay,
            interpolation,
            routing,
            times: ChannelTimes::linked(),
        }
    }

    /// A [`DelayLayoutBuilder`] for the same delay, to give it a routing or channel times of
    /// its own.
    #[must_use]
    pub const fn builder(
        input: BusTopology,
        max_delay: MaxDelay,
        interpolation: InterpolationMode,
    ) -> DelayLayoutBuilder {
        DelayLayoutBuilder {
            input,
            max_delay,
            interpolation,
            routing: None,
            times: ChannelTimes::linked(),
        }
    }

    /// The bus the delay is on.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.layout().count())
    }

    /// The longest delay a line holds.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.max_delay
    }

    /// How a delayed position is read.
    #[must_use]
    pub const fn interpolation(&self) -> InterpolationMode {
        self.interpolation
    }

    /// How the channels recirculate into each other.
    #[must_use]
    pub const fn routing(&self) -> &FeedbackRouting {
        &self.routing
    }

    /// The times of the channels past the first.
    #[must_use]
    pub const fn times(&self) -> &ChannelTimes {
        &self.times
    }
}

/// The terminal step of a [`DelayLayout`] with a routing or channel times of its own:
/// [`build`](Self::build) checks them against the bus and the longest delay, which the
/// options cannot see.
#[derive(Clone, Copy, Debug)]
pub struct DelayLayoutBuilder {
    input: BusTopology,
    max_delay: MaxDelay,
    interpolation: InterpolationMode,
    routing: Option<FeedbackRouting>,
    times: ChannelTimes,
}

/// A refused [`DelayLayout`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum DelayLayoutError {
    /// The routing does not fit the bus.
    #[error(transparent)]
    Routing(#[from] FeedbackRoutingError),
    /// The channel times do not fit the bus or the longest delay.
    #[error(transparent)]
    Times(#[from] ChannelTimesError),
}

impl DelayLayoutBuilder {
    /// Routes the cross-feedback by `routing`.
    #[must_use]
    pub const fn routing(mut self, routing: FeedbackRouting) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Delays the channels past the first by `times`.
    #[must_use]
    pub const fn times(mut self, times: ChannelTimes) -> Self {
        self.times = times;
        self
    }

    /// The layout.
    ///
    /// # Errors
    ///
    /// [`FeedbackRoutingError::Width`] when the routing is for another bus,
    /// [`ChannelTimesError::Length`] unless the times are [linked](ChannelTimes::linked) or
    /// have one time for each channel past the first, and [`ChannelTimesError::Past`] for a
    /// time past the longest delay.
    pub fn build(self) -> Result<DelayLayout, DelayLayoutError> {
        let mut layout = DelayLayout::new(self.input, self.max_delay, self.interpolation);
        if let Some(routing) = self.routing {
            if routing.input() != self.input {
                return Err(FeedbackRoutingError::Width {
                    routing: routing.width(),
                    family: layout.width(),
                }
                .into());
            }
            layout.routing = routing;
        }
        let times = self.times;
        if !times.is_linked() && times.fixed().len() != layout.width() - 1 {
            return Err(ChannelTimesError::Length {
                width: layout.width(),
                expected: layout.width() - 1,
                got: times.fixed().len(),
            }
            .into());
        }
        let max = self.max_delay.get();
        if let Some(i) = times.fixed().iter().position(|t| t.get() > max.get()) {
            return Err(ChannelTimesError::Past {
                channel: i + 1,
                time: times.fixed()[i],
                max,
            }
            .into());
        }
        layout.times = times;
        Ok(layout)
    }
}

#[cfg(test)]
mod tests;
