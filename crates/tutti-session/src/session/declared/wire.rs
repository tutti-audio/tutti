//! A declared wire resolved onto the graph's own wire kinds: the keys of a
//! patch's ends become nodes, and the ports they name become the ports
//! [`Graph::connect`] takes.

use tutti_buffers::Float;
use tutti_graph::{
    BusOutputPort, CycleEdge, EdgeId, EventInputPort, EventOutputPort, GraphSpec, NodeBus, NodeKey,
    PortSink,
};
use tutti_units::Gain;

use crate::graph::Pending;
use crate::key::{BusEnd, Inlet, MainSink, Outlet};
use crate::patch::{WireDecl, WireEnds};
use crate::{BusInputPort, ChannelEdge, ConnectError, Graph, InputPort, MainOutput, OutputPort};

/// What a patch last made of one wire: the options it was declared with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeptWire {
    pub(crate) unfaded: bool,
    pub(crate) initial: Option<Gain>,
}

impl KeptWire {
    pub(crate) fn of<K>(wire: &WireDecl<K>) -> Self {
        Self {
            unfaded: wire.unfaded,
            initial: wire.initial,
        }
    }
}

/// A wire as the graph names it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Resolved {
    Channel(OutputPort, InputPort),
    Main(NodeKey, MainSink),
    Bus(BusOutputPort, BusInputPort),
    Event(EventOutputPort, EventInputPort),
}

impl Resolved {
    /// The graph's wire for `ends`, its keys named by `node`.
    pub(crate) fn of<K>(ends: &WireEnds<K>, node: impl Fn(&K) -> NodeKey) -> Self {
        match ends {
            WireEnds::Main(from, to) => Self::Main(
                node(from),
                to.as_ref()
                    .map_or(MainSink::MainOutput, |to| MainSink::Node(node(to))),
            ),
            WireEnds::Channel(from, out, to, input) => Self::Channel(
                OutputPort::new(node(from), *out),
                InputPort::new(node(to), *input),
            ),
            WireEnds::Event(from, out, to, input) => Self::Event(
                EventOutputPort::new(node(from), *out),
                EventInputPort::new(node(to), *input),
            ),
            WireEnds::Bus(from, out, to) => Self::Bus(
                BusOutputPort::Node(NodeBus::new(node(from), *out)),
                to.as_ref()
                    .map_or(BusInputPort(BusEnd::MainOutput), |(to, bus)| {
                        BusInputPort::new(node(to), *bus)
                    }),
            ),
        }
    }

    /// Whether the graph would take the wire, with nothing changed.
    pub(crate) fn check<F: Float>(
        self,
        graph: &Graph<F>,
        pending: Pending<'_>,
    ) -> Result<(), ConnectError> {
        match self {
            Self::Channel(o, i) => graph.check_wire(pending, o, i),
            Self::Main(from, to) => graph.main_wires(pending, from, to).map(drop),
            Self::Bus(o, i) => graph.check_bus_nodes(pending, o, i),
            Self::Event(o, i) => graph.check_event_wire(pending, o, i),
        }
    }

    /// The audio edges the wire is: one channel, one per channel of a main
    /// bus, none for a bus or an event wire.
    pub(crate) fn edges<F: Float>(
        self,
        graph: &Graph<F>,
        pending: Pending<'_>,
    ) -> Vec<ChannelEdge> {
        match self {
            Self::Channel(o, i) => vec![ChannelEdge::new(o, i)],
            Self::Main(from, to) => graph
                .main_wires(pending, from, to)
                .map(|wires| {
                    wires
                        .into_iter()
                        .map(|(o, i)| ChannelEdge::new(o, i))
                        .collect()
                })
                .unwrap_or_default(),
            Self::Bus(..) | Self::Event(..) => Vec::new(),
        }
    }

