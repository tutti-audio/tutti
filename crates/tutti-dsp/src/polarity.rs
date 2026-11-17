//! [`Polarity`]: which channels of a block are inverted, and the inversion.
//!
//! A polarity flip is a multiplication by -1: `y = -x`, exactly, for every sample
//! (a zero stays a zero of either sign, and NaN stays NaN). It is memoryless, so a
//! block's render does not depend on where blocks fall.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_types::{ChannelIndex, ChannelLayout};

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
/// use tutti_types::{ChannelIndex, ChannelLayout};
///
/// let flips = Polarity::new(ChannelLayout::STEREO, [ChannelIndex::of(1)]).unwrap();
/// assert_eq!((flips.inverts(0), flips.inverts(1)), (false, true));
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
            inverted[named.get()] = true;
        }
        Ok(Self { width, inverted })
    }

    /// Whether `channel` is inverted; `false` for a channel the width lacks.
    #[must_use]
    pub fn inverts(&self, channel: usize) -> bool {
        self.inverted.get(channel).copied().unwrap_or(false)
    }

    /// The width it was built for.
    #[must_use]
    pub fn width(&self) -> ChannelLayout {
        self.width
    }

    /// Runs `input` into `output`: channel `c` negated where [`inverts`](Self::inverts),
    /// copied elsewhere. The channels rendered are the fewest of the two views' and the
    /// width's, and the frames the fewer of the two views'. No allocation.
    pub fn process<S: Float>(&self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input
            .channels()
            .min(output.channels())
            .min(self.inverted.len());
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
    }
}

#[cfg(test)]
mod tests;
