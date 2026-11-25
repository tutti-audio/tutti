//! The graph value this crate compiles: a [`Topology`] plus event edges and
//! unit generations.
//!
//! # Why a wrapper and not an extension of `crate`
//!
//! Giving `InputPort`/`OutputPort` a kind would break every place
//! that builds them by literal (`InputPort::new(node, port)`). Event edges also
//! have a different *edge kind* from audio edges (an event edge carries an
//! offset-merged stream, an audio edge a signal that will be summed), so the
//! two maps do not share a type.
//!
//! [`GraphSpec`] therefore **embeds** the `Topology` unchanged (a set of audio
//! sources per sink port) and adds what is new beside it, with distinct port types ([`EventInputPort`],
//! [`EventOutputPort`]) so an audio port cannot be used as an event port by accident.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::bus_edge::{BusFeed, NodeBus};
use crate::model::gain::{EdgeFade, EdgeGain};
use crate::{ChannelFeed, LatencyGraph, PortFeed};
use crate::{Edge, EdgeId, InputPort, PortSink, PortSource};
use crate::{Invalid, Valid};
use crate::{NodeKey, Topology};
use tutti_buffers::ChannelTopology;
use tutti_units::Samples;

use crate::node::Resolution;
use crate::node::param::ParamIn;

/// An event **input** port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventInputPort {
    pub(crate) node: NodeKey,
    pub(crate) port: u16,
}

impl EventInputPort {
    /// The node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.node
    }

    /// Which of its event input ports.
    #[must_use]
    pub const fn port(self) -> u16 {
        self.port
    }

    /// Event input `port` of `node`.
    #[must_use]
    pub const fn new(node: NodeKey, port: u16) -> Self {
        Self { node, port }
    }
}

/// An event **output** port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventOutputPort {
    pub(crate) node: NodeKey,
    pub(crate) port: u16,
}

impl EventOutputPort {
    /// The node.
    #[must_use]
    pub const fn node(self) -> NodeKey {
        self.node
    }

    /// Which of its event output ports.
    #[must_use]
    pub const fn port(self) -> u16 {
        self.port
    }

    /// Event output `port` of `node`.
    #[must_use]
    pub const fn new(node: NodeKey, port: u16) -> Self {
        Self { node, port }
    }
}

/// One source of an event input port.
///
/// Port kinds match by type: an event edge runs from an [`EventOutputPort`] to an
/// [`EventInputPort`], and an audio port is a different type, so an audio↔event
/// edge is not a graph error for `compile` to catch — it does not
/// type-check:
///
/// ```compile_fail
/// use tutti_graph::{EventEdge, EventInputPort, GraphSpec};
/// use tutti_graph::OutputPort;
/// use tutti_graph::NodeKey;
/// let mut g = GraphSpec::default();
/// let audio = OutputPort::new(NodeKey(1), 0);
/// g.connect_events(EventInputPort::new(NodeKey(2), 0), EventEdge::Direct(audio));
/// ```
///
/// ```
/// use tutti_graph::{EventEdge, EventInputPort, EventOutputPort, GraphSpec};
/// use tutti_graph::NodeKey;
/// let mut g = GraphSpec::default();
/// let events = EventOutputPort::new(NodeKey(1), 0);
/// g.connect_events(EventInputPort::new(NodeKey(2), 0), EventEdge::Direct(events));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventEdge {
    /// This block's events from the named port.
    Direct(EventOutputPort),
    /// The named port's events delayed by exactly `delay` frames — a
    /// declared cycle, exactly as `crate::FeedbackFrom` is for
    /// audio, with the same rule: the delay belongs to the edge, and must be
    /// at least the interpreter's maximum block (see `FeedbackFrom`).
    Feedback {
        /// The source port.
        from: EventOutputPort,
        /// How far back, in frames.
        delay: Samples,
    },
}

impl EventEdge {
    /// The port the events leave from.
    #[must_use]
    pub const fn from(self) -> EventOutputPort {
        match self {
            Self::Direct(p) | Self::Feedback { from: p, .. } => p,
        }
    }

    /// A feedback edge from `from`, delayed by `delay` frames.
    pub const fn feedback(from: EventOutputPort, delay: Samples) -> Self {
        Self::Feedback { from, delay }
    }
}

