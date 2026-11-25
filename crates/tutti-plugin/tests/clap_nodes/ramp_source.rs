//! The input the CLAP node tests feed a plugin: a ramp read off each block's `Env`.

#![cfg(feature = "clap")]

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_units::Tail;

/// Input frame `t`: distinct, never zero, exact in `f32`.
pub(crate) fn ramp(t: u64) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

/// A ramp source reading its block's `Env`: frame `t` is [`ramp`]`(t)`. It holds nothing, so
/// it forks by clone.
#[derive(Clone)]
pub(crate) struct RampSource;

impl Node for RampSource {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let first = cx.env.frame.get();
        for (i, s) in io.output(0).iter_mut().enumerate() {
            *s = ramp(first + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
