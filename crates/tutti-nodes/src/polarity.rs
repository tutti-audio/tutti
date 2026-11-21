//! [`PolarityNode`]: a polarity flip of chosen channels, from a [`PolarityConfig`].

use tutti_buffers::{ChannelIndex, ChannelLayout, MAX_CHANNELS};
use tutti_buffers::{Float, InputOutput};
use tutti_dsp::{ChannelOutOfRange, Polarity};
use tutti_graph::{Cost, Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

use crate::kernel::through_planes;

/// Which channels of a width a [`PolarityNode`] inverts: every one, or the ones named.
///
/// ```
/// use tutti_nodes::{PolarityConfig, PolarityNode};
/// use tutti_buffers::{ChannelIndex, ChannelLayout};
///
/// // Invert the right channel of a stereo pair.
/// let config = PolarityConfig::new(ChannelLayout::STEREO).only([ChannelIndex::of(1)]);
/// let node = PolarityNode::try_from(config).unwrap();
/// // The layout has no third channel.
/// assert!(PolarityNode::try_from(PolarityConfig::new(ChannelLayout::STEREO).only([ChannelIndex::of(2)])).is_err());
/// # let _ = node;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolarityConfig {
    width: ChannelLayout,
    only: Option<Vec<ChannelIndex>>,
}

impl PolarityConfig {
    /// Every channel of `width` inverted.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            only: None,
        }
    }

    /// Only `channels` inverted, the rest passed. A channel past the width is refused when
    /// the node is made ([`PolarityError::NoSuchChannel`]); naming none inverts none.
    #[must_use]
    pub fn only(self, channels: impl IntoIterator<Item = ChannelIndex>) -> Self {
        Self {
            only: Some(channels.into_iter().collect()),
            ..self
        }
    }
}

/// Why a [`PolarityConfig`] makes no node.
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
/// (`PolarityNode::try_from`); `tutti_dsp::Polarity` is the algorithm.
#[derive(Clone, Debug)]
pub struct PolarityNode {
    polarity: Polarity,
}

impl TryFrom<PolarityConfig> for PolarityNode {
    type Error = PolarityError;

    fn try_from(config: PolarityConfig) -> Result<Self, PolarityError> {
        let width = config.width;
        if usize::from(width.count()) > MAX_CHANNELS {
            return Err(PolarityError::TooWide {
                width: width.count(),
            });
        }
        let polarity = match config.only {
            None => Polarity::all(width),
            Some(channels) => Polarity::new(width, channels)?,
        };
        Ok(Self { polarity })
    }
}

impl<F: Float> Node<F> for PolarityNode {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.polarity.width()).with_cost(Cost::Light)
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
        prepared(PolarityNode::try_from(config).unwrap())
    }

    /// The shape is the width in and out, `Light`, with no tail.
    #[test]
    fn the_shape_is_the_width_in_and_out_and_light() {
        let n = node(PolarityConfig::new(3u16));
        let shape = Node::<f32>::shape(&n);
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Light);
        assert_eq!(shape.tail, tutti_units::Tail::None);
    }

    /// Only the named channels are negated: of four, channels 1 and 3.
    #[test]
    fn only_the_named_channels_are_negated() {
        let mut n = node(PolarityConfig::new(4u16).only([1, 3].map(ChannelIndex::of)));
        let x = [0.25f32, -0.5, 1.0];
        let out = drive_block(&mut n, &[&x, &x, &x, &x]);
        for (c, o) in out.iter().enumerate() {
            let want: Vec<f32> = x.iter().map(|v| if c % 2 == 1 { -v } else { *v }).collect();
            assert_eq!(*o, want, "channel {c}");
        }
    }

    /// A channel the layout lacks, and a layout past one bus, are refused when the node is
    /// made.
    #[test]
    fn a_channel_outside_the_layout_is_refused() {
        assert_eq!(
            PolarityNode::try_from(
                PolarityConfig::new(ChannelLayout::STEREO).only([0, 2].map(ChannelIndex::of))
            )
            .unwrap_err(),
            PolarityError::NoSuchChannel(ChannelOutOfRange {
                channel: ChannelIndex::of(2),
                width: 2
            })
        );
        assert_eq!(
            PolarityNode::try_from(PolarityConfig::new(17u16)).unwrap_err(),
            PolarityError::TooWide { width: 17 }
        );
    }

    /// In an `f64` graph the flip negates a 29-bit signal exactly.
    #[test]
    fn in_an_f64_graph_the_flip_negates_a_wide_signal_exactly() {
        use crate::test_support::{RATE, render_f64};
        let x = tutti_fixtures::signal::wide_signal(64);
        let node = PolarityNode::try_from(PolarityConfig::new(2u16)).unwrap();
        let out = render_f64(node, RATE, &[&x, &x], 64);
        let want: Vec<f64> = x.iter().map(|v| -v).collect();
        assert_eq!(out[0], want);
        assert_eq!(out[1], want);
    }
}