/// The graph as a value: a [`Topology`] (nodes, audio edges, main inputs
/// and outputs) plus event edges, param modulation and unit generations.
///
/// Plain data — `Clone`, `Eq` and `Hash` — built and edited on the control
/// thread, by hand or through the editor's wiring calls
/// ([`Editor::connect_edge`](crate::Editor::connect_edge) and the calls beside it).
/// [`validate`](Self::validate) checks what can be checked without the
/// nodes' shapes and returns a [`ValidGraph`], which is what
/// [`compile`](fn@crate::compile) takes.
///
/// Audio edges live in the embedded `Topology`, a set of sources per input
/// port; the compiler sums a set of two or more in source
/// order (`Op::Sum`). Event ports
/// ([`EventInputPort`], [`EventOutputPort`]) are separate types, so an audio port cannot be
/// used as an event port by accident, and an event input may have several
/// sources, merged by offset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GraphSpec {
    /// Nodes, audio edges, main outputs and main input width — unchanged.
    pub topology: Topology,
    /// Event edges, keyed on the sink port: every source of the port (an
    /// event input may have several). An empty `Vec` is
    /// the same as an absent key.
    ///
    /// The `Vec`'s order does **not** matter: fan-in merges by offset, and
    /// events at equal offsets go in **source order**, the source's
    /// `(NodeKey, port)` ([`EventOutputPort`]'s `Ord`), whatever order they are
    /// listed in. [`connect_events`](Self::connect_events) keeps the list in
    /// that order, so two specs with the same wiring built in different
    /// orders compare equal.
    pub events: BTreeMap<EventInputPort, Vec<EventEdge>>,
    /// Unit generation per node. Bumping it is how a value says "same key, new
    /// unit" — a rebind or a replacement. Absent
    /// means generation 0.
    pub generations: BTreeMap<NodeKey, u32>,
    /// Event edges that require their sink to honour offsets at least this
    /// finely, keyed `(sink, source)` — see
    /// [`require_resolution`](Self::require_resolution).
    pub required_resolution: BTreeMap<(EventInputPort, EventOutputPort), Resolution>,
    /// Driven params, keyed on the param port (see
    /// [`connect_param`](Self::connect_param)): each one's event source. A
    /// port with no entry reads its base. A port's range is a value the editor
    /// holds ([`Editor::set_param_range`](crate::Editor::set_param_range)).
    pub params: BTreeMap<ParamIn, EventOutputPort>,
    /// Edge gains: an entry makes the edge a
    /// **weighted lane** of its sink's sum. An edge with no entry is plain.
    ///
    /// The entry is structural: adding or removing one recompiles, because a
    /// one-edge sink with an entry needs an `Op::Sum` of one addend where a
    /// plain one aliases. Changing the lane's base, range or fade does not
    /// ([`Editor::set_edge_gain`](crate::Editor::set_edge_gain)): they are
    /// values the editor holds. An entry
    /// whose edge does not exist is refused by [`validate`](Self::validate);
    /// [`disconnect`](Self::disconnect) removes the edge's entry with it.
    pub gains: BTreeMap<EdgeId, EdgeGain>,
    /// Bus edges:
    /// the sources of each sink bus, as a set kept in source order
    /// ([`connect_bus`](Self::connect_bus)). A bus edge is a fold between two
    /// declared layouts, and it needs the nodes' shapes to mean anything:
    /// [`lowered`](Self::lowered) replaces each with per-port edges and static
    /// weights, and that value, not this one, is what
    /// [`validate`](Self::validate) and [`compile`](fn@crate::compile) take.
    /// [`Editor::commit`](crate::Editor::commit) lowers for you. An empty
    /// `Vec` is the same as an absent key.
    pub bus_edges: BTreeMap<NodeBus, Vec<BusFeed>>,
    /// The declared topology of the main output, the graph's outputs, as a bus: what a
    /// bus edge into the main output folds to. `None` is undeclared. Every main output the
    /// engine renders is at most [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)
    /// wide, which is a bus's width. The session keeps it in step with its main output
    /// declaration.
    pub main_output: Option<ChannelTopology>,
    /// The main output's generation: a commit that changes the main output's declaration
    /// carries the next one ([`MainOutputId`](crate::MainOutputId)), and the plan it compiles to reports
    /// it ([`Plan::main_output_id`](crate::Plan::main_output_id)).
    pub main_output_id: crate::MainOutputId,
    /// Bus edges into the main output, in source order
    /// ([`connect_bus_to_main_output`](Self::connect_bus_to_main_output)): lowered, as
    /// `bus_edges` are, to lanes on the output channels.
    pub main_output_sources: Vec<BusFeed>,
}

