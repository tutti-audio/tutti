//! The nodes of a loop: one that adds a unit impulse to its first frame, and
//! one that passes its input on, so a loop through both echoes the impulse.

#![allow(dead_code, reason = "each area binary uses the nodes it needs")]

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_units::Hz;

/// A pass-through that adds a unit impulse to its first frame: what a loop
/// through it echoes.
#[derive(Clone)]
pub struct Echo {
    layout: ChannelLayout,
    fired: bool,
    cells: EchoCells,
}

declare_params!([] Echo, EchoConfig, EchoCells {
    required { layout: ChannelLayout }
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
});

impl From<EchoConfig> for Echo {
    fn from(config: EchoConfig) -> Self {
        Self {
            layout: config.layout,
            fired: false,
            cells: EchoCells::from_config(&config),
        }
    }
}

impl Node for Echo {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.layout).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        outs.get(0).copy_from_slice(ins.get(0));
        if !self.fired {
            outs.get(0)[0] += 1.0;
            self.fired = true;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// A pass-through: the other node of a loop.
#[derive(Clone)]
pub struct Relay {
    layout: ChannelLayout,
    cells: RelayCells,
}

declare_params!([] Relay, RelayConfig, RelayCells {
    required { layout: ChannelLayout }
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
});

impl From<RelayConfig> for Relay {
    fn from(config: RelayConfig) -> Self {
        Self {
            layout: config.layout,
            cells: RelayCells::from_config(&config),
        }
    }
}

impl Node for Relay {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.layout).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        outs.get(0).copy_from_slice(ins.get(0));
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}
