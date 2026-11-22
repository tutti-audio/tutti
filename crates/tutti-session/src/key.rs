//! Typed keys, ports and the one wiring vocabulary of the session's
//! [`Graph`].
//!
//! - [`Key<N>`] is a node's identity with its type: [`Graph::add`] returns one, and every call that takes a node takes
//!   `impl Into<NodeKey>`, so a [`Key`] and an erased [`NodeKey`] both work.
//! - [`OutputPort`] and [`InputPort`] name one channel: a node's output or input
//!   port (`key.output(0)`, `key.input(1)`), a main input channel
//!   ([`MainInput::channel`]) or a main output channel ([`MainOutput::channel`]).
//! - [`EventOutputPort`] and [`EventInputPort`] name one event port
//!   (`key.event_output(0)`, `key.event_input(1)`); **event input 0 is the
//!   node's main event input**, where the chase (`Node::chase`) delivers.
//! - [`Graph::connect`] takes an [`Outlet`] and an [`Inlet`] of the same kind
//!   of wire: one channel, a node's whole main bus, a bus edge, or an event
//!   edge (never faded: a note-off is a message, not a level).

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use tutti_buffers::Float;
use tutti_graph::{BusOutputPort, EdgeId, EventInputPort, EventOutputPort, NodeBus, NodeKey};
use tutti_units::Amplitude;

use crate::{ConnectError, DeclarationError, Graph};

mod sealed {
    pub trait Sealed {}
}
use sealed::Sealed;

/// A node's key, with the node's type: the handle [`Graph::add`] returns.
///
/// It is a [`NodeKey`] and nothing more at run time (`Copy`, `Eq`, `Hash`),
/// so a key can be kept, compared and sent anywhere a node is taken
/// (`impl Into<NodeKey>`). The type is what makes a node's own params and
/// ports methods of its key rather than numbers.
pub struct Key<N: ?Sized> {
    node: NodeKey,
    kind: PhantomData<fn() -> N>,
}

impl<N: ?Sized> Key<N> {
    pub(crate) const fn new(node: NodeKey) -> Self {
        Self {
            node,
            kind: PhantomData,
        }
    }

    /// The key without its type.
    #[must_use]
    pub const fn erase(self) -> NodeKey {
        self.node
    }

    /// Output port `index` of the node.
    #[must_use]
    pub const fn output(self, index: usize) -> OutputPort {
        OutputPort(OutEnd::Node(self.node, index))
    }

    /// Input port `index` of the node.
    #[must_use]
    pub const fn input(self, index: usize) -> InputPort {
        InputPort(InEnd::Node(self.node, index))
    }

    /// Event output `index` of the node.
    #[must_use]
    pub const fn event_output(self, index: u16) -> EventOutputPort {
        EventOutputPort {
            node: self.node,
            port: index,
        }
    }

    /// Event input `index` of the node (a port per MIDI Group, a plugin's note
    /// port `N`).
    ///
    /// **Event input 0 is the main event input**: the chase (`Node::chase`),
    /// the state a node is given when it takes over from another, is
    /// delivered there.
    #[must_use]
    pub const fn event_input(self, index: u16) -> EventInputPort {
        EventInputPort {
            node: self.node,
            port: index,
        }
    }

    /// Output bus `bus` of the node, as a source of a bus edge.
    #[must_use]
    pub const fn bus_output(self, bus: u8) -> BusOutputPort {
        BusOutputPort::Node(NodeBus::new(self.node, bus))
    }

    /// Input bus `bus` of the node, as a sink of a bus edge.
    #[must_use]
    pub const fn bus_input(self, bus: u8) -> BusInputPort {
        BusInputPort::node(NodeBus::new(self.node, bus))
    }
}

impl<N: ?Sized> Clone for Key<N> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<N: ?Sized> Copy for Key<N> {}
impl<N: ?Sized> PartialEq for Key<N> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}
impl<N: ?Sized> Eq for Key<N> {}
impl<N: ?Sized> PartialOrd for Key<N> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<N: ?Sized> Ord for Key<N> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.node.cmp(&other.node)
    }
}
impl<N: ?Sized> Hash for Key<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.node.hash(state);
    }
}
impl<N: ?Sized> fmt::Debug for Key<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key<{}>({})", std::any::type_name::<N>(), self.node.0)
    }
}
impl<N: ?Sized> From<Key<N>> for NodeKey {
    fn from(key: Key<N>) -> Self {
        key.node
    }
}