impl GraphSpec {
    /// A spec over `topology` with no event edges and every node at
    /// generation 0.
    #[must_use]
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            ..Self::default()
        }
    }

    /// The generation of `node`: 0 unless one was set.
    pub fn generation(&self, node: NodeKey) -> u32 {
        self.generations.get(&node).copied().unwrap_or(0)
    }

    /// Adds `edge` to the audio sources of input port `at`
    /// ([`Topology::connect`]).
    pub fn connect(&mut self, at: InputPort, edge: Edge) {
        self.topology.connect(at, edge);
    }

    /// Removes `edge` from the audio sources of input port `at`, and its gain
    /// entry with it (a stale entry would fail every later
    /// [`validate`](Self::validate)). Returns whether the edge was there.
    pub fn disconnect(&mut self, at: InputPort, edge: Edge) -> bool {
        self.gains.remove(&EdgeId {
            sink: PortSink::Port(at),
            from: edge,
        });
        self.topology.disconnect(at, edge)
    }

    /// Removes `source` from the sources of main output `channel`, and its
    /// gain entry with it. Returns whether it was there.
    ///
    /// # Panics
    ///
    /// If `channel` is past [`Topology::outputs`].
    pub fn disconnect_output(&mut self, channel: usize, source: PortSource) -> bool {
        self.gains.remove(&EdgeId {
            sink: PortSink::Output(channel as u16),
            from: Edge::Direct(source),
        });
        self.topology.disconnect_output(channel, source)
    }

    /// Gives edge `id` the entry `gain`, replacing one it had: the edge
    /// becomes (or stays) a weighted lane. Adding an entry is structural; see
    /// [`gains`](Self::gains). The edge must exist
    /// ([`validate`](Self::validate) checks it).
    pub fn set_edge_gain(&mut self, id: EdgeId, gain: EdgeGain) -> Option<EdgeGain> {
        self.gains.insert(id, gain)
    }

    /// Removes edge `id`'s entry: the edge is plain again. Returns it.
    pub fn clear_edge_gain(&mut self, id: EdgeId) -> Option<EdgeGain> {
        self.gains.remove(&id)
    }

    /// Drives edge `id`'s gain from event output `from`, as
    /// [`connect_param`](Self::connect_param) drives a param port: the
    /// source **replaces** the base from its first event on (ramps and
    /// segments addressed to [`EdgeGain::ADDR`]), the value is clamped to
    /// the lane's range, a source that changes (or goes) crossfades over
    /// [`DECLICK`](crate::DECLICK), an undriven lane reads its
    /// base, and a lane's first block starts at its value. The rules are the
    /// param port's, stated once on [`connect_param`](Self::connect_param). The step is the sum's own, so
    /// the lane's values have no slot of their own.
    ///
    /// **PDC.** The source is aligned to the edge's sink: to the sink node's
    /// arrival (a later source raises it, as a param source does), or, for a
    /// main output channel, to the output time (a later source raises
    /// that). The delay is keyed [`EventDelayKey::EdgeGain`](crate::EventDelayKey::EdgeGain).
    ///
    /// An edge without an entry gets one (it reads the editor's base and
    /// range, unity and the default until set): the edge becomes a weighted
    /// lane, which is structural. Returns the source
    /// it replaced. The edge must exist ([`validate`](Self::validate) refuses
    /// [`GraphInvalid::GainWithoutEdge`]), `from`'s node must be in the graph
    /// ([`GraphInvalid::UnknownGainNode`]), and `from`'s port must be an
    /// event output of it (`compile`'s `CompileError::GainSourceOutOfRange`).
    pub fn connect_edge_gain(
        &mut self,
        id: EdgeId,
        from: EventOutputPort,
    ) -> Option<EventOutputPort> {
        self.gains.entry(id).or_default().source.replace(from)
    }

    /// Stops driving edge `id`'s gain from `from`. Returns whether `from`
    /// was its source. The edge keeps its entry (and so its place in the
    /// sum); with no source it reads its base.
    pub fn disconnect_edge_gain(&mut self, id: EdgeId, from: EventOutputPort) -> bool {
        let Some(g) = self.gains.get_mut(&id) else {
            return false;
        };
        if g.source == Some(from) {
            g.source = None;
            true
        } else {
            false
        }
    }

    /// This spec with every declick fade ended where it stands, or `None`
    /// when no edge fades out: an edge fading `Out` (by `fade`) is gone, and
    /// one fading `In` is at its weight, which the spec does not hold (a fork
    /// starts steady, as it does in every mode: it hears the state, not a
    /// toggle).
    #[must_use]
    pub(crate) fn settled(&self, fade: impl Fn(EdgeId) -> EdgeFade) -> Option<Self> {
        let leaving: Vec<EdgeId> = self
            .gains
            .keys()
            .copied()
            .filter(|&id| fade(id) == EdgeFade::Out)
            .collect();
        if leaving.is_empty() {
            return None;
        }
        let mut spec = self.clone();
        for id in leaving {
            match (id.sink, id.from) {
                (PortSink::Port(at), from) => {
                    spec.disconnect(at, from);
                }
                (PortSink::Output(c), Edge::Direct(s)) => {
                    spec.disconnect_output(usize::from(c), s);
                }
                (PortSink::Output(_), Edge::Feedback(_)) => {}
            }
        }
        Some(spec)
    }

    /// Whether edge `id` is in the topology: a direct or feedback edge of an
    /// input port, or a source of a main output channel.
    #[must_use]
    pub fn has_edge(&self, id: EdgeId) -> bool {
        match id.sink {
            PortSink::Port(at) => self
                .topology
                .edges
                .get(&at)
                .is_some_and(|e| e.contains(&id.from)),
            PortSink::Output(ch) => match id.from {
                Edge::Direct(s) => self
                    .topology
                    .outputs
                    .get(usize::from(ch))
                    .is_some_and(|v| v.contains(&s)),
                Edge::Feedback(_) => false,
            },
        }
    }

    /// Adds one event source to `at`, in source order (see
    /// [`events`](Self::events)). A source already listed at `at` is added
    /// again, and [`validate`](Self::validate) refuses the duplicate.
    pub fn connect_events(&mut self, at: EventInputPort, edge: EventEdge) {
        let sources = self.events.entry(at).or_default();
        let i = sources.partition_point(|e| e.from() <= edge.from());
        sources.insert(i, edge);
    }

    /// Marks the event edge `from → at` as requiring its sink to honour event
    /// offsets at least as finely as `resolution`.
    ///
    /// **The rule:** `compile` refuses a marked edge whose sink declares a
    /// coarser [`Shape::event_resolution`](crate::Shape::event_resolution)
    /// ([`CompileError::ResolutionTooCoarse`](crate::CompileError::ResolutionTooCoarse)).
    /// Unmarked edges are never refused — a note into a block-rate node is
    /// late by at most a block, which is a choice a patch may make; sample-
    /// accurate automation into one silently is not. So the edge carrying
    /// [`ParamRamp`](crate::ParamRamp) automation is the one to mark
    /// [`Resolution::Sample`]. A marked edge must exist ([`validate`](Self::validate)
    /// checks it): remove edges with [`disconnect_events`](Self::disconnect_events),
    /// which drops the mark too (editing `events` by hand does not), or drop
    /// a mark alone with [`clear_resolution`](Self::clear_resolution).
    pub fn require_resolution(
        &mut self,
        at: EventInputPort,
        from: EventOutputPort,
        resolution: Resolution,
    ) {
        self.required_resolution.insert((at, from), resolution);
    }

    /// Drops the resolution mark on `from → at`, if any.
    pub fn clear_resolution(&mut self, at: EventInputPort, from: EventOutputPort) {
        self.required_resolution.remove(&(at, from));
    }

    /// Removes the event edge `from → at` (direct or feedback), and its
    /// resolution mark with it — so a disconnect can never leave a stale mark
    /// that fails every later [`validate`](Self::validate). Returns whether an
    /// edge was removed.
    pub fn disconnect_events(&mut self, at: EventInputPort, from: EventOutputPort) -> bool {
        let Some(sources) = self.events.get_mut(&at) else {
            return false;
        };
        let before = sources.len();
        sources.retain(|e| e.from() != from);
        let removed = sources.len() != before;
        if sources.is_empty() {
            self.events.remove(&at);
        }
        self.required_resolution.remove(&(at, from));
        removed
    }

    /// Drives param port `at` from event output `from`, in place of any
    /// source it had (which is returned). A port has at most one event
    /// source: its value is the param's value, and summing two is a node's
    /// job (a control lane).
    ///
    /// The port must be one its node declares
    /// ([`Shape::params`](crate::Shape::params)), or `compile` refuses it
    /// ([`CompileError::UnknownParam`](crate::CompileError::UnknownParam)).
    /// Until a range is set ([`Editor::set_param_range`](crate::Editor::set_param_range))
    /// the value is not clamped.
    ///
    /// # How a driven param is computed
    ///
    /// Each driven param becomes one fused step of its node's op,
    /// computing, per frame:
    ///
    /// ```text
    ///   value[i] = clamp(held[i], range)
    ///   held[i]  = the event source's value, from its first event on
    ///            = base_ramp[i], before that
    /// ```
    ///
    /// - **The base** is the node's own control — a `Param<U>` it hands out in
    ///   its `Controls` — read once per block through
    ///   [`Node::param_base`](crate::Node::param_base) and ramped linearly
    ///   across the block, landing exactly on the new value at its last frame,
    ///   so a fader move does not zipper.
    /// - **The event source replaces the base**. Its
    ///   [`ParamRamp`](crate::ParamRamp)s and
    ///   [`ParamSegment`](crate::ParamSegment)s addressed to the param are
    ///   the param's own value, sample-accurate, from the source's first
    ///   event on (a first ramp starts from the base at its frame, also
    ///   when it replaces a segment sent first on that frame); before
    ///   it, a source with nothing to say is not a source: outside the
    ///   connect's declick the port reads
    ///   [`ParamInput::Base`](crate::ParamInput::Base), as unconnected,
    ///   and delivers frames from the block of the source's first event
    ///   on. Automation replaces the knob, as in
    ///   every DAW. Other events are ignored.
    /// - **An undriven param resolves to its base, never to 0.** A port with
    ///   no source this block reads
    ///   [`ParamInput::Base`](crate::ParamInput::Base), and the node uses its
    ///   own control, exactly as if nothing could drive it: the fast path
    ///   costs one branch, and nothing is copied. A port can be connected and
    ///   disconnected by any commit.
    /// - **Connecting or disconnecting is declicked.** When a port's source
    ///   changes (a new one, or none), its output crossfades from where it
    ///   was — the last value it delivered, or the base — to the new value
    ///   over [`DECLICK`](crate::DECLICK). Nothing else
    ///   is smoothed: a step in a source lands on its frame (the
    ///   sample-accuracy contract). A unit's **first** block is not a change:
    ///   a unit placed by a commit (an insert, a hard replace, a fork, a
    ///   re-prepare's resume) starts at its driven value, as its audio starts
    ///   at its first frame.
    /// - **A source's state is the source's, not its slot's.** The event
    ///   source's ramp (the value it holds, and a ramp under way) is kept
    ///   while the source stays, across commits that move its slot; a new
    ///   source starts at rest, reading the base until its first event (a
    ///   steady source re-wired to the port must send again: a control
    ///   lane's `LaneControls::resync`).
    /// - **A fresh unit hears nothing held.** A unit placed by a commit (a
    ///   fork's included) starts with no ramp, so a source must send its
    ///   current value on its first block for the port to read it (a control
    ///   lane does, having nothing in flight; a hand-wired source answers
    ///   [`EventKind::Resync`](crate::EventKind::Resync) or drives the knob
    ///   instead).
    /// - **A crossfade's base.** A [`replace`](crate::Editor::replace) with a
    ///   fade keeps the key's param state, so both units hear one source;
    ///   the base is the incoming unit's control, ramped over one block like
    ///   any control move, not over the fade. Ramping it over the fade would
    ///   hold the incoming unit off its own control for the fade's length, and
    ///   the audio crossfade already covers the swap.
    /// - **PDC.** A param source is aligned to the node's arrival like any of
    ///   its inputs: a source that arrives earlier is delayed
    ///   ([`EventDelayKey::ParamEvent`](crate::EventDelayKey::ParamEvent)), and a later
    ///   one raises the node's arrival.
    /// - **A node's mix is a port too.** [`ParamIn`] at
    ///   [`NodeMix::ADDR`](crate::NodeMix::ADDR) drives the node's
    ///   [`Mix`](tutti_units::Mix), which no node declares: the same rules
    ///   (one source, replaces the base, the declick, PDC), with the mix cell
    ///   (`Editor::set_mix`) as the base and the value clamped to `0..=1`
    ///   whatever range [`Editor::set_param_range`](crate::Editor::set_param_range)
    ///   is given.
    pub fn connect_param(&mut self, at: ParamIn, from: EventOutputPort) -> Option<EventOutputPort> {
        self.params.insert(at, from)
    }

    /// Stops driving param port `at` from `from`. Returns whether `from`
    /// was its source. The port keeps its range; with no source it reads its
    /// base.
    pub fn disconnect_param(&mut self, at: ParamIn, from: EventOutputPort) -> bool {
        if self.params.get(&at) == Some(&from) {
            self.params.remove(&at);
            true
        } else {
            false
        }
    }

    /// Whether `self` and `other` compile to the same plan: every field is
    /// structure, and equal.
    pub(crate) fn same_plan(&self, other: &Self) -> bool {
        // Every field named and none skipped, so a field added later fails to
        // compile here until it is decided to be structure.
        let Self {
            topology,
            events,
            generations,
            required_resolution,
            params,
            gains,
            bus_edges,
            main_output,
            main_output_id,
            main_output_sources,
        } = self;
        *topology == other.topology
            && *events == other.events
            && *bus_edges == other.bus_edges
            && *main_output == other.main_output
            && *main_output_id == other.main_output_id
            && *main_output_sources == other.main_output_sources
            && *generations == other.generations
            && *required_resolution == other.required_resolution
            && *params == other.params
            && *gains == other.gains
    }

    /// Checks everything that does not need the nodes' [`Shape`](crate::Shape)s.
    ///
    /// The `Topology` half runs `Topology::validate` unchanged; the event
    /// half checks that every event edge names nodes that exist and that no
    /// source is listed twice at one sink. Event port *ranges* need the
    /// shapes and are checked by `compile`, as is a cycle that runs through
    /// an event edge.
    ///
    /// # Errors
    ///
    /// Every fatal fault found, as a list of [`GraphInvalid`] (never just
    /// the first). An unconnected input is not a fault: it reads silence.
    pub fn validate(&self) -> Result<ValidGraph, Vec<GraphInvalid>> {
        let mut errs: Vec<GraphInvalid> = Vec::new();
        let valid = match self.topology.validate() {
            Ok(v) => Some(v),
            Err(e) => {
                errs.extend(
                    e.into_iter()
                        .filter(Invalid::is_fatal)
                        .map(GraphInvalid::Topology),
                );
                None
            }
        };

        let nodes = &self.topology.nodes;
        for (&at, sources) in &self.events {
            if !nodes.contains_key(&at.node) {
                errs.push(GraphInvalid::UnknownEventNode {
                    at,
                    missing: at.node,
                });
            }
            let mut seen = BTreeSet::new();
            for edge in sources {
                let from = edge.from();
                if !nodes.contains_key(&from.node) {
                    errs.push(GraphInvalid::UnknownEventNode {
                        at,
                        missing: from.node,
                    });
                }
                if !seen.insert(from) {
                    errs.push(GraphInvalid::DuplicateEventSource { at, from });
                }
            }
        }
        for &key in self.generations.keys() {
            if !nodes.contains_key(&key) {
                errs.push(GraphInvalid::UnknownGeneration { node: key });
            }
        }
        for (&at, from) in &self.params {
            for node in [at.node, from.node] {
                if !nodes.contains_key(&node) {
                    errs.push(GraphInvalid::UnknownParamNode { at, missing: node });
                }
            }
        }
        for (&id, gain) in &self.gains {
            if !self.has_edge(id) {
                errs.push(GraphInvalid::GainWithoutEdge { id });
            }
            if let Some(from) = gain.source
                && !nodes.contains_key(&from.node)
            {
                errs.push(GraphInvalid::UnknownGainNode {
                    id,
                    missing: from.node,
                });
            }
        }
        if self.has_bus_edges() {
            errs.push(GraphInvalid::BusEdgesUnlowered);
        }
        for &(at, from) in self.required_resolution.keys() {
            let wired = self
                .events
                .get(&at)
                .is_some_and(|v| v.iter().any(|e| e.from() == from));
            if !wired {
                errs.push(GraphInvalid::RequirementWithoutEdge { at, from });
            }
        }

        match valid {
            Some(valid) if errs.is_empty() => Ok(ValidGraph {
                valid,
                events: self.events.clone(),
                generations: self.generations.clone(),
                required_resolution: self.required_resolution.clone(),
                params: self.params.clone(),
                gains: self.gains.clone(),
                main_output_bank: None,
            }),
            _ => Err(errs),
        }
    }
}

