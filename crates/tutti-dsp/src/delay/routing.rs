//! [`FeedbackRouting`]: which channel's delayed tap recirculates into which channel's line.

use tutti_buffers::BusTopology;
use tutti_units::{Amplitude, Gain};

/// The widest routing: a bus's channel count.
pub(crate) const MAX_WIDTH: usize = tutti_buffers::MAX_CHANNELS;

/// The cross-feedback routing of a delay: entry `(c, j)` is how much of channel `j`'s
/// delayed tap recirculates into channel `c`'s line, before the cross-feedback amount
/// scales it.
///
/// A range-checked value: square on one bus, every
/// weight finite and **every row's absolute sum at most 1**. With the self and cross
/// feedback bounded as a pair ([`Feedback::stable_pair`](tutti_units::Feedback::stable_pair)),
/// that keeps each line's loop gain under
/// [`Feedback::MAX_STABLE`](tutti_units::Feedback::MAX_STABLE) whatever the matrix says.
/// [`FeedbackRoutingBuilder::build`] refuses a row that sums past 1;
/// [`normalized`](FeedbackRoutingBuilder::normalized) is the named saturation, which scales it
/// down. A weight that is not finite is refused by
/// both: it would survive the scaling (`inf / inf` is NaN) and poison the loop it feeds
/// for good.
///
/// The presets: [`none`](Self::none) (no recirculation between channels: they render
/// independently), [`swap`](Self::swap) (a stereo ping-pong, `[[0, 1], [1, 0]]`) and
/// [`ring`](Self::ring) (channel `c` fed by `c - 1`, which spins echoes around a surround
/// field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeedbackRouting {
    input: BusTopology,
    weights: [[Gain; MAX_WIDTH]; MAX_WIDTH],
}

/// A refused routing.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FeedbackRoutingError {
    /// The matrix is not `width x width`, row-major.
    #[error("a {width}-channel routing has {expected} weights, not {got}")]
    Length {
        /// The bus's width.
        width: usize,
        /// `width * width`.
        expected: usize,
        /// What was given.
        got: usize,
    },
    /// A weight is not finite.
    #[error("the weight at row {row}, column {column} is {weight}, not finite")]
    NotFinite {
        /// The row, the channel the tap feeds.
        row: usize,
        /// The column, the channel the tap is of.
        column: usize,
        /// The refused weight.
        weight: Gain,
    },
    /// A row's absolute weights sum past 1 ([`FeedbackRoutingBuilder::normalized`] scales it down).
    #[error("row {row} sums to {sum}, past 1")]
    Row {
        /// The row.
        row: usize,
        /// The sum of its absolute weights.
        sum: Amplitude,
    },
    /// The routing is for another bus than the delay's.
    #[error("a routing for {routing} channels on a {family}-channel delay")]
    Width {
        /// The routing's width.
        routing: usize,
        /// The delay's.
        family: usize,
    },
}

impl FeedbackRouting {
    /// No routing between channels: each renders independently, and the cross-feedback
    /// is inert.
    #[must_use]
    pub const fn none(input: BusTopology) -> Self {
        Self {
            input,
            weights: [[Gain::SILENT; MAX_WIDTH]; MAX_WIDTH],
        }
    }

    /// The stereo ping-pong: each channel is fed by the other, `[[0, 1], [1, 0]]`.
    #[must_use]
    pub const fn swap() -> Self {
        let mut r = Self::none(BusTopology::STEREO);
        r.weights[0][1] = Gain::UNITY;
        r.weights[1][0] = Gain::UNITY;
        r
    }

    /// A ring: channel `c` is fed by channel `c - 1` (the first by the last), so an echo
    /// goes round the field. A mono bus has no other channel to feed from: it is
    /// [`none`](Self::none).
    #[must_use]
    pub fn ring(input: BusTopology) -> Self {
        let mut r = Self::none(input);
        let n = r.width();
        if n > 1 {
            for c in 0..n {
                r.weights[c][(c + n - 1) % n] = Gain::UNITY;
            }
        }
        r
    }

    /// A [`FeedbackRoutingBuilder`] for the routing `matrix` on `input`, `width x width`
    /// row-major: entry `matrix[c * width + j]` is how much of channel `j`'s tap feeds
    /// channel `c`.
    #[must_use]
    pub const fn builder(input: BusTopology, matrix: &[Gain]) -> FeedbackRoutingBuilder<'_> {
        FeedbackRoutingBuilder {
            input,
            matrix,
            normalize: false,
        }
    }

    /// The bus this routing is for.
    #[must_use]
    pub const fn input(&self) -> BusTopology {
        self.input
    }

    /// The channels it routes.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.input.layout().count() as usize
    }

    /// How much of channel `from`'s tap feeds channel `to`'s line: 0 for a channel
    /// that is not on the bus.
    #[must_use]
    pub fn weight(&self, to: usize, from: usize) -> Gain {
        if to < MAX_WIDTH && from < MAX_WIDTH {
            self.weights[to][from]
        } else {
            Gain::SILENT
        }
    }

    /// Whether channel `to` is fed by no tap.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.weights.iter().flatten().all(|&w| w == Gain::SILENT)
    }
}

/// The terminal step of a [`FeedbackRouting`] made from a matrix: [`build`](Self::build)
/// checks that the matrix is `width x width`, that every weight is finite and that every row
/// sums to at most 1, which depend on the bus and the matrix together.
#[derive(Clone, Copy, Debug)]
pub struct FeedbackRoutingBuilder<'a> {
    input: BusTopology,
    matrix: &'a [Gain],
    normalize: bool,
}

impl FeedbackRoutingBuilder<'_> {
    /// Scales each row that sums past 1 down to sum to 1 (to within rounding) instead of
    /// refusing it. The named saturation: the one way a matrix is rescaled.
    #[must_use]
    pub const fn normalized(mut self) -> Self {
        self.normalize = true;
        self
    }

    /// The routing.
    ///
    /// # Errors
    ///
    /// [`FeedbackRoutingError::Length`], [`NotFinite`](FeedbackRoutingError::NotFinite) or,
    /// unless [`normalized`](Self::normalized), [`Row`](FeedbackRoutingError::Row), the first
    /// found.
    pub fn build(self) -> Result<FeedbackRouting, FeedbackRoutingError> {
        let Self {
            input,
            matrix,
            normalize,
        } = self;
        let mut r = FeedbackRouting::none(input);
        let n = r.width();
        if matrix.len() != n * n {
            return Err(FeedbackRoutingError::Length {
                width: n,
                expected: n * n,
                got: matrix.len(),
            });
        }
        if let Some(i) = matrix.iter().position(|w| !w.get().is_finite()) {
            return Err(FeedbackRoutingError::NotFinite {
                row: i / n,
                column: i % n,
                weight: matrix[i],
            });
        }
        for (c, row) in matrix.chunks(n).enumerate() {
            let sum: f64 = row.iter().map(|w| w.magnitude().get()).sum();
            if normalize {
                let scale = Gain::new(if sum > 1.0 { 1.0 / sum } else { 1.0 });
                for (j, &w) in row.iter().enumerate() {
                    r.weights[c][j] = w * scale;
                }
            } else if sum > 1.0 {
                return Err(FeedbackRoutingError::Row {
                    row: c,
                    sum: Amplitude::new(sum),
                });
            } else {
                r.weights[c][..n].copy_from_slice(row);
            }
        }
        Ok(r)
    }
}
