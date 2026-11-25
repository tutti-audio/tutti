//! Wiring: the typed edits of audio, event, param and gain edges, and of the
//! main output's width and declaration.

use tutti_buffers::{ChannelLayout, ChannelTopology, Float};

use super::{Editor, MissingSource, WireError};
use crate::Shape;
use crate::model::mode::NodeMix;
use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort};
use crate::node::param::ParamIn;
use crate::{Edge, EdgeId, InputPort, MainOutputId, PortSink, PortSource, Resolution, Topology};

impl<F: Float> Editor<F> {
    /// Adds the audio edge `id` — a source into an input port, or into a main
    /// output channel (a sink holds a set, which the graph sums). Returns
    /// whether the edge is new: wiring an edge already there, or
    /// `PortSource::Zero` (silence is the empty set), changes nothing. The
    /// edge's gain entry, if it has one, is the edge's own and stays.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] for a node the edge
    /// names that does not exist, a source port past its node's outputs or a
    /// main input past the graph's, [`WireError::NoInputPort`] for an input
    /// port past its node's inputs, [`WireError::NoOutputChannel`] for a
    /// channel past the graph's outputs, and [`WireError::FeedbackIntoOutput`]
    /// for a feedback edge into a main output channel.
    pub fn connect_edge(&mut self, id: EdgeId) -> Result<bool, WireError> {
        match (id.sink, id.from) {
            (PortSink::Port(at), edge) => {
                self.check_input(at)?;
                match edge {
                    Edge::Direct(source) => self.check_sources(&[source])?,
                    Edge::Feedback(f) => self.check_sources(&[PortSource::Node(f.from)])?,
                }
            }
            (PortSink::Output(channel), Edge::Direct(source)) => {
                self.check_channel(channel)?;
                self.check_sources(&[source])?;
            }
            (PortSink::Output(channel), Edge::Feedback(_)) => {
                return Err(WireError::FeedbackIntoOutput { channel });
            }
        }
        if self.spec.has_edge(id) || id.from == Edge::Direct(PortSource::Zero) {
            return Ok(false);
        }
        let spec = self.spec_mut();
        match (id.sink, id.from) {
            (PortSink::Port(at), edge) => spec.connect(at, edge),
            (PortSink::Output(channel), Edge::Direct(source)) => {
                spec.topology.connect_output(usize::from(channel), source);
            }
            (PortSink::Output(_), Edge::Feedback(_)) => {}
        }
        Ok(true)
    }

    /// Removes the audio edge `id` and its gain entry with it (a stale entry
    /// would fail every later commit). Returns whether the edge was there:
    /// one that is not (a node or channel that does not exist included) is
    /// not an error, as removing an edge twice is not.
    pub fn disconnect_edge(&mut self, id: EdgeId) -> bool {
        if !self.spec.has_edge(id) {
            return false;
        }
        let spec = self.spec_mut();
        match (id.sink, id.from) {
            (PortSink::Port(at), edge) => spec.disconnect(at, edge),
            (PortSink::Output(channel), Edge::Direct(source)) => {
                spec.disconnect_output(usize::from(channel), source)
            }
            (PortSink::Output(_), Edge::Feedback(_)) => false,
        }
    }

