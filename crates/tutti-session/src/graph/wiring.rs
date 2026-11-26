//! What a wire is checked against: the graph, and the shapes of nodes a patch
//! is about to place (or swap in) that it does not hold yet.

use tutti_buffers::Float;
use tutti_graph::{
    BusError, BusOutputPort, Buses, EventInputPort, EventOutputPort, NodeKey, Shape, Shapes,
};

use crate::key::{BusEnd, BusInputPort, InEnd, MainSink};
use crate::{ConnectError, InputPort, MainOutput, NoSuchNode, OutputPort};

use super::Graph;

/// Shapes standing in for the ones the graph holds at the same keys: the
/// nodes a patch declares and has not placed yet. `None` is the graph as it
/// stands.
pub(crate) type Pending<'a> = Option<&'a Shapes>;

impl<F: Float> Graph<F> {
    fn contains_in(&self, pending: Pending<'_>, node: NodeKey) -> bool {
        pending.is_some_and(|p| p.contains_key(&node)) || self.contains(node)
    }

    fn shape_in(&self, pending: Pending<'_>, node: NodeKey) -> Option<Shape> {
        pending
            .and_then(|p| p.get(&node).copied())
            .or_else(|| self.shape(node))
    }

    fn shape_of_in(&self, pending: Pending<'_>, node: NodeKey) -> Result<Shape, NoSuchNode> {
        self.contains_in(pending, node)
            .then(|| self.shape_in(pending, node))
            .flatten()
            .ok_or(NoSuchNode::new(node))
    }

    fn node_inputs_in(&self, pending: Pending<'_>, node: NodeKey) -> Result<usize, NoSuchNode> {
        self.shape_of_in(pending, node)
            .map(|s| s.audio_in().count() as usize)
    }

    fn node_outputs_in(&self, pending: Pending<'_>, node: NodeKey) -> Result<usize, NoSuchNode> {
        self.shape_of_in(pending, node)
            .map(|s| s.audio_out().count() as usize)
    }

    pub(crate) fn node_buses_in(
        &self,
        pending: Pending<'_>,
        node: NodeKey,
    ) -> Result<(Buses, Buses), NoSuchNode> {
        self.shape_of_in(pending, node)
            .map(|s| (*s.buses_in(), *s.buses_out()))
    }

    /// The first of the bus wires `edges`, in order, that the graph's lowering
    /// refuses once every one up to it is declared, with why.
    pub(crate) fn first_unlowered(
        &self,
        pending: Pending<'_>,
        edges: &[(BusOutputPort, BusInputPort)],
    ) -> Option<(usize, BusError)> {
        let mut shapes = self.editor.shapes().clone();
        shapes.extend(pending.into_iter().flatten().map(|(k, s)| (*k, *s)));
        let mut spec = self.editor.spec().clone();
        for (at, (from, to)) in edges.iter().enumerate() {
            match to.0 {
                BusEnd::Node(bus) => spec.connect_bus(*from, bus),
                BusEnd::MainOutput => spec.connect_bus_to_main_output(*from),
            };
            if let Err(cause) = spec.lowered(&shapes) {
                return Some((at, cause));
            }
        }
        None
    }

    /// Whether `port` names an event output the graph has.
    pub(crate) fn check_event_out(
        &self,
        pending: Pending<'_>,
        port: EventOutputPort,
    ) -> Result<(), ConnectError> {
        if !self.contains_in(pending, port.node()) {
            return Err(ConnectError::NoSuchNode(port.node()));
        }
        let fits = self
            .shape_in(pending, port.node())
            .is_some_and(|s| port.port() < s.event_out);
        fits.then_some(()).ok_or(ConnectError::NoSuchEventOut(port))
    }

    /// Whether `port` names an event input the graph has.
    pub(crate) fn check_event_in(
        &self,
        pending: Pending<'_>,
        port: EventInputPort,
    ) -> Result<(), ConnectError> {
        if !self.contains_in(pending, port.node()) {
            return Err(ConnectError::NoSuchNode(port.node()));
        }
        let fits = self
            .shape_in(pending, port.node())
            .is_some_and(|s| port.port() < s.event_in);
        fits.then_some(()).ok_or(ConnectError::NoSuchEventIn(port))
    }

    pub(crate) fn check_event_wire(
        &self,
        pending: Pending<'_>,
        from: EventOutputPort,
        to: EventInputPort,
    ) -> Result<(), ConnectError> {
        self.check_event_out(pending, from)?;
        self.check_event_in(pending, to)?;
        if from.node() == to.node() {
            return Err(ConnectError::Itself(from.node()));
        }
        Ok(())
    }

    pub(crate) fn check_out(
        &self,
        pending: Pending<'_>,
        port: OutputPort,
    ) -> Result<(), ConnectError> {
        let node = port.node();
        if !self.contains_in(pending, node) {
            return Err(ConnectError::NoSuchNode(node));
        }
        (port.index() < self.node_outputs_in(pending, node)?)
            .then_some(())
            .ok_or(ConnectError::NoSuchOutPort(port))
    }