/// One channel a signal leaves from: an output port of a node, or a channel
/// of the graph's main input. Made by [`Key::out`], [`OutputPort::new`]
/// and [`MainInput::channel`].
///
/// The index is a port number of the node's declared ports, a count and not
/// a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputPort(OutEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum OutEnd {
    Node(NodeKey, usize),
    Input(usize),
}

impl OutputPort {
    /// Output port `index` of `node`.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, index: usize) -> Self {
        Self(OutEnd::Node(node.into(), index))
    }

    /// The node it leaves, `None` for a main input channel.
    #[must_use]
    pub const fn node(self) -> Option<NodeKey> {
        match self.0 {
            OutEnd::Node(node, _) => Some(node),
            OutEnd::Input(_) => None,
        }
    }

    /// The port's index on its node, or the main input channel.
    #[must_use]
    pub const fn index(self) -> usize {
        match self.0 {
            OutEnd::Node(_, index) | OutEnd::Input(index) => index,
        }
    }

    pub(crate) const fn end(self) -> OutEnd {
        self.0
    }

    /// The port the graph's `source` names, `None` for explicit silence.
    pub(crate) fn of_source(source: tutti_graph::PortSource) -> Option<Self> {
        match source {
            tutti_graph::PortSource::Node(p) => Some(Self::new(p.node(), usize::from(p.port()))),
            tutti_graph::PortSource::MainInput(c) => Some(MainInput::channel(usize::from(c))),
            tutti_graph::PortSource::Zero => None,
        }
    }
}

/// One channel a signal arrives at: an input port of a node, or a channel of
/// the graph's main output. Made by [`Key::input`],
/// [`InputPort::new`] and [`MainOutput::channel`].
///
/// The index is a port number of the node's declared ports, a count and not
/// a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputPort(InEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum InEnd {
    Node(NodeKey, usize),
    Output(usize),
}

impl InputPort {
    /// Input port `index` of `node`.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, index: usize) -> Self {
        Self(InEnd::Node(node.into(), index))
    }

    /// The node it arrives at, `None` for a main output channel.
    #[must_use]
    pub const fn node(self) -> Option<NodeKey> {
        match self.0 {
            InEnd::Node(node, _) => Some(node),
            InEnd::Output(_) => None,
        }
    }

    /// The port's index on its node, or the main output channel.
    #[must_use]
    pub const fn index(self) -> usize {
        match self.0 {
            InEnd::Node(_, index) | InEnd::Output(index) => index,
        }
    }

    pub(crate) const fn end(self) -> InEnd {
        self.0
    }

    /// The port the graph's `sink` names.
    pub(crate) fn of_sink(sink: tutti_graph::PortSink) -> Self {
        match sink {
            tutti_graph::PortSink::Port(p) => Self::new(p.node(), usize::from(p.port())),
            tutti_graph::PortSink::Output(c) => MainOutput::channel(usize::from(c)),
        }
    }
}

/// The graph's main output: the channels the device or the render reads.
///
/// A value of its own rather than a method of [`Graph`], so naming a channel
/// borrows nothing: `s.graph().connect(osc.output(0), MainOutput::channel(0))`
/// is one borrow of the session. As an [`Inlet`] it is the sink of a node's
/// main bus ([`Graph::connect`] channel by channel onto the outputs) and of a
/// bus edge (folded to the topology the main output was declared with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MainOutput;

impl MainOutput {
    /// Main output channel `index`, as a sink.
    #[must_use]
    pub const fn channel(index: usize) -> InputPort {
        InputPort(InEnd::Output(index))
    }
}

/// The graph's main input: the hardware or host input channels.
///
/// A value of its own, like [`MainOutput`], so naming a channel borrows
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MainInput;

impl MainInput {
    /// Main input channel `index` (a hardware or host input), as a source.
    #[must_use]
    pub const fn channel(index: usize) -> OutputPort {
        OutputPort(OutEnd::Input(index))
    }
}

/// One event edge, named: what feeds which event input. What
/// [`Graph::connect`] adds between an [`EventOutputPort`] and an
/// [`EventInputPort`], and what [`SessionEvent::Unwired`](crate::SessionEvent::Unwired)
/// lists for a node that lost an event port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventEdge {
    from: EventOutputPort,
    to: EventInputPort,
}

