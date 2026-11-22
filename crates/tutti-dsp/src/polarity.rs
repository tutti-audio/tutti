//! [`Polarity`]: which channels of a block are inverted, and the inversion.
//!
//! A polarity flip is a multiplication by -1: `y = -x`, exactly, for every sample
//! (a zero stays a zero of either sign, and NaN stays NaN). It is memoryless, so a
//! block's render does not depend on where blocks fall.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelIndex, ChannelLayout};
use tutti_buffers::{Float, InputOutput};

/// A channel an inversion names that the width lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("channel {} is not in a layout of {width} channels", channel.get())]
pub struct ChannelOutOfRange {
    /// The channel named.
    pub channel: ChannelIndex,
    /// The layout's width.
    pub width: u16,
}

/// The channels of a [`ChannelLayout`] that [`process`](Self::process) inverts.
///
/// Built for a width: [`all`](Self::all) inverts every channel and [`new`](Self::new) the
/// ones named. A channel the width lacks is refused, never wrapped or dropped.
///
/// ```
/// use tutti_dsp::Polarity;
/// use tutti_buffers::{ChannelIndex, ChannelLayout};
///
/// let flips = Polarity::new(ChannelLayout::STEREO, [ChannelIndex::of(1)]).unwrap();
/// let (first, second) = (ChannelIndex::of(0), ChannelIndex::of(1));
/// assert_eq!((flips.inverts(first), flips.inverts(second)), (false, true));
/// assert!(Polarity::new(ChannelLayout::STEREO, [ChannelIndex::of(2)]).is_err(), "no third channel");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Polarity {
    width: ChannelLayout,
    inverted: Vec<bool>,
}

impl Polarity {
    /// Every channel of `width` inverted.
    #[must_use]
    pub fn all(width: ChannelLayout) -> Self {
        Self {
            width,
            inverted: vec![true; usize::from(width.count())],
        }
    }

    /// Only `channels` of `width` inverted (none, for none named).
    ///
    /// # Errors
    ///
    /// [`ChannelOutOfRange`] for the first channel the width lacks: the validity is the
    /// width's and the channels' together.
    pub fn new(
        width: ChannelLayout,
        channels: impl IntoIterator<Item = ChannelIndex>,
    ) -> Result<Self, ChannelOutOfRange> {
        let mut inverted = vec![false; usize::from(width.count())];
        for channel in channels {
            let named = width.index(channel.get()).ok_or(ChannelOutOfRange {
                channel,
                width: width.count(),
            })?;
            if let Some(slot) = inverted.get_mut(named.get()) {
                *slot = true;
            }
        }
        Ok(Self { width, inverted })
    }

    /// Whether `channel` is inverted; `false` for a channel the width lacks.
    #[must_use]
    pub fn inverts(&self, channel: ChannelIndex) -> bool {
        self.inverted.get(channel.get()).copied().unwrap_or(false)
    }

    /// The width it was built for.
    #[must_use]
    pub fn width(&self) -> ChannelLayout {
        self.width
    }

    /// Runs `input` into `output`: channel `c` negated where [`inverts`](Self::inverts),
    /// copied elsewhere. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process<F: Float>(&self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.inverted.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        for (c, &flip) in self.inverted.iter().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            if flip {
                for (y, &x) in y.iter_mut().zip(x) {
                    *y = -x;
                }
            } else {
                y.copy_from_slice(x);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
