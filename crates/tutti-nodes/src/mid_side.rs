//! [`MidSideNode`]: a stereo pair converted to mid/side or back.

use tutti_buffers::{Planes, PlanesMut};
use tutti_dsp::MidSide;
use tutti_graph::{Cost, Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::BusTopology;

use crate::kernel::through_planes;

/// A mid/side converter: stereo in, stereo out, in the direction it is made with.
///
/// [`MidSide::Encode`] turns left/right into mid (channel 0, `(L + R) / 2`) and side
/// (channel 1, `(L - R) / 2`); [`MidSide::Decode`] turns mid/side back into left/right
/// (`M + S`, `M - S`). Memoryless, with no params, no latency and no tail. Made from a
/// [`MidSideConfig`] (`MidSideNode::from`); `tutti_dsp::MidSide` is the algorithm.
/// A decoded encoding is the input to within one rounding per operation (see
/// [`MidSide::process`]).
#[derive(Clone, Copy, Debug)]
pub struct MidSideNode {
    direction: MidSide,
}

/// What a [`MidSideNode`] is made from: its direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MidSideConfig {
    direction: MidSide,
}

impl MidSideConfig {
    /// Left/right in, mid/side out.
    #[must_use]
    pub fn encode() -> Self {
        Self {
            direction: MidSide::Encode,
        }
    }

    /// Mid/side in, left/right out.
    #[must_use]
    pub fn decode() -> Self {
        Self {
            direction: MidSide::Decode,
        }
    }
}

impl From<MidSideConfig> for MidSideNode {
    fn from(config: MidSideConfig) -> Self {
        Self {
            direction: config.direction,
        }
    }
}

impl Node for MidSideNode {
    fn shape(&self) -> Shape {
        Shape::through(BusTopology::STEREO).with_cost(Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        through_planes(
            &mut io,
            2,
            2,
            |input: Planes<'_, f32>, output: PlanesMut<'_, '_, f32>| {
                self.direction.process(input, output);
            },
        );
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl IntoNode for MidSideNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        ForkByClone(self).into_parts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{drive_block, prepared};

    /// Encoding `(1, 0.5)` gives `(0.75, 0.25)`; decoding that gives `(1, 0.5)` back: the
    /// values are exact in `f32`.
    #[test]
    fn the_node_encodes_and_decodes_the_closed_form() {
        let mut enc = prepared(MidSideNode::from(MidSideConfig::encode()));
        let mut dec = prepared(MidSideNode::from(MidSideConfig::decode()));
        let coded = drive_block(&mut enc, &[&[1.0, 1.0, -0.5], &[0.5, -1.0, -0.5]]);
        assert_eq!(coded, [vec![0.75, 0.0, -0.5], vec![0.25, 1.0, 0.0]]);
        let back = drive_block(&mut dec, &[&coded[0], &coded[1]]);
        assert_eq!(back, [vec![1.0, 1.0, -0.5], vec![0.5, -1.0, -0.5]]);
    }

    /// The shape is stereo in and out, `Light`.
    #[test]
    fn the_shape_is_stereo_and_light() {
        let shape = MidSideNode::from(MidSideConfig::encode()).shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.cost, Cost::Light);
    }
}