/// Why a [`GraphSpec`] is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphInvalid {
    /// The embedded `Topology` is invalid. Only fatal faults are carried; an
    /// unconnected audio input reads silence and is not one.
    Topology(Invalid),
    /// An event edge names a node that is not in the topology.
    UnknownEventNode {
        /// The sink port whose edge names it.
        at: EventInputPort,
        /// The key that is not there.
        missing: NodeKey,
    },
    /// The same source is listed twice at one sink. Rejected rather than
    /// delivering every event twice: it is always an editing mistake, and the
    /// per-edge PDC state is keyed by `(sink, source)`.
    DuplicateEventSource {
        /// The sink port.
        at: EventInputPort,
        /// The repeated source.
        from: EventOutputPort,
    },
    /// A generation is recorded for a node that does not exist.
    UnknownGeneration {
        /// The key.
        node: NodeKey,
    },
    /// A resolution requirement names an event edge that is not in the
    /// graph — a stale mark, which would otherwise pass unchecked.
    RequirementWithoutEdge {
        /// The sink port.
        at: EventInputPort,
        /// The source port.
        from: EventOutputPort,
    },
    /// A param edge names a node that is not in the topology.
    UnknownParamNode {
        /// The param port whose entry names it.
        at: ParamIn,
        /// The key that is not there.
        missing: NodeKey,
    },
    /// A gain entry names an edge that is not in the graph: a stale entry,
    /// which would otherwise weight nothing, or weight a later edge that
    /// reuses the key. [`GraphSpec::disconnect`] removes the entry with the
    /// edge.
    GainWithoutEdge {
        /// The edge the entry names.
        id: EdgeId,
    },
    /// An edge gain is driven from an event output of a node that is not in
    /// the topology.
    UnknownGainNode {
        /// The edge whose entry names it.
        id: EdgeId,
        /// The key that is not there.
        missing: NodeKey,
    },
    /// The spec holds bus edges. A bus edge means a fold between the nodes'
    /// declared buses, which `validate` (it sees no shapes) cannot read:
    /// [`GraphSpec::lowered`] first, and validate its `spec`.
    BusEdgesUnlowered,
}