    /// Writes the wire into `spec` (or takes it out), and names the edges it
    /// is there: what `GraphSpec::cycle` reports a cycle by.
    pub(crate) fn in_spec<F: Float>(
        self,
        spec: &mut GraphSpec,
        graph: &Graph<F>,
        pending: Pending<'_>,
        connect: bool,
    ) -> Vec<CycleEdge> {
        let mut named = Vec::new();
        match self {
            Self::Channel(..) | Self::Main(..) => {
                for edge in self.edges(graph, pending) {
                    let Some(EdgeId {
                        sink: PortSink::Port(at),
                        from,
                    }) = Graph::<F>::edge_id(edge)
                    else {
                        continue;
                    };
                    if connect {
                        spec.connect(at, from);
                    } else {
                        spec.disconnect(at, from);
                    }
                    named.push(CycleEdge::Audio(at));
                }
            }
            Self::Event(from, at) => {
                if connect {
                    spec.connect_events(at, tutti_graph::EventEdge::Direct(from));
                } else {
                    spec.disconnect_events(at, from);
                }
                named.push(CycleEdge::Event { at, from });
            }
            Self::Bus(from, to) => match to.0 {
                BusEnd::Node(at) => {
                    if connect {
                        spec.connect_bus(from, at);
                    } else {
                        spec.disconnect_bus(from, at);
                    }
                    named.push(CycleEdge::Bus { at, from });
                }
                BusEnd::MainOutput => {
                    if connect {
                        spec.connect_bus_to_main_output(from);
                    } else {
                        spec.disconnect_bus_from_main_output(from);
                    }
                }
            },
        }
        named
    }

    /// Whether the graph holds the wire: every edge of it, or for an event or
    /// bus wire the edge itself.
    pub(crate) fn held<F: Float>(self, graph: &Graph<F>) -> bool {
        match self {
            Self::Channel(..) | Self::Main(..) => {
                let edges = self.edges(graph, None);
                !edges.is_empty() && edges.into_iter().all(|edge| graph.holds_edge(edge))
            }
            Self::Event(from, to) => graph
                .sources(to)
                .is_ok_and(|sources| sources.contains(&from)),
            Self::Bus(from, to) => graph.holds_bus_edge(from, to),
        }
    }

    /// Connects the wire, with no fade when `unfaded`, and a gain lane at
    /// `initial` on each of its edges.
    pub(crate) fn connect<F: Float>(
        self,
        graph: &mut Graph<F>,
        unfaded: bool,
        initial: Option<Gain>,
    ) -> Result<(), ConnectError> {
        let edges = match self {
            Self::Channel(o, i) => made(graph, o, i, unfaded)?,
            Self::Main(o, MainSink::Node(i)) => made(graph, o, i, unfaded)?,
            Self::Main(o, MainSink::MainOutput) => made(graph, o, MainOutput, unfaded)?,
            Self::Bus(o, i) => made(graph, o, i, unfaded)?,
            Self::Event(o, i) => made(graph, o, i, unfaded)?,
        };
        if let Some(gain) = initial {
            for edge in edges {
                graph
                    .declare_edge_gain(edge, gain)
                    .expect("a declared initial gain is finite");
            }
        }
        Ok(())
    }

    /// Takes the wire out, with no fade when `unfaded`.
    pub(crate) fn disconnect<F: Float>(
        self,
        graph: &mut Graph<F>,
        unfaded: bool,
    ) -> Result<(), ConnectError> {
        match self {
            Self::Channel(o, i) => cut(graph, o, i, unfaded),
            Self::Main(o, MainSink::Node(i)) => cut(graph, o, i, unfaded),
            Self::Main(o, MainSink::MainOutput) => cut(graph, o, MainOutput, unfaded),
            Self::Bus(o, i) => cut(graph, o, i, unfaded),
            Self::Event(o, i) => cut(graph, o, i, unfaded),
        }
    }
}

fn made<F: Float, O: Outlet, T: Inlet<O::Wire>>(
    graph: &mut Graph<F>,
    from: O,
    to: T,
    unfaded: bool,
) -> Result<Vec<ChannelEdge>, ConnectError> {
    let connection = graph.connect(from, to)?;
    let connection = if unfaded {
        connection.unfaded()
    } else {
        connection
    };
    Ok(connection.edges())
}

fn cut<F: Float, O: Outlet, T: Inlet<O::Wire>>(
    graph: &mut Graph<F>,
    from: O,
    to: T,
    unfaded: bool,
) -> Result<(), ConnectError> {
    let cut = graph.disconnect(from, to)?;
    if unfaded {
        let _ = cut.unfaded();
    }
    Ok(())
}
