//! [`MidSideNode`]: a stereo pair converted to mid/side or back.

use tutti_buffers::BusTopology;
use tutti_buffers::{Float, InputOutput};
use tutti_dsp::MidSide;
use tutti_graph::{Cost, Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

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

impl<F: Float> Node<F> for MidSideNode {
    fn shape(&self) -> Shape {
        Shape::through(BusTopology::STEREO).cost(Cost::Light)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        through_planes(&mut io, 2, 2, |block: &mut InputOutput<'_, F>| {
            self.direction.process(block)
        });
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl<F: Float> IntoNode<F> for MidSideNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
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
        let shape = Node::<f32>::shape(&MidSideNode::from(MidSideConfig::encode()));
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.cost, Cost::Light);
    }

    /// In an `f64` graph the encoder is `(l + r) / 2` and `(l - r) / 2` on 29-bit signals, to
    /// `2^-45`.
    #[test]
    fn in_an_f64_graph_the_encoder_is_the_half_sum_and_half_difference_of_wide_signals() {
        use crate::test_support::{F64_TOLERANCE, RATE, render_f64};
        let l = tutti_fixtures::signal::wide_signal(64);
        let r: Vec<f64> = l.iter().rev().map(|v| v - 0.3).collect();
        let out = render_f64(
            MidSideNode::from(MidSideConfig::encode()),
            RATE,
            &[&l, &r],
            64,
        );
        for n in 0..64 {
            assert!(
                (out[0][n] - (l[n] + r[n]) / 2.0).abs() <= F64_TOLERANCE,
                "mid {n}"
            );
            assert!(
                (out[1][n] - (l[n] - r[n]) / 2.0).abs() <= F64_TOLERANCE,
                "side {n}"
            );
        }
    }
}