impl EventEdge {
    /// The edge from `from` into `to`.
    #[must_use]
    pub const fn new(from: EventOutputPort, to: EventInputPort) -> Self {
        Self { from, to }
    }

    /// What feeds it.
    #[must_use]
    pub const fn source(self) -> EventOutputPort {
        self.from
    }

    /// Where it goes.
    #[must_use]
    pub const fn sink(self) -> EventInputPort {
        self.to
    }
}

/// One audio edge, named: what feeds which channel. What
/// [`Graph::connect`] adds and a modulation route onto its gain
/// (`ModTarget`) drives: the
/// **gain** of an edge is a weight of the sum its sink holds. The edge must
/// exist; a target whose edge is not in the graph yet
/// is unresolved until it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelEdge {
    from: OutputPort,
    to: InputPort,
}

impl ChannelEdge {
    /// The edge from `from` into `to`.
    #[must_use]
    pub const fn new(from: OutputPort, to: InputPort) -> Self {
        Self { from, to }
    }

    /// What feeds it.
    #[must_use]
    pub const fn source(self) -> OutputPort {
        self.from
    }

    /// Where it goes.
    #[must_use]
    pub const fn sink(self) -> InputPort {
        self.to
    }

    /// Whether the edge touches `node` (as its sink or its source).
    pub(crate) fn touches(self, node: NodeKey) -> bool {
        self.from.node() == Some(node) || self.to.node() == Some(node)
    }

    /// Makes the edge plain again: no gain, aliased when it is its sink's only
    /// source. Returns whether it had one. Structural, so it marks the graph
    /// edited when it did. A modulation route onto the edge makes the gain
    /// again (at unity) the next time the declarations apply: remove the route
    /// first.
    pub fn clear_gain<F: Float>(self, graph: &mut Graph<F>) -> bool {
        graph.clear_edge_gain_of(self)
    }
}

/// The kinds of wire [`Graph::connect`] makes. Sealed: a source and a sink
/// connect when they are the same kind.
pub trait Wire: Sealed {
    #[doc(hidden)]
    type Out: Copy;
    #[doc(hidden)]
    type In: Copy;
    #[doc(hidden)]
    fn connect<F: Float>(
        graph: &mut Graph<F>,
        from: Self::Out,
        to: Self::In,
    ) -> Result<Vec<(ChannelEdge, bool)>, ConnectError>;
    #[doc(hidden)]
    fn disconnect<F: Float>(
        graph: &mut Graph<F>,
        from: Self::Out,
        to: Self::In,
        hard: bool,
    ) -> Result<bool, ConnectError>;
    #[doc(hidden)]
    fn harden<F: Float>(
        graph: &mut Graph<F>,
        from: Self::Out,
        to: Self::In,
        edges: &[(ChannelEdge, bool)],
    );
}

/// One channel to one channel: an [`OutputPort`] into an [`InputPort`], and an edge
/// with a gain.
#[derive(Clone, Copy, Debug)]
pub struct ChannelWire;

/// A node's whole main output bus into another node's main input bus, or the
/// main output's channels, channel by channel: one edge per channel, each with a
/// gain.
#[derive(Clone, Copy, Debug)]
pub struct MainBusWire;

/// A bus edge: a fold between two declared bus topologies. Its lanes carry
/// the fold's weights and no gain of their own.
#[derive(Clone, Copy, Debug)]
pub struct BusWire;

/// An event edge: an [`EventOutputPort`] into an [`EventInputPort`]. Events merge by
/// offset at the sink, the graph ordering ties by source, and an event edge is
/// **never faded**: a note-off is a message, not a level (doc 030, section 4),
/// so [`Connection::unfaded`] and [`Disconnection::unfaded`] change nothing here.
#[derive(Clone, Copy, Debug)]
pub struct EventWire;

/// The wires whose edges carry a gain of their own.
pub trait Weighted: Wire {}
impl Weighted for ChannelWire {}
impl Weighted for MainBusWire {}

/// Something a signal leaves from: [`OutputPort`] (one channel), a [`Key`] or
/// [`NodeKey`] (the node's whole main bus) or a [`BusOutputPort`]. Sealed.
pub trait Outlet: Sealed {
    /// The kind of wire it starts.
    type Wire: Wire;
    #[doc(hidden)]
    fn out(self) -> <Self::Wire as Wire>::Out;
}

