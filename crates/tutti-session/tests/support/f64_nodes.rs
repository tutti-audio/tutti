//! Two nodes in any sample format and a figure `f32` cannot hold: what the
//! `f64` session tests render.

use tutti_buffers::Float;
use tutti_graph::{Cx, Io, Node, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, Param};
use tutti_units::{Amplitude, UnitParam};

/// One part in 2^40 above one: past `f32`'s 24-bit significand, inside `f64`'s 53.
pub const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;

/// A constant source in any sample format.
#[derive(Clone)]
pub struct Dc(pub f64);

impl<F: Float> Node<F> for Dc {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        io.output(0).fill(F::from_f64(self.0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Multiplies a mono signal by its `Amplitude` param, in any sample format.
#[derive(Clone)]
pub struct Gain(pub Param<Amplitude>);

impl<F: Float> Node<F> for Gain {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let size = io.frames();
        let driven = self
            .declared_param::<{ UnitParam::Volume as u16 }>(&io)
            .frames();
        let cell = self.0.load().get();
        let (ins, mut outs) = io.split();
        for (n, (o, i)) in outs
            .get(0)
            .iter_mut()
            .zip(ins.get(0))
            .enumerate()
            .take(size)
        {
            // A lane's frames are `f32` words; the product is in `F`.
            let gain = driven.map_or(cell, |lane| f64::from(lane[n]));
            *o = *i * F::from_f64(gain);
        }
        Status::Modified
    }
    fn reset(&mut self) {}

    fn param_base(&self, k: usize) -> Option<f32> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for Gain {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.0.detach();
        fork
    }
}

tutti_graph::declare_params!([] Gain, n { Volume => Amplitude = &n.0 });
