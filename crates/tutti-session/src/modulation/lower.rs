//! The lowering: modulation declarations, compiled, become the derived nodes
//! and edges the session builds.
//!
//! [`lower`] is a pure function from what [`compile`](tutti_modulation::compile)
//! made, with each target and source resolved against the graph, to a
//! [`Lowered`]: a lane per target with a program, a node per signal source and
//! the event edges between them. Nothing here reads the editor or keeps a
//! record; the diff against what is built is `derived`'s.

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::ChannelLayout;
use tutti_graph::{
    EdgeId, EventInputPort, EventOutputPort, NodeKey, NodeMix, ParamIn, ParamRange, PassError,
};
use tutti_modulation::{
    Compiled, ControlProgram, Declarations, ProgramCompileError, SignalSource, SourceKey, TargetKey,
};
use tutti_rt::ParamCell;
use tutti_units::{Normalized, ParamId};

/// What a target's lane drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drive {
    /// A param port of a node. The knob is the node's own cell for the param:
    /// the base the lane's offsets add to, written by whoever sets the param,
    /// so one cell has one writer. A route modulates around the base and its
    /// removal returns to it.
    Port(ParamIn),
    /// The mix of a node, through its own port: the lane clamps to the mix's
    /// range (`0..=1`), and its knob is the node's mix cell.
    Mix(NodeKey),
    /// The gain of a graph edge: the lane clamps to the entry's range. Its
    /// knob is the edge's own gain cell, in the gain's own units (`1.0` is
    /// unity). An edge with no entry gets one when the lane is built.
    Gain(EdgeId),
    /// A hosted plugin's parameter: the event input of its node the lane
    /// feeds. The lane's own cell starts at the plugin's value.
    Plugin {
        /// The event input of the node hosting the parameter.
        input: EventInputPort,
        /// The parameter.
        id: ParamId,
    },
}

impl Drive {
    /// The param port the lane's events go to, for a drive that is one.
    pub(crate) fn port(&self) -> Option<ParamIn> {
        match *self {
            Self::Port(at) => Some(at),
            Self::Mix(node) => Some(ParamIn {
                node,
                param: NodeMix::ADDR,
            }),
            Self::Gain(_) | Self::Plugin { .. } => None,
        }
    }
}

/// A target resolved: what its lane drives, and the cells it starts from.
#[derive(Clone, Debug)]
pub(crate) struct Resolved {
    pub(crate) drive: Drive,
    /// The node's unit generation: a new unit has new cells.
    pub(crate) generation: u32,
    /// The lane's base: the param's own cell, the node's mix cell or the
    /// edge's gain cell. `None`: no knob, the base is automation alone.
    pub(crate) knob: Option<ParamCell>,
    /// A plugin lane's first value.
    pub(crate) seed: Option<Normalized>,
}

impl Resolved {
    /// Whether a lane built for `self` still serves `other`: the structure of
    /// a lane is its drive, the unit it was built on and the knob cell.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.drive == other.drive
            && self.generation == other.generation
            && match (&self.knob, &other.knob) {
                (Some(a), Some(b)) => a.ptr_eq(b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// What feeds a signal source's node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feed {
    /// A follower's input: every audio output channel of the node, in order.
    Audio(NodeKey),
    /// A note envelope's MIDI.
    Events(EventOutputPort),
}

/// A signal source's node as lowered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SourcePlan {
    pub(crate) decl: SignalSource,
    pub(crate) feed: Option<Feed>,
    /// A follower's input width.
    pub(crate) width: ChannelLayout,
}

/// A target's lane as lowered.
#[derive(Clone, Debug)]
pub(crate) struct LanePlan {
    pub(crate) resolved: Resolved,
    pub(crate) program: ControlProgram,
    pub(crate) range: ParamRange,
}

/// The derived declarations: what the modulation declarations are in the
/// graph.
#[derive(Clone, Debug, Default)]
pub(crate) struct Lowered {
    pub(crate) lanes: BTreeMap<TargetKey, LanePlan>,
    pub(crate) sources: BTreeMap<SourceKey, SourcePlan>,
    /// Event edges, a source's node into a target's lane.
    pub(crate) edges: BTreeSet<(SourceKey, TargetKey)>,
}

/// Why the declarations do not lower.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Refusal {
    Uncompilable(ProgramCompileError),
    Cycle { route: tutti_modulation::RouteKey },
    NoSuchParam(TargetKey),
    NoSuchEventInput(TargetKey),
    Unplaced { target: TargetKey, error: PassError },
    SharedPort { first: TargetKey, second: TargetKey },
    WrongFeed(SourceKey),
}

/// The lanes, signal nodes and edges for `compiled`, whose targets and sources
/// resolved to `targets` and `sources` (a target or source that did not has no
/// entry: no lane, no node, and the edges that name one are dropped). A
/// target's range is its declaration's in `decl`.
///
/// # Errors
///
/// [`Refusal::SharedPort`] when two targets resolved to one port (or to one
/// edge's gain): one lane each would be two event sources on it.
pub(crate) fn lower(
    decl: &Declarations<'_>,
    compiled: &Compiled,
    mut targets: BTreeMap<TargetKey, Resolved>,
    sources: BTreeMap<SourceKey, SourcePlan>,
) -> Result<Lowered, Refusal> {
    let mut lanes = BTreeMap::new();
    let mut claimed: BTreeMap<ClaimedPort, TargetKey> = BTreeMap::new();
    for (&target, program) in &compiled.programs {
        let Some(resolved) = targets.remove(&target) else {
            continue;
        };
        if let Some(port) = ClaimedPort::of(&resolved.drive)
            && let Some(first) = claimed.insert(port, target)
        {
            return Err(Refusal::SharedPort {
                first,
                second: target,
            });
        }
        let (min, max) = decl
            .targets
            .iter()
            .find(|(k, _)| *k == target)
            .map_or((f64::NEG_INFINITY, f64::INFINITY), |(_, t)| (t.min, t.max));
        let range = ParamRange::new(min, max)
            .expect("a compiled target has no NaN bound: compile refuses it (NanRange)");
        lanes.insert(
            target,
            LanePlan {
                resolved,
                program: program.clone(),
                range,
            },
        );
    }
    let edges = compiled
        .edges
        .iter()
        .copied()
        .filter(|(s, t)| sources.contains_key(s) && lanes.contains_key(t))
        .collect();
    Ok(Lowered {
        lanes,
        sources,
        edges,
    })
}

/// What one lane takes over: an event source per param port or edge gain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ClaimedPort {
    Param(ParamIn),
    Gain(EdgeId),
}

impl ClaimedPort {
    fn of(drive: &Drive) -> Option<Self> {
        match drive {
            Drive::Gain(id) => Some(Self::Gain(*id)),
            Drive::Plugin { .. } => None,
            Drive::Port(_) | Drive::Mix(_) => drive.port().map(Self::Param),
        }
    }
}