/// Something a signal arrives at, for a wire of kind `W`: [`InputPort`] (one
/// channel), a [`Key`] or [`NodeKey`] (the node's whole main bus), a
/// [`BusInputPort`], or the [`MainOutput`]. Sealed.
pub trait Inlet<W: Wire>: Sealed {
    #[doc(hidden)]
    fn sink(self) -> W::In;
}

/// A main-bus sink: a node, or the main output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainSink {
    #[doc(hidden)]
    Node(NodeKey),
    #[doc(hidden)]
    MainOutput,
}

/// A bus-edge sink: a node's input bus, or the main output. Made by
/// [`Key::bus_input`] and [`MainOutput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusInputPort(pub(crate) BusEnd);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BusEnd {
    Node(NodeBus),
    MainOutput,
}

impl BusInputPort {
    /// Input bus `bus` of `node`.
    #[must_use]
    pub fn new(node: impl Into<NodeKey>, bus: u8) -> Self {
        Self(BusEnd::Node(NodeBus::new(node.into(), bus)))
    }

    pub(crate) const fn node(bus: NodeBus) -> Self {
        Self(BusEnd::Node(bus))
    }
}

impl Sealed for OutputPort {}
impl Sealed for InputPort {}
impl Sealed for NodeKey {}
impl<N: ?Sized> Sealed for Key<N> {}
impl Sealed for BusOutputPort {}
impl Sealed for BusInputPort {}
impl Sealed for MainOutput {}
impl Sealed for ChannelWire {}
impl Sealed for MainBusWire {}
impl Sealed for BusWire {}
impl Sealed for EventWire {}
impl Sealed for EventOutputPort {}
impl Sealed for EventInputPort {}

impl Wire for ChannelWire {
    type Out = OutputPort;
    type In = InputPort;
    fn connect<F: Float>(
        graph: &mut Graph<F>,
        from: OutputPort,
        to: InputPort,
    ) -> Result<Vec<(ChannelEdge, bool)>, ConnectError> {
        let added = graph.add_edge(from, to)?;
        Ok(vec![(ChannelEdge::new(from, to), added)])
    }
    fn disconnect<F: Float>(
        graph: &mut Graph<F>,
        from: OutputPort,
        to: InputPort,
        hard: bool,
    ) -> Result<bool, ConnectError> {
        graph.check_wire(from, to)?;
        Ok(graph.remove_edge(from, to, hard))
    }
    fn harden<F: Float>(
        graph: &mut Graph<F>,
        _: OutputPort,
        _: InputPort,
        edges: &[(ChannelEdge, bool)],
    ) {
        for (edge, _) in edges {
            graph.harden_edge(*edge);
        }
    }
}

impl Wire for MainBusWire {
    type Out = NodeKey;
    type In = MainSink;
    fn connect<F: Float>(
        graph: &mut Graph<F>,
        from: NodeKey,
        to: MainSink,
    ) -> Result<Vec<(ChannelEdge, bool)>, ConnectError> {
        let wires = graph.main_wires(from, to)?;
        let mut edges = Vec::with_capacity(wires.len());
        for (out, input) in wires {
            let added = graph.add_edge(out, input)?;
            edges.push((ChannelEdge::new(out, input), added));
        }
        Ok(edges)
    }
    fn disconnect<F: Float>(
        graph: &mut Graph<F>,
        from: NodeKey,
        to: MainSink,
        hard: bool,
    ) -> Result<bool, ConnectError> {
        let wires = match graph.main_wires(from, to) {
            Ok(wires) => wires,
            // A main bus that is not wired (no bus there, another width) has
            // no edge to take out; a node that is not there is a mistake.
            Err(e @ ConnectError::NoSuchNode(_)) => return Err(e),
            Err(_) => return Ok(false),
        };
        let mut removed = false;
        for (out, input) in wires {
            removed |= graph.remove_edge(out, input, hard);
        }
        Ok(removed)
    }
    fn harden<F: Float>(
        graph: &mut Graph<F>,
        _: NodeKey,
        _: MainSink,
        edges: &[(ChannelEdge, bool)],
    ) {
        for (edge, _) in edges {
            graph.harden_edge(*edge);
        }
    }
}