/// A [`GraphSpec`] that passed [`validate`](GraphSpec::validate).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidGraph {
    valid: Valid,
    events: BTreeMap<EventInputPort, Vec<EventEdge>>,
    generations: BTreeMap<NodeKey, u32>,
    required_resolution: BTreeMap<(EventInputPort, EventOutputPort), Resolution>,
    params: BTreeMap<ParamIn, EventOutputPort>,
    gains: BTreeMap<EdgeId, EdgeGain>,
    main_output_bank: Option<crate::MainOutputBank>,
}

impl ValidGraph {
    /// This graph with the outputs from `bank.base` on being the old main output's:
    /// the channels a plan renders beside the main output's own while a main output change
    /// crossfades ([`MainOutputBank`](crate::MainOutputBank)). The editor's commit makes
    /// them; the compiler gives them no meter, tap or compensation.
    #[must_use]
    pub(crate) fn with_main_output_bank(mut self, bank: crate::MainOutputBank) -> Self {
        debug_assert!(bank.base <= self.valid.get().outputs.len());
        self.main_output_bank = Some(bank);
        self
    }

    /// The old main output's outputs the topology's channels end with, if a main output
    /// change is crossfading.
    pub fn main_output_bank(&self) -> Option<crate::MainOutputBank> {
        self.main_output_bank
    }