    pub(crate) fn check_in(
        &self,
        pending: Pending<'_>,
        port: InputPort,
    ) -> Result<(), ConnectError> {
        let fits = match port.end() {
            InEnd::Node(node, index) => {
                if !self.contains_in(pending, node) {
                    return Err(ConnectError::NoSuchNode(node));
                }
                index < self.node_inputs_in(pending, node)?
            }
            InEnd::Output(index) => index < self.outputs(),
        };
        fits.then_some(()).ok_or(ConnectError::NoSuchInPort(port))
    }

    /// Checks one wire from `from` into `to`: both ends exist, and a node does
    /// not feed itself.
    pub(crate) fn check_wire(
        &self,
        pending: Pending<'_>,
        from: OutputPort,
        to: InputPort,
    ) -> Result<(), ConnectError> {
        self.check_out(pending, from)?;
        self.check_in(pending, to)?;
        match (from.node(), to.node()) {
            (a, Some(b)) if a == b => Err(ConnectError::Itself(a)),
            _ => Ok(()),
        }
    }

    /// Whether the nodes a bus edge names are in the graph.
    pub(crate) fn check_bus_nodes(
        &self,
        pending: Pending<'_>,
        from: BusOutputPort,
        to: BusInputPort,
    ) -> Result<(), ConnectError> {
        if let BusOutputPort::Node(bus) = from
            && !self.contains_in(pending, bus.node())
        {
            return Err(ConnectError::NoSuchNode(bus.node()));
        }
        if let BusEnd::Node(bus) = to.0
            && !self.contains_in(pending, bus.node())
        {
            return Err(ConnectError::NoSuchNode(bus.node()));
        }
        Ok(())
    }

    /// The `(output port, input port)` pairs that connect `from`'s main
    /// output to `to`'s main input (or the main output's outputs), checked.
    pub(crate) fn main_wires(
        &self,
        pending: Pending<'_>,
        from: NodeKey,
        to: MainSink,
    ) -> Result<Vec<(OutputPort, InputPort)>, ConnectError> {
        let (_, out_buses) = self.node_buses_in(pending, from)?;
        let main = |buses: &Buses, node| {
            let (index, bus) = buses.main_bus().ok_or(ConnectError::NoMainBus(node))?;
            let first = buses
                .first_port(index)
                .ok_or(ConnectError::NoMainBus(node))?;
            Ok::<_, ConnectError>((first, bus.width()))
        };
        let (out_first, out_width) = main(&out_buses, from)?;
        let out_topology = out_buses
            .main_bus()
            .map(|(_, bus)| bus.topology)
            .ok_or(ConnectError::NoMainBus(from))?;
        match to {
            MainSink::Node(to) => {
                if from == to {
                    return Err(ConnectError::Itself(from));
                }
                let (in_buses, _) = self.node_buses_in(pending, to)?;
                let (in_first, in_width) = main(&in_buses, to)?;
                if out_width != in_width {
                    return Err(ConnectError::Width {
                        from,
                        from_width: out_width,
                        to,
                        to_width: in_width,
                    });
                }
                Ok((0..out_width)
                    .map(|c| {
                        (
                            OutputPort::new(from, out_first + c),
                            InputPort::new(to, in_first + c),
                        )
                    })
                    .collect())
            }
            MainSink::MainOutput => {
                let main_output_width = self.outputs();
                if out_width > main_output_width {
                    return Err(ConnectError::MainOutputNarrow {
                        from,
                        from_width: out_width,
                        main_output_width,
                    });
                }
                if out_width == main_output_width {
                    return Ok((0..out_width)
                        .map(|c| (OutputPort::new(from, out_first + c), MainOutput::channel(c)))
                        .collect());
                }
                // A narrower bus is placed on the main output by speaker: the main
                // output's declared topology. A bus that names no speaker is
                // read in the engine's order for its width.
                let source = out_topology;
                let placed = if source.is_unassigned() {
                    tutti_buffers::ChannelTopology::default_for(source.width())
                        .expect("a bus is at most MAX_CHANNELS wide")
                } else {
                    source
                };
                let fold =
                    tutti_buffers::Fold::<f32>::between(&placed, self.main_output_topology())
                        .map_err(|cause| ConnectError::MainOutputFold { from, cause })?;
                let mut wires = Vec::new();
                for c in 0..main_output_width {
                    for (k, weight) in fold.contributions(c) {
                        if weight != 1.0 {
                            return Err(ConnectError::MainOutputWeighted { from });
                        }
                        wires.push((OutputPort::new(from, out_first + k), MainOutput::channel(c)));
                    }
                }
                Ok(wires)
            }
        }
    }
}
