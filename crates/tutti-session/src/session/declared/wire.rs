//! A declared wire resolved onto the graph's own wire kinds: the keys of a
//! patch's ends become nodes, and the ports they name become the ports
//! [`Graph::connect`] takes.

use tutti_buffers::Float;
use tutti_graph::{
    BusOutputPort, CycleEdge, EdgeId, EventInputPort, EventOutputPort, GraphSpec, NodeBus, NodeKey,
    PortSink,
};
use tutti_units::{Gain, Samples};

use crate::graph::Pending;
use crate::key::{BusEnd, Inlet, MainSink, Outlet};
use crate::patch::{WireDecl, WireEnds, WireId};
use crate::{
    BusInputPort, ChannelEdge, ChannelWire, ConnectError, Connection, EventEdge, Graph, InputPort,
    MainBusWire, MainOutput, OutputPort, Wire,
};

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

/// The pair of ports an edge joins, whatever the edge's kind: what a pair of
/// ports holds at most one edge of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ports {
    Audio(ChannelEdge),
    Event(EventEdge),
}

impl Ports {
    /// The pairs of ports `wire`'s edges join: none for a bus wire.
    pub(crate) fn of<F: Float>(
        wire: Resolved,
        graph: &Graph<F>,
        pending: Pending<'_>,
    ) -> Vec<Self> {
        match wire {
            Resolved::Event(from, to, _) => vec![Self::Event(EventEdge::new(from, to))],
            Resolved::Channel(..) | Resolved::Main(..) => wire
                .edges(graph, pending)
                .into_iter()
                .map(Self::Audio)
                .collect(),
            Resolved::Bus(..) => Vec::new(),
        }
    }

    /// The delay of the edge the graph holds on these ports: `None` for no
    /// edge, `Some(None)` for a direct one.
    pub(crate) fn held_delay<F: Float>(self, graph: &Graph<F>) -> Option<Option<Samples>> {
        match self {
            Self::Audio(edge) => graph.held_delay(edge),
            Self::Event(edge) => graph.held_event_delay(edge.source(), edge.sink()),
        }
    }
}

/// Which node a key names at one end of a wire: the node itself, or, for an
/// instrument's audio leaving it, the strip after its main pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Node,
    Strip,
}

/// A wire as the graph names it, with its feedback delay (`None` for a direct
/// wire). A bus wire has no delayed edge.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Resolved {
    Channel(OutputPort, InputPort, Option<Samples>),
    Main(NodeKey, MainSink, Option<Samples>),
    Bus(BusOutputPort, BusInputPort),
    Event(EventOutputPort, EventInputPort, Option<Samples>),
}

impl Resolved {
    /// The graph's wire for `id`, its keys named by `node`: audio leaves an
    /// instrument from its strip, and everything else is the node.
    pub(crate) fn of<K>(id: &WireId<K>, node: impl Fn(&K, Role) -> NodeKey) -> Self {
        let delay = id.delay;
        let out = |key: &K| node(key, Role::Strip);
        let at = |key: &K| node(key, Role::Node);
        match &id.ends {
            WireEnds::Main(from, to) => Self::Main(
                out(from),
                to.as_ref()
                    .map_or(MainSink::MainOutput, |to| MainSink::Node(at(to))),
                delay,
            ),
            WireEnds::Channel(from, port, to, input) => Self::Channel(
                OutputPort::new(out(from), *port),
                InputPort::new(at(to), *input),
                delay,
            ),
            WireEnds::Event(from, port, to, input) => Self::Event(
                EventOutputPort::new(at(from), *port),
                EventInputPort::new(at(to), *input),
                delay,
            ),
            WireEnds::Bus(from, port, to) => Self::Bus(
                BusOutputPort::Node(NodeBus::new(out(from), *port)),
                to.as_ref()
                    .map_or(BusInputPort(BusEnd::MainOutput), |(to, bus)| {
                        BusInputPort::new(at(to), *bus)
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
            Self::Channel(o, i, _) => graph.check_wire(pending, o, i),
            Self::Main(from, to, _) => graph.main_wires(pending, from, to).map(drop),
            Self::Bus(o, i) => graph.check_bus_nodes(pending, o, i),
            Self::Event(o, i, _) => graph.check_event_wire(pending, o, i),
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
            Self::Channel(o, i, _) => vec![ChannelEdge::new(o, i)],
            Self::Main(from, to, _) => graph
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

    /// The feedback delay of the wire, `None` for a direct one.
    fn delay(self) -> Option<Samples> {
        match self {
            Self::Channel(.., delay) | Self::Main(.., delay) | Self::Event(.., delay) => delay,
            Self::Bus(..) => None,
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
                    }) = Graph::<F>::edge_id(edge, self.delay())
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
            Self::Event(from, at, delay) => {
                if connect {
                    spec.connect_events(
                        at,
                        match delay {
                            None => tutti_graph::EventEdge::Direct(from),
                            Some(delay) => tutti_graph::EventEdge::Feedback { from, delay },
                        },
                    );
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

    /// Whether the graph holds the wire: every edge of it, of its delay, or
    /// for a bus wire the edge itself.
    pub(crate) fn held<F: Float>(self, graph: &Graph<F>) -> bool {
        match self {
            Self::Channel(..) | Self::Main(..) | Self::Event(..) => {
                let ports = Ports::of(self, graph, None);
                !ports.is_empty()
                    && ports
                        .into_iter()
                        .all(|ports| ports.held_delay(graph) == Some(self.delay()))
            }
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
            Self::Channel(o, i, d) => {
                let edges = graph.add_channel_wire(o, i, d)?;
                finished(
                    Connection::<ChannelWire, F>::new(graph, edges, o, i),
                    unfaded,
                )
            }
            Self::Main(o, to, d) => {
                let edges = graph.add_main_wire(o, to, d)?;
                finished(
                    Connection::<MainBusWire, F>::new(graph, edges, o, to),
                    unfaded,
                )
            }
            Self::Bus(o, i) => finished(graph.connect(o, i)?, unfaded),
            Self::Event(o, i, d) => {
                graph.add_event_edge(o, i, d)?;
                Vec::new()
            }
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
            Self::Channel(o, i, _) => cut(graph, o, i, unfaded),
            Self::Main(o, MainSink::Node(i), _) => cut(graph, o, i, unfaded),
            Self::Main(o, MainSink::MainOutput, _) => cut(graph, o, MainOutput, unfaded),
            Self::Bus(o, i) => cut(graph, o, i, unfaded),
            Self::Event(o, i, _) => cut(graph, o, i, unfaded),
        }
    }
}

/// The edges of `connection`, hardened when `unfaded`.
fn finished<F: Float, W: Wire>(
    connection: Connection<'_, W, F>,
    unfaded: bool,
) -> Vec<ChannelEdge> {
    let connection = if unfaded {
        connection.unfaded()
    } else {
        connection
    };
    connection.edges()
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
