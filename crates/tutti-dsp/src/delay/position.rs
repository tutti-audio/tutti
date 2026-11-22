//! [`LinePosition`]: a delay position inside a [`DelayLine`], and the refusal of one past it.

use tutti_units::{ReadPosition, Samples};

use crate::DelayLine;

/// A delay position the [`DelayLine`] that made it holds: at most the line's capacity back.
///
/// A fixed delay takes its position once, from [`DelayLine::position`] (a position past the
/// line is refused) or [`DelayLine::end`]; a delay moved per frame takes it from
/// [`DelayLine::held`]. A read ([`DelayLine::read_sample`], [`DelayLine::read_whole`]) takes
/// only this type, so it does not check the position again. A position of another, longer
/// line is a logic error, caught by a debug assertion in the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinePosition(ReadPosition);

impl LinePosition {
    /// The position, as the line holds it.
    #[inline]
    #[must_use]
    pub const fn get(self) -> ReadPosition {
        self.0
    }

    /// The position `frames` frames nearer the newest write, `0` at most: inside the line
    /// whenever this one is.
    #[inline]
    #[must_use]
    pub const fn saturating_sub(self, frames: Samples) -> Self {
        Self(self.0.saturating_sub(frames))
    }
}

/// A delay position [`DelayLine::position`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DelayError {
    /// The position is past the longest delay the line holds.
    #[error("a delay of {requested:?} is past the line's longest, {longest} frames")]
    PastLine {
        /// The position asked for.
        requested: ReadPosition,
        /// The line's capacity in frames: the longest delay it holds.
        longest: usize,
    },
}

impl<F: tutti_buffers::Float> DelayLine<F> {
    /// The longest delay the line holds: its capacity, back from the newest write.
    #[inline]
    #[must_use]
    pub fn end(&self) -> LinePosition {
        LinePosition(ReadPosition::from_frames(Samples::new(
            self.max_delay_samples,
        )))
    }

    /// `delay` as a position inside the line: the check of a fixed delay, made once where it
    /// is set.
    ///
    /// # Errors
    ///
    /// [`DelayError::PastLine`] when `delay` is past the line's capacity. A delay exactly at
    /// the capacity is accepted.
    pub fn position(&self, delay: ReadPosition) -> Result<LinePosition, DelayError> {
        if delay > self.end().0 {
            return Err(DelayError::PastLine {
                requested: delay,
                longest: self.max_delay_samples,
            });
        }
        Ok(LinePosition(delay))
    }

    /// `delay` held at the line's capacity: a delay past the line reads the longest the line
    /// holds, and a delay inside it is unchanged.
    ///
    /// This is the hold of a delay driven per frame (a swept delay, or a ramp or control
    /// signal that is not bounded by the line before it reaches the read), where there is no
    /// control side to refuse a frame's value on. A fixed delay takes
    /// [`position`](Self::position) instead.
    #[inline]
    #[must_use]
    pub fn held(&self, delay: ReadPosition) -> LinePosition {
        LinePosition(delay.min(self.end().0))
    }
}