    /// The checked topology.
    pub fn topology(&self) -> &Topology {
        self.valid.get()
    }

    /// The checked event edges.
    pub fn events(&self) -> &BTreeMap<EventInputPort, Vec<EventEdge>> {
        &self.events
    }

    /// The generation of `node`: 0 unless one was set.
    pub fn generation(&self, node: NodeKey) -> u32 {
        self.generations.get(&node).copied().unwrap_or(0)
    }

    /// The checked resolution requirements, keyed `(sink, source)`.
    pub fn required_resolution(&self) -> &BTreeMap<(EventInputPort, EventOutputPort), Resolution> {
        &self.required_resolution
    }

    /// The checked driven params: each port's source.
    pub fn params(&self) -> &BTreeMap<ParamIn, EventOutputPort> {
        &self.params
    }

    /// The checked edge gains: each names an edge of the topology.
    pub fn gains(&self) -> &BTreeMap<EdgeId, EdgeGain> {
        &self.gains
    }
}

/// The latency folds over the whole spec: the topology's audio ports, plus
/// each node's event and param sources, which the compiler counts
/// toward a node's arrival. So `crate::latency_plan(&spec)` is the
/// compensation `compile` gives the spec's plan (`tests/topology/compile_passes.rs`
/// pins it); over `spec.topology` alone, a node fed events by a latent node
/// would arrive early.
impl LatencyGraph for GraphSpec {
    type Node = NodeKey;

