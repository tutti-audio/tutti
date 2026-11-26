//! A declared wire resolved onto the graph's own wire kinds: the keys of a
//! patch's ends become nodes, and the ports they name become the ports
//! [`Graph::connect`] takes.

use tutti_buffers::Float;
use tutti_graph::{BusOutputPort, EventInputPort, EventOutputPort, NodeBus, NodeKey};
use tutti_units::Gain;

use crate::graph::Pending;
use crate::key::{BusEnd, Inlet, MainSink, Outlet};
use crate::patch::wire::sealed::end::Sealed;
use crate::patch::{Chain, InPort, InputEnd, OutPort, OutputEnd, WireDecl, WireEnd};
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

/// Why a wire has no graph wire: its ends are not one kind, or the graph's own
/// refusal.
pub(crate) enum Refusal {
    Kinds,
    Connect(ConnectError),
}

impl From<ConnectError> for Refusal {
    fn from(cause: ConnectError) -> Self {
        Self::Connect(cause)
    }
}

impl From<crate::NoSuchNode> for Refusal {
    fn from(missing: crate::NoSuchNode) -> Self {
        Self::Connect(missing.into())
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
    /// The wire between `from`'s output `out` and `to`'s input `port` (`None`
    /// is the main output), on the kind the two ends share. A main bus joins
    /// a bus or a channel end by that bus's or the first channel of its main
    /// bus.
    pub(crate) fn of<F: Float>(
        graph: &Graph<F>,
        pending: Pending<'_>,
        from: NodeKey,
        out: OutPort,
        to: Option<NodeKey>,
        port: InPort,
    ) -> Result<Self, Refusal> {
        let main_in = |node: NodeKey| -> Result<usize, Refusal> {
            let (buses, _) = graph.node_buses_in(pending, node)?;
            let (index, _) = buses.main_bus().ok_or(ConnectError::NoMainBus(node))?;
            Ok(buses
                .first_port(index)
                .ok_or(ConnectError::NoMainBus(node))?)
        };
        Ok(match (out, to, port) {
            (OutPort::Main, Some(to), InPort::Main) => Self::Main(from, MainSink::Node(to)),
            (OutPort::Main, None, InPort::Main) => Self::Main(from, MainSink::MainOutput),
            (OutPort::Channel(i), Some(to), InPort::Channel(j)) => {
                Self::Channel(OutputPort::new(from, i), InputPort::new(to, j))
            }
            (OutPort::Channel(i), Some(to), InPort::Main) => {
                Self::Channel(OutputPort::new(from, i), InputPort::new(to, main_in(to)?))
            }
            (OutPort::Event(i), Some(to), InPort::Event(j)) => {
                Self::Event(EventOutputPort::new(from, i), EventInputPort::new(to, j))
            }
            (OutPort::Bus(b), Some(to), InPort::Bus(c)) => Self::Bus(
                BusOutputPort::Node(NodeBus::new(from, b)),
                BusInputPort::new(to, c),
            ),
            (OutPort::Bus(b), None, InPort::Main) => Self::Bus(
                BusOutputPort::Node(NodeBus::new(from, b)),
                BusInputPort(BusEnd::MainOutput),
            ),
            (OutPort::Main, Some(to), InPort::Bus(c)) => {
                let (_, buses) = graph.node_buses_in(pending, from)?;
                let (index, _) = buses.main_bus().ok_or(ConnectError::NoMainBus(from))?;
                let bus = u8::try_from(index).map_err(|_| ConnectError::NoMainBus(from))?;
                Self::Bus(
                    BusOutputPort::Node(NodeBus::new(from, bus)),
                    BusInputPort::new(to, c),
                )
            }
            _ => return Err(Refusal::Kinds),
        })
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

    /// The bus wire's ends, for the lowering check.
    pub(crate) fn bus(self) -> Option<(BusOutputPort, BusInputPort)> {
        match self {
            Self::Bus(o, i) => Some((o, i)),
            _ => None,
        }
    }

    /// Whether the wire's edges carry a gain of their own.
    pub(crate) fn weighted(self) -> bool {
        matches!(self, Self::Channel(..) | Self::Main(..))
    }

    /// The audio edges the wire is: one channel, one per channel of a main
    /// bus, none for a bus or an event wire.
    pub(crate) fn edges<F: Float>(self, graph: &Graph<F>) -> Vec<ChannelEdge> {
        match self {
            Self::Channel(o, i) => vec![ChannelEdge::new(o, i)],
            Self::Main(from, to) => graph
                .main_wires(None, from, to)
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

/// The key-level ends of a wire, the identity of a kept wire.
pub(crate) type Ends<K> = (OutputEnd<K>, InputEnd<K>);

/// A wire into the graph's main output: a connection, as into any port.
impl<K> Sealed<K> for MainOutput {
    type Joined = crate::Connected<K>;
    fn end(self) -> InputEnd<K> {
        InputEnd {
            key: None,
            port: InPort::Main,
        }
    }
    fn joined(chain: Chain<K>) -> crate::Connected<K> {
        chain.ended()
    }
}

impl<K> WireEnd<K> for MainOutput {}