    /// Adds the event edge `edge` into event input `at`; a port holds the set
    /// of its sources, merged by offset ([`GraphSpec::events`](crate::GraphSpec::events)).
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] with
    /// [`MissingSource::Node`] for a node that does not exist,
    /// [`MissingSource::EventInputPort`] for `at` past its node's event
    /// inputs, and [`MissingSource::EventPort`] for the source past its
    /// node's event outputs. A source the port already lists is added again
    /// and refused by the commit
    /// ([`GraphInvalid::DuplicateEventSource`](crate::GraphInvalid::DuplicateEventSource)),
    /// not here: an edit may pass through it, wiring a node's new edge before
    /// the old one from the same source is removed.
    pub fn connect_events(
        &mut self,
        at: EventInputPort,
        edge: EventEdge,
    ) -> Result<bool, WireError> {
        self.check_event_in(at)?;
        self.check_event_out(edge.from())?;
        self.spec_mut().connect_events(at, edge);
        Ok(true)
    }

    /// Removes exactly the event edge `edge` from `at` (a direct edge and a
    /// feedback edge from one source are two edges), and the source's
    /// resolution mark with the last of them. Returns whether it was there.
    pub fn disconnect_events(&mut self, at: EventInputPort, edge: EventEdge) -> bool {
        if !self
            .spec
            .events
            .get(&at)
            .is_some_and(|sources| sources.contains(&edge))
        {
            return false;
        }
        let spec = self.spec_mut();
        let sources = spec.events.entry(at).or_default();
        if let Some(i) = sources.iter().position(|e| *e == edge) {
            sources.remove(i);
        }
        let from = edge.from();
        let source_left = sources.iter().any(|e| e.from() == from);
        if sources.is_empty() {
            spec.events.remove(&at);
        }
        if !source_left {
            spec.clear_resolution(at, from);
        }
        true
    }

    /// Drives param port `at` from event output `from`, in place of any
    /// source it had ([`GraphSpec::connect_param`](crate::GraphSpec::connect_param)
    /// states how a driven param is computed).
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] for `at`'s node or
    /// `from`'s not existing ([`MissingSource::Node`]), a param the node does
    /// not declare ([`MissingSource::Param`]; a node's mix is declared by
    /// every node) or `from` past its node's event outputs
    /// ([`MissingSource::EventPort`]).
    ///
    /// Returns whether the port's source changed: wiring the source it
    /// already has changes nothing.
    pub fn connect_param(&mut self, at: ParamIn, from: EventOutputPort) -> Result<bool, WireError> {
        let shape = self.shape_of(at.node)?;
        // The declaration `compile` checks (`UnknownParam`).
        if at.param != NodeMix::ADDR && shape.params.index_of(at.param).is_none() {
            return Err(MissingSource::Param {
                node: at.node,
                param: at.param,
            }
            .into());
        }
        self.check_event_out(from)?;
        if self.spec.params.get(&at) == Some(&from) {
            return Ok(false);
        }
        self.spec_mut().connect_param(at, from);
        Ok(true)
    }

    /// Stops driving param port `at` from `from`: the port reads its base
    /// again. Returns whether `from` was its source.
    pub fn disconnect_param(&mut self, at: ParamIn, from: EventOutputPort) -> bool {
        self.spec.params.get(&at) == Some(&from) && self.spec_mut().disconnect_param(at, from)
    }

    /// Drives edge `id`'s gain from event output `from`
    /// ([`GraphSpec::connect_edge_gain`](crate::GraphSpec::connect_edge_gain)
    /// states how). An edge with no gain entry gets one, with its cell, as
    /// [`set_edge_gain`](Self::set_edge_gain) gives it.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Gain`] when `id` is not an
    /// edge of the spec or is a bus lane
    /// ([`check_gain_edge`](Self::check_gain_edge)), and
    /// [`WireError::Missing`] for `from` naming a node that does not exist or
    /// a port past its event outputs.
    ///
    /// Returns whether the lane's entry or source changed.
    pub fn connect_edge_gain(
        &mut self,
        id: EdgeId,
        from: EventOutputPort,
    ) -> Result<bool, WireError> {
        self.check_gain_edge(id)?;
        self.check_event_out(from)?;
        if self
            .spec
            .gains
            .get(&id)
            .is_some_and(|g| g.source == Some(from))
        {
            return Ok(false);
        }
        self.declare_lane(id);
        self.spec_mut().connect_edge_gain(id, from);
        Ok(true)
    }

    /// Stops driving edge `id`'s gain from `from`; the edge keeps its entry,
    /// and reads its base. Returns whether `from` was its source.
    pub fn disconnect_edge_gain(&mut self, id: EdgeId, from: EventOutputPort) -> bool {
        self.spec
            .gains
            .get(&id)
            .is_some_and(|g| g.source == Some(from))
            && self.spec_mut().disconnect_edge_gain(id, from)
    }

    /// Removes edge `id`'s gain entry: the edge is plain again. Returns
    /// whether it had one.
    pub fn clear_edge_gain(&mut self, id: EdgeId) -> bool {
        self.spec.gains.contains_key(&id) && self.spec_mut().clear_edge_gain(id).is_some()
    }

    /// Makes the graph's main output `width` channels wide. A new channel is
    /// silent; a channel cut takes its sources and their gain entries with
    /// it. Returns whether the width changed.
    pub fn resize_outputs(&mut self, width: ChannelLayout) -> bool {
        let width = usize::from(width.count());
        if self.spec.topology.outputs.len() == width {
            return false;
        }
        let spec = self.spec_mut();
        spec.topology.outputs.resize(width, Vec::new());
        spec.gains
            .retain(|id, _| !matches!(id.sink, PortSink::Output(c) if usize::from(c) >= width));
        true
    }

    /// Wires the main output: channel `c` is fed by exactly the `c`th of
    /// `sources`, and the output is as wide as there are sources
    /// (`PortSource::Zero` is a silent channel). Every channel's earlier
    /// sources go, and their gain entries with them. The checked form of
    /// [`Topology::set_outputs`](crate::Topology::set_outputs).
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] for a source that
    /// names a node that does not exist, a port past its node's outputs or a
    /// main input past the graph's.
    pub fn wire_outputs(
        &mut self,
        sources: impl IntoIterator<Item = PortSource>,
    ) -> Result<(), WireError> {
        let sources: Vec<PortSource> = sources.into_iter().collect();
        self.check_sources(&sources)?;
        let mut wired = Topology::default();
        wired.set_outputs(sources);
        if wired.outputs != self.spec.topology.outputs {
            let spec = self.spec_mut();
            spec.topology.outputs = wired.outputs;
            let mut gains = std::mem::take(&mut spec.gains);
            gains.retain(|id, _| !matches!(id.sink, PortSink::Output(_)) || spec.has_edge(*id));
            spec.gains = gains;
        }
        Ok(())
    }

    /// Makes the graph's main input `width` channels wide. An edge from a
    /// channel cut goes, with its gain entry. Returns whether the width
    /// changed.
    pub fn resize_inputs(&mut self, width: ChannelLayout) -> bool {
        if self.spec.topology.inputs == width {
            return false;
        }
        let spec = self.spec_mut();
        spec.topology.inputs = width;
        let cut = |s: &PortSource| matches!(s, PortSource::MainInput(c) if *c >= width.count());
        for sources in spec.topology.edges.values_mut() {
            sources.retain(|e| !matches!(e, Edge::Direct(s) if cut(s)));
        }
        spec.topology.edges.retain(|_, sources| !sources.is_empty());
        for sources in &mut spec.topology.outputs {
            sources.retain(|s| !cut(s));
        }
        let mut gains = std::mem::take(&mut spec.gains);
        gains.retain(|id, _| spec.has_edge(*id));
        spec.gains = gains;
        true
    }

    /// Marks the event edge `from -> at` as requiring its sink to honour
    /// event offsets at least as finely as `resolution`
    /// ([`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution)
    /// states the rule). Replaces a mark it had.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] for a node or port
    /// that does not exist (as [`connect_events`](Self::connect_events)
    /// names them), and [`WireError::NoSuchEventEdge`] when both ports exist
    /// but no edge from `from` into `at` does.
    pub fn require_resolution(
        &mut self,
        at: EventInputPort,
        from: EventOutputPort,
        resolution: Resolution,
    ) -> Result<(), WireError> {
        self.check_event_in(at)?;
        self.check_event_out(from)?;
        if !self
            .spec
            .events
            .get(&at)
            .is_some_and(|edges| edges.iter().any(|e| e.from() == from))
        {
            return Err(WireError::NoSuchEventEdge { at, from });
        }
        if self.spec.required_resolution.get(&(at, from)) != Some(&resolution) {
            self.spec_mut().require_resolution(at, from, resolution);
        }
        Ok(())
    }

    /// Drops the resolution mark on `from -> at`. Returns whether it had one.
    pub fn clear_resolution(&mut self, at: EventInputPort, from: EventOutputPort) -> bool {
        let had = self.spec.required_resolution.contains_key(&(at, from));
        if had {
            self.spec_mut().clear_resolution(at, from);
        }
        had
    }

    /// Declares the main output: its topology (`None` undeclared) and the
    /// generation a commit carries to the host
    /// ([`GraphSpec::main_output`](crate::GraphSpec::main_output),
    /// [`GraphSpec::main_output_id`](crate::GraphSpec::main_output_id)). A
    /// bus edge into the main output folds to the declared topology at the
    /// next commit. Returns whether either changed.
    pub fn declare_main_output(
        &mut self,
        main_output: Option<ChannelTopology>,
        id: MainOutputId,
    ) -> bool {
        if self.spec.main_output == main_output && self.spec.main_output_id == id {
            return false;
        }
        let spec = self.spec_mut();
        spec.main_output = main_output;
        spec.main_output_id = id;
        true
    }

    fn check_input(&self, at: InputPort) -> Result<(), WireError> {
        let node = self
            .spec
            .topology
            .nodes
            .get(&at.node)
            .ok_or(MissingSource::Node { node: at.node })?;
        if at.port >= node.inputs.count() {
            return Err(WireError::NoInputPort {
                node: at.node,
                port: at.port,
                width: usize::from(node.inputs.count()),
            });
        }
        Ok(())
    }

    fn check_channel(&self, channel: u16) -> Result<(), WireError> {
        let width = self.spec.topology.outputs.len();
        if usize::from(channel) >= width {
            return Err(WireError::NoOutputChannel { channel, width });
        }
        Ok(())
    }

    /// The shape of `node`, which every node of the spec has: `insert` writes
    /// the node's spec and its shape together, `remove` takes both out, and a
    /// fork or a builder places its nodes through `insert`. A node the editor
    /// knows no shape for is refused as missing.
    fn shape_of(&self, node: crate::NodeKey) -> Result<&Shape, MissingSource> {
        self.shapes.get(&node).ok_or(MissingSource::Node { node })
    }

    fn check_event_in(&self, at: EventInputPort) -> Result<(), MissingSource> {
        let width = self.shape_of(at.node())?.event_in;
        if at.port() >= width {
            return Err(MissingSource::EventInputPort {
                node: at.node(),
                port: at.port(),
                width: usize::from(width),
            });
        }
        Ok(())
    }

    fn check_event_out(&self, from: EventOutputPort) -> Result<(), MissingSource> {
        let width = self.shape_of(from.node())?.event_out;
        if from.port() >= width {
            return Err(MissingSource::EventPort {
                node: from.node(),
                port: from.port(),
                width: usize::from(width),
            });
        }
        Ok(())
    }
}
