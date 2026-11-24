//! Compiler pass 1: the units of a graph, each shape checked against its spec.

use crate::Edge;
use crate::NodeKey;
use crate::NodeMix;
use crate::compile::ids::{PlanUnitId, Table};
use crate::compile::{CompileError, CycleEdge, Shapes};
use crate::model::spec::{EventEdge, ValidGraph};
use crate::node::shape::Shape;
use crate::node::{MAX_PORTS, Prepare};

/// The units of a graph in key order, each with the shape its unit declared.
///
/// Every key and every port a graph names is a unit of this table with that
/// port: [`check`] refuses the graph otherwise.
pub(super) struct Units {
    /// The node of each unit, ascending (so a key's unit is a binary search).
    pub(super) keys: Table<PlanUnitId, NodeKey>,
    /// The shape of each unit, which agrees with its node's spec.
    pub(super) shapes: Table<PlanUnitId, Shape>,
}

impl Units {
    /// The unit of `key`, if the graph has that node.
    pub(super) fn get(&self, key: NodeKey) -> Option<PlanUnitId> {
        self.keys.search_by_key(&key, |k| *k)
    }

    /// The unit of `key`, a node the checked graph names.
    pub(super) fn unit(&self, key: NodeKey) -> PlanUnitId {
        self.get(key)
            .expect("a checked graph names only its own nodes")
    }

    /// The shape of the node `key`.
    pub(super) fn shape_of(&self, key: NodeKey) -> &Shape {
        &self.shapes[self.unit(key)]
    }
}

/// Checks every node's shape against its spec, and every event, param, gain
/// and resolution edge against the shapes' ports, and returns the units.
///
/// Takes the graph, the shapes the units declared and the `Prepare` the plan
/// is for; returns the [`Units`]. The result's widths, latency, tail and port
/// counts agree with the spec, each event, param and gain source names an
/// output its node has, each marked event edge's sink honours the resolution
/// it requires, and every feedback delay is at least one maximum block.
pub(super) fn check(
    graph: &ValidGraph,
    shapes: &Shapes,
    prepare: &Prepare,
) -> Result<Units, CompileError> {
    let topology = graph.topology();
    let mut units = Units {
        keys: topology.nodes.keys().copied().collect(),
        shapes: Table::default(),
    };
    for (&key, spec) in &topology.nodes {
        let shape = *shapes
            .get(&key)
            .ok_or(CompileError::MissingShape { node: key })?;
        if (spec.inputs, spec.outputs) != (shape.audio_in(), shape.audio_out()) {
            return Err(CompileError::WidthMismatch {
                node: key,
                declared: (spec.inputs, spec.outputs),
                shape: (shape.audio_in(), shape.audio_out()),
            });
        }
        if spec.latency != shape.latency.samples() {
            return Err(CompileError::LatencyMismatch {
                node: key,
                declared: spec.latency,
                shape: shape.latency,
            });
        }
        if spec.tail != shape.tail {
            return Err(CompileError::TailMismatch {
                node: key,
                declared: spec.tail,
                shape: shape.tail,
            });
        }
        for count in [
            shape.audio_in().count() as usize,
            shape.audio_out().count() as usize,
            shape.event_in as usize,
            shape.event_out as usize,
        ] {
            if count > MAX_PORTS {
                return Err(CompileError::TooManyPorts { node: key, count });
            }
        }
        units.shapes.push(shape);
    }
    for (&at, sources) in graph.events() {
        if at.port >= units.shape_of(at.node).event_in {
            return Err(CompileError::EventPortOutOfRange { at, from: None });
        }
        for e in sources {
            let from = e.from();
            if from.port >= units.shape_of(from.node).event_out {
                return Err(CompileError::EventPortOutOfRange {
                    at,
                    from: Some(from),
                });
            }
        }
    }

    // Params: each is declared by its node, and each source is an output of
    // its own.
    for (&at, &from) in graph.params() {
        if at.param != NodeMix::ADDR && units.shape_of(at.node).params.index_of(at.param).is_none()
        {
            return Err(CompileError::UnknownParam { at });
        }
        if from.port >= units.shape_of(from.node).event_out {
            return Err(CompileError::ParamSourceOutOfRange { at, from });
        }
    }
    // Edge gains: each driving source is an output of its node.
    for (&id, gain) in graph.gains() {
        if let Some(from) = gain.source
            && from.port >= units.shape_of(from.node).event_out
        {
            return Err(CompileError::GainSourceOutOfRange {
                sink: id.sink,
                from,
            });
        }
    }

    // Resolution: a marked edge's sink must honour what it requires (see
    // `GraphSpec::require_resolution` for the rule, and why only marked edges).
    for (&(at, from), &required) in graph.required_resolution() {
        let sink = units.shape_of(at.node).event_resolution;
        if !sink.satisfies(required) {
            return Err(CompileError::ResolutionTooCoarse {
                at,
                from,
                required,
                sink,
            });
        }
    }

    // Feedback delays: at least one block, or a read would need the future.
    let max_block = prepare.max_block().samples();
    for (&at, e) in topology
        .edges
        .iter()
        .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
    {
        if let Edge::Feedback(f) = *e
            && f.delay < max_block
        {
            return Err(CompileError::FeedbackTooShort {
                edge: CycleEdge::Audio(at),
                delay: f.delay,
                max_block,
            });
        }
    }
    for (&at, sources) in graph.events() {
        for e in sources {
            if let EventEdge::Feedback { from, delay } = *e
                && delay < max_block
            {
                return Err(CompileError::FeedbackTooShort {
                    edge: CycleEdge::Event { at, from },
                    delay,
                    max_block,
                });
            }
        }
    }
    Ok(units)
}
