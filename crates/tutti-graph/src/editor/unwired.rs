//! What placing a unit at a key leaves unwired: the wiring recorded for the
//! key that names a port the new unit's shape does not have.

use super::Editor;
use crate::bus_edge::{BusFeed, BusOutputPort, NodeBus};
use crate::gain::EdgeFade;
use crate::mode::NodeMix;
use crate::node::Shape;
use crate::param::ParamIn;
use crate::spec::{EventEdge, EventInputPort, EventOutputPort};
use crate::{Edge, EdgeId, FeedbackFrom, NodeKey, PortSink, PortSource};
use tutti_buffers::Float;

/// A unit placed at a key, and what the placement left unwired.
///
/// Returned by [`Editor::insert`] and [`Editor::replace_or_swap`]: the
/// node's typed controls, and the wiring the new unit's shape cannot carry.
#[derive(Debug)]
pub struct Inserted<C> {
    /// The node's typed controls.
    pub controls: C,
    /// The wiring the placement removed from the spec.
    pub unwired: Unwired,
}

/// The wiring a placement removed because the new shape lacks a port it
/// names.
///
/// A port keeps its identity by its index within its kind (audio input,
/// audio output, event input, event output) and, for a bus, by its bus and
/// band; wiring to a port the shape still has stays exactly as it was, gains
/// and declick state included. Everything here left the spec at the
/// placement. An edge whose declick fade was already ending it (the caller
/// disconnected it) is not listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "the edges a placement removed are the caller's to report"]
pub struct Unwired {
    /// Audio edges, direct and feedback, into an input port or out of an
    /// output port the shape lacks. A main output channel an edge fed is
    /// its [`PortSink::Output`].
    pub edges: Vec<EdgeId>,
    /// Event edges, direct and feedback, into an event input or out of an
    /// event output the shape lacks, by the sink port they fed.
    pub event_edges: Vec<(EventInputPort, EventEdge)>,
    /// Bus edges into a bus the shape lacks or out of one, by the sink bus.
    pub bus_edges: Vec<(NodeBus, BusFeed)>,
    /// Bus edges into the main output out of an output bus the shape lacks.
    pub main_output_buses: Vec<BusFeed>,
    /// Param ports a source drove that no longer exists or whose source
    /// port is gone: the port, and the event output that drove it. The
    /// source is cleared; a param the shape no longer declares loses its
    /// entry.
    pub params: Vec<(ParamIn, EventOutputPort)>,
    /// Edge gains driven from an event output the shape lacks: the edge, and
    /// the output that drove it. The edge keeps its gain entry and reads its
    /// base.
    pub gains: Vec<(EdgeId, EventOutputPort)>,
}

impl Unwired {
    /// Whether the placement removed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl<F: Float> Editor<F> {
    /// Removes every record of `key`'s wiring that names a port `shape`
    /// lacks, and returns what went.
    pub(super) fn unwire(&mut self, key: NodeKey, shape: &Shape) -> Unwired {
        let mut out = Unwired::default();
        let (ain, aout) = (shape.audio_in().count(), shape.audio_out().count());
        let (ein, eout) = (shape.event_in, shape.event_out);
        let spec = &mut self.spec;
        let faded_out = |spec: &crate::GraphSpec, id: EdgeId| {
            spec.gains.get(&id).is_some_and(|g| g.fade == EdgeFade::Out)
        };
        let gone_source =
            |s: &PortSource| matches!(s, PortSource::Node(p) if p.node == key && p.port >= aout);

        // Audio edges into a node's input ports.
        let mut dropped: Vec<EdgeId> = Vec::new();
        spec.topology.edges.retain(|&at, list| {
            list.retain(|e| {
                let from_gone = match e {
                    Edge::Direct(s) => gone_source(s),
                    Edge::Feedback(FeedbackFrom { from, .. }) => {
                        from.node == key && from.port >= aout
                    }
                };
                let gone = (at.node == key && at.port >= ain) || from_gone;
                if gone {
                    dropped.push(EdgeId {
                        sink: PortSink::Port(at),
                        from: *e,
                    });
                }
                !gone
            });
            !list.is_empty()
        });
        // Edges into the main outputs.
        for (c, list) in spec.topology.outputs.iter_mut().enumerate() {
            list.retain(|s| {
                let gone = gone_source(s);
                if gone {
                    dropped.push(EdgeId {
                        sink: PortSink::Output(c as u16),
                        from: Edge::Direct(*s),
                    });
                }
                !gone
            });
        }
        for id in dropped {
            if !faded_out(spec, id) {
                out.edges.push(id);
            }
            spec.gains.remove(&id);
        }

        // Event edges.
        let mut gone_marks: Vec<(EventInputPort, EventOutputPort)> = Vec::new();
        spec.events.retain(|&at, list| {
            let sink_gone = at.node == key && at.port >= ein;
            list.retain(|e| {
                let from = e.from();
                let gone = sink_gone || (from.node == key && from.port >= eout);
                if gone {
                    out.event_edges.push((at, *e));
                    gone_marks.push((at, from));
                }
                !gone
            });
            !list.is_empty()
        });
        for mark in gone_marks {
            spec.required_resolution.remove(&mark);
        }

        // Bus edges.
        let bus_in_gone = |b: &NodeBus| b.node == key && b.resolve(shape.buses_in()).is_none();
        let feed_gone = |f: &BusFeed| {
            matches!(f.source, BusOutputPort::Node(b)
                if b.node == key && b.resolve(shape.buses_out()).is_none())
        };
        spec.bus_edges.retain(|to, feeds| {
            let sink_gone = bus_in_gone(to);
            feeds.retain(|f| {
                let gone = sink_gone || feed_gone(f);
                if gone && f.fade != EdgeFade::Out {
                    out.bus_edges.push((*to, *f));
                }
                !gone
            });
            !feeds.is_empty()
        });
        spec.main_output_sources.retain(|f| {
            let gone = feed_gone(f);
            if gone && f.fade != EdgeFade::Out {
                out.main_output_buses.push(*f);
            }
            !gone
        });

        // Param ports: a param the shape no longer declares loses its entry,
        // and a source whose port is gone stops driving.
        let params = &mut spec.params;
        params.retain(|at, m| {
            let undeclared = at.node == key
                && at.param != NodeMix::ADDR
                && shape.params.index_of(at.param).is_none();
            if undeclared && let Some(from) = m.source {
                out.params.push((*at, from));
            }
            !undeclared
        });
        for (at, m) in params.iter_mut() {
            if let Some(from) = m.source
                && from.node == key
                && from.port >= eout
            {
                m.source = None;
                out.params.push((*at, from));
            }
        }

        // A gain driven from an output the shape lacks reads its base.
        for (id, g) in spec.gains.iter_mut() {
            if let Some(from) = g.source
                && from.node == key
                && from.port >= eout
            {
                g.source = None;
                out.gains.push((*id, from));
            }
        }
        out
    }
}