impl Wire for BusWire {
    type Out = BusOutputPort;
    type In = BusInputPort;
    fn connect<F: Float>(
        graph: &mut Graph<F>,
        from: BusOutputPort,
        to: BusInputPort,
    ) -> Result<Vec<(ChannelEdge, bool)>, ConnectError> {
        graph.add_bus_edge(from, to)?;
        Ok(Vec::new())
    }
    fn disconnect<F: Float>(
        graph: &mut Graph<F>,
        from: BusOutputPort,
        to: BusInputPort,
        hard: bool,
    ) -> Result<bool, ConnectError> {
        graph.check_bus_nodes(from, to)?;
        Ok(graph.remove_bus_edge(from, to, hard))
    }
    fn harden<F: Float>(
        graph: &mut Graph<F>,
        from: BusOutputPort,
        to: BusInputPort,
        _: &[(ChannelEdge, bool)],
    ) {
        graph.harden_bus_edge(from, to);
    }
}

impl Wire for EventWire {
    type Out = EventOutputPort;
    type In = EventInputPort;
    fn connect<F: Float>(
        graph: &mut Graph<F>,
        from: EventOutputPort,
        to: EventInputPort,
    ) -> Result<Vec<(ChannelEdge, bool)>, ConnectError> {
        graph.add_event_edge(from, to)?;
        Ok(Vec::new())
    }
    fn disconnect<F: Float>(
        graph: &mut Graph<F>,
        from: EventOutputPort,
        to: EventInputPort,
        _hard: bool,
    ) -> Result<bool, ConnectError> {
        graph.check_event_wire(from, to)?;
        Ok(graph.remove_event_edge(from, to))
    }
    fn harden<F: Float>(
        _: &mut Graph<F>,
        _: EventOutputPort,
        _: EventInputPort,
        _: &[(ChannelEdge, bool)],
    ) {
    }
}

impl Outlet for EventOutputPort {
    type Wire = EventWire;
    fn out(self) -> EventOutputPort {
        self
    }
}

impl Inlet<EventWire> for EventInputPort {
    fn sink(self) -> EventInputPort {
        self
    }
}

/// A port with a set of sources: what [`Graph::sources`],
/// [`Graph::set_sources`] takes, so one
/// vocabulary covers an audio [`InputPort`] (its sources sum) and an
/// [`EventInputPort`] (its sources merge). Sealed.
pub trait Fanin: Sealed + Copy {
    /// What feeds it.
    type Source: Copy;
    #[doc(hidden)]
    fn sources<F: Float>(self, graph: &Graph<F>) -> Result<Vec<Self::Source>, ConnectError>;
    #[doc(hidden)]
    fn set_sources<F: Float>(
        self,
        graph: &mut Graph<F>,
        from: Vec<Self::Source>,
        hard: bool,
    ) -> Result<Vec<EdgeId>, ConnectError>;
}

impl Fanin for InputPort {
    type Source = OutputPort;
    fn sources<F: Float>(self, graph: &Graph<F>) -> Result<Vec<OutputPort>, ConnectError> {
        graph.check_in(self)?;
        Ok(graph.channel_sources(self))
    }
    fn set_sources<F: Float>(
        self,
        graph: &mut Graph<F>,
        from: Vec<OutputPort>,
        hard: bool,
    ) -> Result<Vec<EdgeId>, ConnectError> {
        graph.set_channel_sources(self, from, hard)
    }
}

impl Fanin for EventInputPort {
    type Source = EventOutputPort;
    fn sources<F: Float>(self, graph: &Graph<F>) -> Result<Vec<EventOutputPort>, ConnectError> {
        graph.check_event_in(self)?;
        Ok(graph.event_sources_of(self))
    }
    fn set_sources<F: Float>(
        self,
        graph: &mut Graph<F>,
        from: Vec<EventOutputPort>,
        _hard: bool,
    ) -> Result<Vec<EdgeId>, ConnectError> {
        graph.set_event_sources_of(self, &from)?;
        Ok(Vec::new())
    }
}

impl Outlet for OutputPort {
    type Wire = ChannelWire;
    fn out(self) -> OutputPort {
        self
    }
}
impl Outlet for NodeKey {
    type Wire = MainBusWire;
    fn out(self) -> NodeKey {
        self
    }
}
impl<N: ?Sized> Outlet for Key<N> {
    type Wire = MainBusWire;
    fn out(self) -> NodeKey {
        self.node
    }
}
impl Outlet for BusOutputPort {
    type Wire = BusWire;
    fn out(self) -> BusOutputPort {
        self
    }
}

