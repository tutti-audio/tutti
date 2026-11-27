//! [`PolarityNode`]: a polarity flip of chosen channels, from a [`PolarityConfig`].

use tutti_buffers::{ChannelIndex, ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, InputOutput};
use tutti_dsp::{ChannelOutOfRange, Polarity};
use tutti_graph::{
    Cost, Cx, ForkByClone, HostValues, Initials, IntoNode, Io, Node, NodeConfig, NodeParts,
    Prepare, Shape, Status,
};

use crate::kernel::through_planes;

/// Which channels of a width a [`PolarityNode`] inverts: every one ([`all`](Self::all)) or
/// the ones named ([`only`](Self::only)), checked against the width where the config is made.
///
/// ```
/// use tutti_nodes::{PolarityConfig, PolarityNode};
/// use tutti_buffers::{ChannelIndex, ChannelLayout};
///
/// // Invert the right channel of a stereo pair.
/// let config = PolarityConfig::only(ChannelLayout::STEREO, [ChannelIndex::of(1)]).unwrap();
/// let node = PolarityNode::from(config);
/// // The layout has no third channel.
/// assert!(PolarityConfig::only(ChannelLayout::STEREO, [ChannelIndex::of(2)]).is_err());
/// # let _ = node;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolarityConfig {
    polarity: Polarity,
}

impl PolarityConfig {
    /// Every channel of `width` inverted.
    ///
    /// # Errors
    ///
    /// [`PolarityError::TooWide`] for a width past one bus.
    pub fn all(width: impl Into<ChannelLayout>) -> Result<Self, PolarityError> {
        let width = Self::bus(width.into())?;
        Ok(Self {
            polarity: Polarity::all(width),
        })
    }

    /// Only `channels` of `width` inverted, the rest passed; naming none inverts none.
    ///
    /// # Errors
    ///
    /// [`PolarityError::TooWide`] for a width past one bus, and
    /// [`PolarityError::NoSuchChannel`] for a channel past the width.
    pub fn only(
        width: impl Into<ChannelLayout>,
        channels: impl IntoIterator<Item = ChannelIndex>,
    ) -> Result<Self, PolarityError> {
        let width = Self::bus(width.into())?;
        Ok(Self {
            polarity: Polarity::new(width, channels)?,
        })
    }

    /// `width`, when one bus holds it.
    fn bus(width: ChannelLayout) -> Result<ChannelLayout, PolarityError> {
        if usize::from(width.count()) > MAX_CHANNELS {
            return Err(PolarityError::TooWide {
                width: width.count(),
            });
        }
        Ok(width)
    }
}

/// Why a [`PolarityConfig`] cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PolarityError {
    /// A channel named is not in the layout.
    #[error(transparent)]
    NoSuchChannel(#[from] ChannelOutOfRange),
    /// The layout is wider than one bus.
    #[error("a polarity node is at most {MAX_CHANNELS} channels wide, not {width}")]
    TooWide {
        /// The layout's width.
        width: u16,
    },
}

/// A polarity flip of any width: output `c` is `-input c` where the config inverts it and
/// `input c` elsewhere, exactly.
///
/// Memoryless, with no params, no latency and no tail. Made from a [`PolarityConfig`]
/// (`PolarityNode::from`); `tutti_dsp::Polarity` is the algorithm.
#[derive(Clone, Debug)]
pub struct PolarityNode {
    polarity: Polarity,
}

impl From<PolarityConfig> for PolarityNode {
    fn from(config: PolarityConfig) -> Self {
        Self {
            polarity: config.polarity,
        }
    }
}

/// A polarity flip has no param: the channels it inverts are its structure.
impl<F: Float> NodeConfig<F> for PolarityConfig {
    type Node = PolarityNode;

    fn build(&self, _: &HostValues) -> PolarityNode {
        PolarityNode::from(self.clone())
    }

    fn same_structure(&self, other: &Self) -> bool {
        self == other
    }

    fn initials(&self) -> Initials {
        Initials::new()
    }
}

impl<F: Float> Node<F> for PolarityNode {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.polarity.width()).cost(Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let width = usize::from(self.polarity.width().count());
        through_planes(&mut io, width, width, |block: &mut InputOutput<'_, F>| {
            self.polarity.process(block)
        });
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl<F: Float> IntoNode<F> for PolarityNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
        ForkByClone(self).into_parts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{drive_block, prepared};

    fn node(config: PolarityConfig) -> PolarityNode {
        prepared(PolarityNode::from(config))
    }

    /// Only the named channels are negated: of four, channels 1 and 3.
    #[test]
    fn only_the_named_channels_are_negated() {
        let mut n = node(PolarityConfig::only(4u16, [1, 3].map(ChannelIndex::of)).unwrap());
        let x = [0.25f32, -0.5, 1.0];
        let out = drive_block(&mut n, &[&x, &x, &x, &x]);
        for (c, o) in out.iter().enumerate() {
            let want: Vec<f32> = x.iter().map(|v| if c % 2 == 1 { -v } else { *v }).collect();
            assert_eq!(*o, want, "channel {c}");
        }
    }

    /// A channel the layout lacks, and a layout past one bus, are refused when the config is
    /// made.
    #[test]
    fn a_channel_outside_the_layout_is_refused() {
        assert_eq!(
            PolarityConfig::only(ChannelLayout::STEREO, [0, 2].map(ChannelIndex::of)).unwrap_err(),
            PolarityError::NoSuchChannel(ChannelOutOfRange {
                channel: ChannelIndex::of(2),
                width: 2
            })
        );
        assert_eq!(
            PolarityConfig::all(17u16).unwrap_err(),
            PolarityError::TooWide { width: 17 }
        );
    }

    /// In an `f64` graph the flip negates a 29-bit signal exactly.
    #[test]
    fn in_an_f64_graph_the_flip_negates_a_wide_signal_exactly() {
        use crate::test_support::{RATE, render_f64};
        let x = tutti_fixtures::signal::wide_signal(64);
        let node = PolarityNode::from(PolarityConfig::all(2u16).unwrap());
        let out = render_f64(node, RATE, &[&x, &x], 64);
        let want: Vec<f64> = x.iter().map(|v| -v).collect();
        assert_eq!(out[0], want);
        assert_eq!(out[1], want);
    }
}
