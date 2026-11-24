//! Wiring: the typed edits of audio, event, param and gain edges, and of the
//! main output's width and declaration.

use tutti_buffers::{ChannelLayout, ChannelTopology, Float};

use super::{Editor, MissingSource, WireError};
use crate::model::spec::{EventEdge, EventInputPort, EventOutputPort};
use crate::node::param::ParamIn;
use crate::{Edge, EdgeId, InputPort, MainOutputId, PortSink, PortSource, Resolution};

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
        let was = self.spec.has_edge(id);
        match (id.sink, id.from) {
            (PortSink::Port(at), edge) => self.spec.connect(at, edge),
            (PortSink::Output(channel), Edge::Direct(source)) => {
                self.spec
                    .topology
                    .connect_output(usize::from(channel), source);
            }
            (PortSink::Output(_), Edge::Feedback(_)) => {}
        }
        let added = !was && self.spec.has_edge(id);
        if added {
            self.touch();
        }
        Ok(added)
    }

    /// Removes the audio edge `id` and its gain entry with it (a stale entry
    /// would fail every later commit). Returns whether the edge was there:
    /// one that is not (a node or channel that does not exist included) is
    /// not an error, as removing an edge twice is not.
    pub fn disconnect_edge(&mut self, id: EdgeId) -> bool {
        let removed = match (id.sink, id.from) {
            (PortSink::Port(at), edge) => self.spec.disconnect(at, edge),
            (PortSink::Output(channel), Edge::Direct(source))
                if usize::from(channel) < self.spec.topology.outputs.len() =>
            {
                self.spec.disconnect_output(usize::from(channel), source)
            }
            (PortSink::Output(_), _) => false,
        };
        if removed {
            self.touch();
        }
        removed
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
    pub fn connect_events(&mut self, at: EventInputPort, edge: EventEdge) -> Result<(), WireError> {
        self.check_event_in(at)?;
        self.check_event_out(edge.from())?;
        self.touch();
        self.spec.connect_events(at, edge);
        Ok(())
    }

    /// Removes exactly the event edge `edge` from `at` (a direct edge and a
    /// feedback edge from one source are two edges), and the source's
    /// resolution mark with the last of them. Returns whether it was there.
    pub fn disconnect_events(&mut self, at: EventInputPort, edge: EventEdge) -> bool {
        let Some(sources) = self.spec.events.get_mut(&at) else {
            return false;
        };
        let Some(i) = sources.iter().position(|e| *e == edge) else {
            return false;
        };
        sources.remove(i);
        let from = edge.from();
        let source_left = sources.iter().any(|e| e.from() == from);
        if sources.is_empty() {
            self.spec.events.remove(&at);
        }
        if !source_left {
            self.spec.clear_resolution(at, from);
        }
        self.touch();
        true
    }

    /// Drives param port `at` from event output `from`, in place of any
    /// source it had ([`GraphSpec::connect_param`](crate::GraphSpec::connect_param)
    /// states how a driven param is computed).
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`WireError::Missing`] for `at`'s node or
    /// `from`'s not existing ([`MissingSource::Node`]) or `from` past its
    /// node's event outputs ([`MissingSource::EventPort`]). A param the node
    /// does not declare is `compile`'s to refuse, as the shape is.
    pub fn connect_param(&mut self, at: ParamIn, from: EventOutputPort) -> Result<(), WireError> {
        self.check_node(at.node)?;
        self.check_event_out(from)?;
        if self.spec.connect_param(at, from) != Some(from) {
            self.touch();
        }
        Ok(())
    }

    /// Stops driving param port `at` from `from`: the port reads its base
    /// again. Returns whether `from` was its source.
    pub fn disconnect_param(&mut self, at: ParamIn, from: EventOutputPort) -> bool {
        let removed = self.spec.disconnect_param(at, from);
        if removed {
            self.touch();
        }
        removed
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
    pub fn connect_edge_gain(
        &mut self,
        id: EdgeId,
        from: EventOutputPort,
    ) -> Result<(), WireError> {
        self.check_gain_edge(id)?;
        self.check_event_out(from)?;
        let had = self.spec.gains.contains_key(&id);
        self.declare_lane(id);
        if self.spec.connect_edge_gain(id, from) != Some(from) || !had {
            self.touch();
        }
        Ok(())
    }

    /// Stops driving edge `id`'s gain from `from`; the edge keeps its entry,
    /// and reads its base. Returns whether `from` was its source.
    pub fn disconnect_edge_gain(&mut self, id: EdgeId, from: EventOutputPort) -> bool {
        let removed = self.spec.disconnect_edge_gain(id, from);
        if removed {
            self.touch();
        }
        removed
    }

    /// Removes edge `id`'s gain entry: the edge is plain again. Returns
    /// whether it had one.
    pub fn clear_edge_gain(&mut self, id: EdgeId) -> bool {
        let cleared = self.spec.clear_edge_gain(id).is_some();
        if cleared {
            self.touch();
        }
        cleared
    }

    /// Makes the graph's main output `width` channels wide. A new channel is
    /// silent; a channel cut takes its sources and their gain entries with
    /// it. Returns whether the width changed.
    pub fn resize_outputs(&mut self, width: usize) -> bool {
        let outputs = &mut self.spec.topology.outputs;
        if outputs.len() == width {
            return false;
        }
        outputs.resize(width, Vec::new());
        self.spec
            .gains
            .retain(|id, _| !matches!(id.sink, PortSink::Output(c) if usize::from(c) >= width));
        self.touch();
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
        let before = self.spec.topology.outputs.clone();
        self.spec.topology.set_outputs(sources);
        if self.spec.topology.outputs != before {
            let spec = &self.spec;
            let stale: Vec<EdgeId> = spec
                .gains
                .keys()
                .filter(|id| matches!(id.sink, PortSink::Output(_)) && !spec.has_edge(**id))
                .copied()
                .collect();
            for id in stale {
                self.spec.gains.remove(&id);
            }
            self.touch();
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
        self.spec.topology.inputs = width;
        let cut = |s: &PortSource| matches!(s, PortSource::MainInput(c) if *c >= width.count());
        for sources in self.spec.topology.edges.values_mut() {
            sources.retain(|e| !matches!(e, Edge::Direct(s) if cut(s)));
        }
        self.spec
            .topology
            .edges
            .retain(|_, sources| !sources.is_empty());
        for sources in &mut self.spec.topology.outputs {
            sources.retain(|s| !cut(s));
        }
        let spec = &self.spec;
        let stale: Vec<EdgeId> = spec
            .gains
            .keys()
            .filter(|id| !spec.has_edge(**id))
            .copied()
            .collect();
        for id in stale {
            self.spec.gains.remove(&id);
        }
        self.touch();
        true
    }

    /// Marks the event edge `from -> at` as requiring its sink to honour
    /// event offsets at least as finely as `resolution`
    /// ([`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution)
    /// states the rule). Replaces a mark it had.
    ///
    /// # Errors
    ///
    /// [`WireError::NoSuchEventEdge`] when no edge from `from` into `at`
    /// exists; nothing changes.
    pub fn require_resolution(
        &mut self,
        at: EventInputPort,
        from: EventOutputPort,
        resolution: Resolution,
    ) -> Result<(), WireError> {
        if !self
            .spec
            .events
            .get(&at)
            .is_some_and(|edges| edges.iter().any(|e| e.from() == from))
        {
            return Err(WireError::NoSuchEventEdge { at, from });
        }
        if self.spec.required_resolution.get(&(at, from)) != Some(&resolution) {
            self.spec.require_resolution(at, from, resolution);
            self.touch();
        }
        Ok(())
    }

    /// Drops the resolution mark on `from -> at`. Returns whether it had one.
    pub fn clear_resolution(&mut self, at: EventInputPort, from: EventOutputPort) -> bool {
        let had = self.spec.required_resolution.contains_key(&(at, from));
        if had {
            self.spec.clear_resolution(at, from);
            self.touch();
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
        self.spec.main_output = main_output;
        self.spec.main_output_id = id;
        self.touch();
        true
    }

    fn check_node(&self, node: crate::NodeKey) -> Result<(), MissingSource> {
        if self.spec.topology.nodes.contains_key(&node) {
            Ok(())
        } else {
            Err(MissingSource::Node { node })
        }
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

    fn check_event_in(&self, at: EventInputPort) -> Result<(), MissingSource> {
        self.check_node(at.node())?;
        let width = self.shapes.get(&at.node()).map_or(0, |s| s.event_in);
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
        self.check_node(from.node())?;
        let width = self.shapes.get(&from.node()).map_or(0, |s| s.event_out);
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