impl Inlet<ChannelWire> for InputPort {
    fn sink(self) -> InputPort {
        self
    }
}
impl Inlet<MainBusWire> for NodeKey {
    fn sink(self) -> MainSink {
        MainSink::Node(self)
    }
}
impl<N: ?Sized> Inlet<MainBusWire> for Key<N> {
    fn sink(self) -> MainSink {
        MainSink::Node(self.node)
    }
}
impl Inlet<MainBusWire> for MainOutput {
    fn sink(self) -> MainSink {
        MainSink::MainOutput
    }
}
impl Inlet<BusWire> for BusInputPort {
    fn sink(self) -> BusInputPort {
        self
    }
}
impl Inlet<BusWire> for MainOutput {
    fn sink(self) -> BusInputPort {
        BusInputPort(BusEnd::MainOutput)
    }
}

/// A main output declaration just made ([`Session::declare_main_output_topology`](crate::Session::declare_main_output_topology)),
/// borrowing the graph so it can be made a cut:
/// `session.declare_main_output_topology(main_output)?.unfaded()`.
///
/// Dropping it keeps the declaration as made, which crossfades the old main output
/// into the new over [`DECLICK`](tutti_graph::DECLICK).
#[derive(Debug)]
pub struct MainOutputDeclaration<'g, F: Float = f32> {
    pub(crate) graph: &'g mut Graph<F>,
}

impl<F: Float> MainOutputDeclaration<'_, F> {
    /// Takes the new main output at one block, a cut: no old main output is rendered beside
    /// it. The default is a crossfade over [`DECLICK`](tutti_graph::DECLICK)
    /// (5 ms), so a change of layout never steps the sound; `unfaded` is for a
    /// test, an analysis graph or a main output changed before anything is listening.
    /// A declaration that waits for a running crossfade applies at once, and
    /// the cut is its.
    pub fn unfaded(self) -> Self {
        self.graph.harden_main_output();
        self
    }
}

/// What a [`Graph::disconnect`] did: whether the edge was connected, and the
/// step that makes the cut land at the next block boundary instead of fading
/// out.
///
/// Dropping it keeps the edge fading out, as the call left it.
#[derive(Debug)]
pub struct Disconnection<'g, W: Wire, F: Float = f32> {
    graph: &'g mut Graph<F>,
    was_connected: bool,
    ends: (W::Out, W::In),
}

impl<'g, W: Wire, F: Float> Disconnection<'g, W, F> {
    pub(crate) fn new(
        graph: &'g mut Graph<F>,
        was_connected: bool,
        from: W::Out,
        to: W::In,
    ) -> Self {
        Self {
            graph,
            was_connected,
            ends: (from, to),
        }
    }

    /// Cuts the edge at the next block boundary: no fade out. The default is
    /// a fade out over [`DECLICK`](tutti_graph::DECLICK) (5 ms), so a wire
    /// never steps out of a running sum; `unfaded` is for a test, an analysis
    /// graph or a wire nobody is listening to. An event edge is never faded,
    /// so it changes nothing there.
    pub fn unfaded(self) -> Self {
        // The call that made this value found both ends, and the edge's
        // fade is all that is left of it: cutting it again cannot refuse.
        let recut = W::disconnect(self.graph, self.ends.0, self.ends.1, true);
        debug_assert!(recut.is_ok(), "re-cutting a disconnected edge refused");
        self
    }

    /// Whether the edge was connected (and so was taken out).
    #[must_use]
    pub fn was_connected(&self) -> bool {
        self.was_connected
    }
}

/// What a [`Session::remove`](crate::Session::remove) did, and the step that makes the cut land at the
/// next block boundary instead of fading the node out.
///
/// Dropping it keeps the removal as the call left it: the node's output edges
/// fade out and the node goes once they have.
#[derive(Debug)]
pub struct Removal<'g, F: Float = f32> {
    graph: &'g mut Graph<F>,
    node: NodeKey,
}

impl<'g, F: Float> Removal<'g, F> {
    pub(crate) fn new(graph: &'g mut Graph<F>, node: NodeKey) -> Self {
        Self { graph, node }
    }

    /// Cuts the node at the next block boundary: no fade out. The default is
    /// a fade out over [`DECLICK`](tutti_graph::DECLICK) (5 ms), so a node
    /// never steps out of a running sum; `unfaded` is for a test, an analysis
    /// graph or a node nobody is listening to. Its controls are held, as for
    /// a faded removal, until the node has left the render.
    pub fn unfaded(self) -> Self {
        self.graph.cut_removal(self.node);
        self
    }
}

