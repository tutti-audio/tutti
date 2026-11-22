//! What stands at the node of a plugin that is gone.

use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};

/// A node that renders silence in the shape the plugin's node had: its ports,
/// events and latency, so the wiring stays valid and delay compensation does
/// not move, with no tail (nothing is left to sound after its input stops) and
/// a light cost, and it parks (it returns `Idle`). A fork of it is itself.
#[derive(Clone, Debug)]
pub(crate) struct Lost {
    shape: Shape,
}

impl Lost {
    /// The stand-in for a node of `shape`.
    pub(crate) const fn of(shape: Shape) -> Self {
        Self {
            shape: shape
                .tail(tutti_units::Tail::None)
                .cost(tutti_graph::Cost::Light),
        }
    }
}

impl Node for Lost {
    fn shape(&self) -> Shape {
        self.shape
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        // Nothing is pending inside it: with no tail, an idle node parks.
        Status::Idle
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Planes;
    use tutti_buffers::{ChannelLayout, MaxBlock};
    use tutti_graph::{ForkByClone, GraphBuilder};
    use tutti_units::{Latency, SampleRate, Samples, Tail};

    /// **The stand-in keeps the ports and the latency, has no tail, and renders
    /// silence**: what a dead plugin's node must keep (its wiring, the delay
    /// compensation around it) and shed (a tail nothing is left to sound).
    #[test]
    fn the_stand_in_keeps_ports_and_latency_drops_the_tail_and_renders_silence() {
        let shape = Shape::through_unassigned(ChannelLayout::MONO)
            .latency(Latency::new(Samples::new(64)))
            .tail(Tail::Finite(Samples::new(100)));
        let stand_in = Lost::of(shape).shape();
        assert_eq!(stand_in.latency, shape.latency, "the last latency");
        assert_eq!(stand_in.tail, Tail::None, "no tail");
        assert_eq!(stand_in.audio_in().count(), shape.audio_in().count());
        assert_eq!(stand_in.audio_out().count(), shape.audio_out().count());

        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
        let lost = g.add(ForkByClone(Lost::of(shape)));
        g.pipe_input(lost).pipe_output(lost);
        let mut r = g
            .renderer(Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(64)),
            ))
            .expect("builds");
        assert_eq!(
            r.render_input(Planes::new(&[&[1.0; 8]]).unwrap())[0],
            [0.0; 8]
        );
    }
}