    fn nodes(&self) -> impl Iterator<Item = NodeKey> {
        LatencyGraph::nodes(&self.topology)
    }

    fn latency(&self, node: NodeKey) -> Samples {
        LatencyGraph::latency(&self.topology, node)
    }

    fn inputs(&self, node: NodeKey) -> impl Iterator<Item = PortFeed<NodeKey>> {
        LatencyGraph::inputs(&self.topology, node)
    }

    /// Direct event sources (a feedback edge carries last block's events, so
    /// nothing along this block's path) and param sources.
    fn other_sources(&self, node: NodeKey) -> impl Iterator<Item = NodeKey> {
        driving_nodes(&self.events, &self.params, &self.gains, node)
    }

    fn output_sources(&self) -> impl Iterator<Item = NodeKey> {
        output_gain_sources(&self.gains)
    }

    fn outputs(&self) -> impl Iterator<Item = ChannelFeed<NodeKey>> {
        LatencyGraph::outputs(&self.topology)
    }
}

/// The same fold over a checked graph: `compile` solves latency through it.
impl LatencyGraph for ValidGraph {
    type Node = NodeKey;

    fn nodes(&self) -> impl Iterator<Item = NodeKey> {
        LatencyGraph::nodes(self.topology())
    }

    fn latency(&self, node: NodeKey) -> Samples {
        LatencyGraph::latency(self.topology(), node)
    }