/// What a [`Graph::set_sources`] did, and the step that makes it land at the
/// next block boundary instead of fading.
///
/// Dropping it keeps the change as the call left it: what was added fading
/// in, what was taken out fading out.
#[derive(Debug)]
pub struct SetSources<'g, P: Fanin, F: Float = f32> {
    graph: &'g mut Graph<F>,
    to: P,
    from: Vec<P::Source>,
    cut: Vec<EdgeId>,
}

impl<'g, P: Fanin, F: Float> SetSources<'g, P, F> {
    pub(crate) fn new(
        graph: &'g mut Graph<F>,
        to: P,
        from: Vec<P::Source>,
        cut: Vec<EdgeId>,
    ) -> Self {
        Self {
            graph,
            to,
            from,
            cut,
        }
    }

    /// Steps the change at the next block boundary: what was added is at
    /// full weight at once, and what was taken out is cut. The default fades
    /// both over [`DECLICK`](tutti_graph::DECLICK) (5 ms); `unfaded` is for a
    /// test, an analysis graph or a sink nobody is listening to. An event
    /// sink is never faded, so it changes nothing there.
    pub fn unfaded(self) -> Self {
        self.graph.cut_edges(&self.cut);
        // The call that made this value found the sink and every source:
        // setting the same sources again cannot refuse.
        let reset = self.to.set_sources(self.graph, self.from.clone(), true);
        debug_assert!(reset.is_ok(), "setting the same sources again refused");
        self
    }
}

/// The edges a [`Graph::connect`] just made, borrowing the graph so a gain
/// can be set on them: `g.connect(a.output(0), b.input(0))?.gain(half)?`.
///
/// Dropping it keeps the edges as they are. Get the [`ChannelEdge`]s with
/// [`edges`](Self::edges) to keep a handle (a modulation target, a later
/// gain).
#[derive(Debug)]
pub struct Connection<'g, W: Wire, F: Float = f32> {
    graph: &'g mut Graph<F>,
    edges: Vec<(ChannelEdge, bool)>,
    ends: (W::Out, W::In),
}

impl<'g, W: Wire, F: Float> Connection<'g, W, F> {
    pub(crate) fn new(
        graph: &'g mut Graph<F>,
        edges: Vec<(ChannelEdge, bool)>,
        from: W::Out,
        to: W::In,
    ) -> Self {
        Self {
            graph,
            edges,
            ends: (from, to),
        }
    }

    /// Connects at full weight at once: no fade in. The default is a fade in
    /// over [`DECLICK`](tutti_graph::DECLICK) (5 ms), so a new
    /// wire never steps into a running sum; `unfaded` is for a test, an analysis
    /// graph or a wire made before anything is listening. It also stops a
    /// fade-out of an edge this call brought back.
    pub fn unfaded(self) -> Self {
        W::harden(self.graph, self.ends.0, self.ends.1, &self.edges);
        self
    }

    /// The edges the call connected: one for a [`ChannelWire`], one per channel
    /// for a [`MainBusWire`], none for a [`BusWire`] (its lanes are the fold's). An
    /// edge the graph already held is listed too.
    #[must_use]
    pub fn edges(&self) -> Vec<ChannelEdge> {
        self.edges.iter().map(|(edge, _)| *edge).collect()
    }
}

impl<W: Weighted, F: Float> Connection<'_, W, F> {
    /// Declares a gain lane on every edge of the connection, at `gain`: the
    /// edges enter the sum at `gain` rather than unity. Adding the lane is
    /// structural (the next [`update`](crate::Session::update) commits a
    /// weighted sum, as adding a source does), and it comes only from here:
    /// later changes to the weight are writes to the edge's
    /// [`GainTarget`](crate::GainTarget). An edge the graph already held
    /// keeps its lane at `gain`.
    ///
    /// # Errors
    ///
    /// [`DeclarationError::NotFinite`]; the edges this call added are taken
    /// out again, so nothing is added.
    pub fn gain(self, gain: Amplitude) -> Result<Self, DeclarationError> {
        if !gain.get().is_finite() {
            for (edge, added) in &self.edges {
                if *added {
                    self.graph.remove_edge(edge.from, edge.to, true);
                }
            }
            return Err(DeclarationError::NotFinite);
        }
        for (edge, _) in &self.edges {
            self.graph.declare_edge_gain(*edge, gain)?;
        }
        Ok(self)
    }
}