    fn inputs(&self, node: NodeKey) -> impl Iterator<Item = PortFeed<NodeKey>> {
        LatencyGraph::inputs(self.topology(), node)
    }

    fn other_sources(&self, node: NodeKey) -> impl Iterator<Item = NodeKey> {
        driving_nodes(&self.events, &self.params, &self.gains, node)
    }

    fn output_sources(&self) -> impl Iterator<Item = NodeKey> {
        output_gain_sources(&self.gains)
    }

    /// The main output's channels: an old main output's channels beside them take their
    /// delays from the plan they were heard in, not from this solve.
    fn outputs(&self) -> impl Iterator<Item = ChannelFeed<NodeKey>> {
        let main_output = self.main_output_bank.map_or(usize::MAX, |bank| bank.base);
        LatencyGraph::outputs(self.topology()).filter(move |feed| feed.channel < main_output)
    }
}

/// The nodes that drive `node` through an event input or a param: its
/// non-audio sources, as [`LatencyGraph::other_sources`].
fn driving_nodes<'a>(
    events: &'a BTreeMap<EventInputPort, Vec<EventEdge>>,
    params: &'a BTreeMap<ParamIn, EventOutputPort>,
    gains: &'a BTreeMap<EdgeId, EdgeGain>,
    node: NodeKey,
) -> impl Iterator<Item = NodeKey> + 'a {
    let ports = EventInputPort { node, port: 0 }..=EventInputPort {
        node,
        port: u16::MAX,
    };
    let event_sources = events
        .range(ports)
        .flat_map(|(_, sources)| sources)
        .filter_map(|e| match *e {
            EventEdge::Direct(from) => Some(from.node),
            EventEdge::Feedback { .. } => None,
        });
    let param_sources = params
        .iter()
        .filter(move |(at, _)| at.node == node)
        .map(|(_, from)| from.node);
    // The lane that drives the gain of an edge into one of the node's ports
    // is aligned to the node's arrival, as a param's source is.
    let gain_sources = gains
        .iter()
        .filter(move |(id, _)| matches!(id.sink, PortSink::Port(at) if at.node == node))
        .filter_map(|(_, g)| g.source.map(|s| s.node));
    event_sources.chain(param_sources).chain(gain_sources)
}

/// The nodes that drive the gain of an edge into a main output channel:
/// [`LatencyGraph::output_sources`].
fn output_gain_sources(gains: &BTreeMap<EdgeId, EdgeGain>) -> impl Iterator<Item = NodeKey> + '_ {
    gains
        .iter()
        .filter(|(id, _)| matches!(id.sink, PortSink::Output(_)))
        .filter_map(|(_, g)| g.source.map(|s| s.node))
}
