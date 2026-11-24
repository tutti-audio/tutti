//! [`Modulation`]: the reconcile step.
//!
//! A host declares sources, routes and targets ([`Declarations`]); this
//! compiles them ([`tutti_modulation::compile`]) and applies the difference to an
//! [`Editor`]: a [`ControlLaneNode`] per target with a program, a node per
//! signal source, and the event edges between them. It keeps what it built
//! (the last [`Compiled`], each target's lane, each source's node) and
//! touches nothing else in the graph.
//!
//! What it needs from the host and cannot derive is one seam, [`ModHost`]:
//! which graph node a target key or a source's input names.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::NodeKey;
use tutti_graph::{
    CommitError, EdgeGain, EdgeId, Editor, EventEdge, EventInputPort, EventKind, EventOutputPort,
    ForkByClone, NodeMix, ParamIn, ParamRange, PassError, PortSink, ScheduleError,
};
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_modulation::{
    Compiled, ControlProgram, Declarations, ProgramCompileError, RouteDecl, RouteKey, RouteTarget,
    SignalKind, SignalSource, Source as ModSource, SourceId, SourceKey, TargetDecl, TargetKey,
    compile,
};
use tutti_rt::ParamCell;
use tutti_units::{At, ParamAddr, ParamId};

use crate::automation::{
    ControlLaneNode, ForeignLaneConfig, ForeignLaneControls, LaneControls, UnitLaneConfig,
};
use crate::signal::{
    EnvelopeFollowerConfig, EnvelopeFollowerNode, NoteEnvelopeConfig, NoteEnvelopeNode,
};

/// What the host's keys name: the one question the reconciler asks of a host.
///
/// The keys in [`Declarations`] are the host's (an entity's bits, a hash);
/// the graph's are [`NodeKey`]s and [`EdgeId`]s. [`Modulation::apply`] asks on
/// every call, so a key that names another node than last time (a rebind) is
/// followed: its lane, or its source's input, is rebuilt.
pub trait ModHost {
    /// The node a target's key names (`TargetKey::Param`, `TargetKey::Mix`).
    /// `None` while the target's node is not in the graph: the target gets no
    /// lane yet (and loses one it had), and [`Applied::unresolved_targets`]
    /// lists it.
    fn node(&self, target: u64) -> Option<NodeKey>;

    /// The edge a target's key names (`TargetKey::Edge`), whose gain the
    /// target modulates. A host with no edge targets leaves the default,
    /// which names none.
    fn edge(&self, target: u64) -> Option<EdgeId> {
        let _ = target;
        None
    }

    /// What feeds signal source `source`'s node. `None`, or a feed naming a
    /// node not in the graph (removed since): a follower gets no node yet
    /// ([`Applied::unresolved_sources`]); a note envelope gets one with
    /// nothing on its MIDI input.
    fn feed(&self, source: SourceKey) -> Option<Feed>;
}

/// What a target's lane drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drive {
    /// A param port: a native param of a node, or the node's mix at
    /// [`NodeMix::ADDR`], whose lane clamps to the port's range (narrowed to
    /// `0..=1` for a mix). The knob is the node's own cell for the param (the
    /// host's, [`Modulation::apply`]'s `cell`), as the mix's is the node's mix
    /// cell ([`Editor::mix_cell`]): the base the lane's offsets add to,
    /// written by whoever sets the param, so one cell has one writer. A route
    /// modulates around the base and its removal returns to it.
    Port(ParamIn),
    /// The gain of a graph edge: the lane drives it through
    /// [`GraphSpec::connect_edge_gain`](tutti_graph::GraphSpec::connect_edge_gain)
    /// and clamps to the entry's range. Its knob is the edge's own gain cell
    /// ([`Editor::edge_gain_cell`]), in the gain's own units (`1.0` is
    /// unity). The edge's entry is made when the target first resolves (an
    /// edge with none is a plain addend), and the lane binds to the cells the
    /// entry made.
    Gain(EdgeId),
    /// A hosted plugin's parameter: the event input of its node the lane
    /// feeds. The lane's own cell starts at the plugin's value; later writes
    /// go through the lane's [`LaneControls::store`] ([`Modulation::lane`]).
    Plugin {
        /// The event input of the node hosting the parameter.
        input: EventInputPort,
        /// The parameter.
        id: ParamId,
    },
}

impl Drive {
    /// The node whose input the lane's value reaches: the param's node, the
    /// node of the port an edge goes into, and none for an edge into a main
    /// output channel (nothing a node reads).
    fn node(&self) -> Option<NodeKey> {
        match self {
            Self::Port(at) => Some(at.node),
            Self::Plugin { input, .. } => Some(input.node()),
            Self::Gain(id) => match id.sink {
                PortSink::Port(at) => Some(at.node()),
                PortSink::Output(_) => None,
            },
        }
    }
}

/// What feeds a signal source's node ([`ModHost::feed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// A follower's input: every audio output channel of the node, in
    /// order. The follower is as wide as the node's output.
    Audio(NodeKey),
    /// A note envelope's MIDI.
    Events(EventOutputPort),
}

/// Why [`Modulation::apply`] refused. A refusal changes nothing: neither
/// the editor nor the reconciler's own state.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ReconcileError {
    /// The declarations do not compile.
    #[error("modulation does not compile")]
    Compile(#[from] ProgramCompileError),
    /// A route from a signal source whose input is downstream of the route's
    /// own target: its edge would close a cycle.
    #[error("route {}: its source's input is downstream of its target", route.0)]
    Cycle {
        /// The route.
        route: RouteKey,
    },
    /// A native target whose node does not declare the param.
    #[error("target {0:?}: its node does not declare it")]
    NoSuchParam(TargetKey),
    /// A foreign target whose node has no event input at the port given.
    #[error("target {0:?}: no such event input")]
    NoSuchEventInput(TargetKey),
    /// A native target whose param port already takes an event source that
    /// is not one of this reconciler's lanes: the graph allows one per port.
    #[error("target {0:?}: its port has another event source")]
    PortTaken(TargetKey),
    /// Two native targets the host resolved to the same param port (or two
    /// edge targets to the same edge): one lane each would be two event
    /// sources on it.
    #[error("targets {first:?} and {second:?} resolve to one param port")]
    SharedPort {
        /// The first, in key order.
        first: TargetKey,
        /// The second.
        second: TargetKey,
    },
    /// A follower fed events, a note envelope fed audio, or a note
    /// envelope fed from an event output its node does not have. (A feed
    /// naming a node not in the graph is no feed: the follower is
    /// unresolved, the envelope gets none.)
    #[error("source {}: a feed it cannot take", .0.0)]
    WrongFeed(SourceKey),
    /// A mix target on a node with no pass ([`Editor::check_pass`]): the dry
    /// side of the mix would be a guess.
    #[error("target {target:?}: no mix without a pass: {error}")]
    Unplaced {
        /// The target.
        target: TargetKey,
        /// What the node would need to declare.
        error: PassError,
    },
}

/// What one [`Modulation::apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The editor's spec or units changed: a lane or a source was added,
    /// removed or rebuilt, or an edge moved. Commit it
    /// ([`Modulation::commit`]). A program, depth, range or knob edit alone
    /// needs none.
    pub commit: bool,
    /// The targets whose program was republished to their running lane
    /// (no commit).
    pub republished: Vec<TargetKey>,
    /// Targets with a program whose node the host did not resolve, or whose
    /// node or edge is not in the editor (never added, or removed since). No
    /// lane until a later `apply` resolves them, and a lane they had is
    /// removed. A node staged by the editor, committed or not, resolves.
    pub unresolved_targets: Vec<TargetKey>,
    /// Followers with no feed, or whose feed node is not in the graph: no
    /// node until a later `apply`.
    pub unresolved_sources: Vec<SourceKey>,
    /// Resyncs are owed ([`Modulation::resyncs_owed`]): call
    /// [`Modulation::commit`] (or [`Modulation::send_resyncs`] after a
    /// commit) even when [`commit`](Self::commit) is false.
    pub resync: bool,
}

/// A target resolved: what its lane drives and the cells it starts from.
#[derive(Clone, Debug)]
struct Bound {
    drive: Drive,
    /// The node's unit generation (a new unit has new cells); 0 until the
    /// editor has been asked.
    generation: u32,
    /// The lane's base: the param's own cell, the node's mix cell, or the
    /// edge's gain cell. `None`: no knob, the base is automation alone.
    knob: Option<ParamCell>,
    /// A plugin lane's first value.
    seed: Option<f64>,
}

impl Bound {
    /// Whether a lane built for `self` still serves `other`.
    fn same(&self, other: &Self) -> bool {
        // An edge's or a mix's entry makes its knob cell and range cell
        // together, so the knob's identity names both.
        self.drive == other.drive
            && self.generation == other.generation
            && match (&self.knob, &other.knob) {
                (Some(a), Some(b)) => a.ptr_eq(b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// A lane's controls: a plugin lane's also set the range it owns.
#[derive(Debug)]
enum Controls {
    Unit(LaneControls),
    Foreign(ForeignLaneControls),
}

impl Controls {
    fn lane(&self) -> &LaneControls {
        match self {
            Self::Unit(c) => c,
            Self::Foreign(c) => c.lane(),
        }
    }
}

/// A target's lane as built.
#[derive(Debug)]
struct Lane {
    key: NodeKey,
    /// The lane node's generation at its insert: another means a host
    /// replaced (or removed) it at its key.
    generation: u32,
    controls: Controls,
    bound: Bound,
    /// The program last published to it.
    program: ControlProgram,
    /// The range last stored, as bits.
    range: (u64, u64),
}

/// A signal source's node as built.
#[derive(Debug)]
struct SignalNode {
    key: NodeKey,
    /// As [`Lane::generation`].
    generation: u32,
    decl: SignalSource,
    feed: Option<Feed>,
    /// A follower's input width.
    width: ChannelLayout,
}

/// A source resolved and checked: what its node is built from.
struct SourcePlan {
    key: NodeKey,
    decl: SignalSource,
    feed: Option<Feed>,
    width: ChannelLayout,
}

/// The modulation reconciler: declarations in, lanes,
/// signal-source nodes and event edges in an [`Editor`] out.
///
/// Each [`apply`](Self::apply) compiles against the previous compile (so
/// sources keep their ids and rate edits are dated), then **diffs per
/// target**: a target with a new program gets a lane, one whose program
/// changed has it republished (no commit), one with no program loses its
/// lane; a new signal source gets its node, input and edges, a gone one
/// loses them. A range edit is a store into the port's range cell (or
/// a plugin lane's own), never a recompile of anything else. What needs a
/// commit is reported ([`Applied::commit`]); [`commit`](Self::commit)
/// commits and then sends the resyncs the apply queued.
///
/// **Resync.** A lane whose program newly reads a note envelope (a new
/// lane, or a route to the envelope added or re-added on an existing lane)
/// would read it as 0 until the envelope next moves, since a source sends
/// only when it moves and a lane frees an input slot its program stopped
/// reading. So [`commit`](Self::commit) schedules an
/// [`EventKind::Resync`] into the envelope's input: the envelope re-sends
/// where it stands, resumed. A scheduled command waits for the commit sent
/// before it, so the re-send reaches the edges that commit placed, and the
/// lane already reads the program the apply published (the publish
/// happens before the schedule). Nothing crosses outside the ports, so the
/// envelope stays `ForkByClone`: a fork's executor has its own command
/// ring and starts idle. A follower needs none: it sends on every grid
/// point while it is not silent.
///
/// **Rebinds.** A native target resolves against the node the editor holds
/// at its key, staged or committed: the unit declares its param ranges when
/// it is placed, so a node new since the last commit, or replaced by a unit
/// with another param list (which gets fresh range cells), resolves in the
/// same apply, and its lane goes out in the commit that sends the node. A
/// replace rebuilds the lane (a fresh lane sends at its first frame, so the
/// new unit hears it). An edge's gain and a node's mix resolve the same way;
/// an edge with no gain entry gets one from the apply. Two targets on one
/// param port are refused ([`ReconcileError::SharedPort`]); a target moved
/// from one key to another on the same port in one `apply` is not two.
/// A node this reconciler built whose key the host has since taken (a
/// replace or a removal) is forgotten, the wiring around it undone, and
/// built again at a fresh key.
///
/// **Unchanged inputs.** An apply whose inputs all equal the last one's
/// (the declarations, what the host resolves, the editor's
/// [`revision`](Editor::revision), the play clock's epoch) would change
/// nothing, so it returns at once ([`skipped`](Self::skipped)), without
/// compiling. A host may apply every frame. While a target or source is
/// unresolved every apply runs.
///
/// **Cycles.** A route from a signal source whose input is downstream of
/// the route's own target is refused by route ([`ReconcileError::Cycle`])
/// before anything is changed. The reconciler wires every source's input
/// itself (from [`ModHost::feed`]), so both ends are known in `apply`.
///
/// # Examples
///
/// The `tutti` umbrella's `examples/headless_modulation.rs` drives an LFO and a
/// follower on a filter's native params through a session, whose update
/// runs this reconciler. The shape of it with a plain `Editor`, for an LFO
/// on a filter's cutoff:
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use std::sync::Arc;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_modulation::{
///     LayerClock, Law, Declarations, LfoShape, Polarity, RouteDecl, RouteKey, RouteTarget,
///     Source, SourceKey, TargetDecl, TargetKey, Waveform,
/// };
/// use tutti_buffers::ChannelLayout;
/// use tutti_nodes::{Feed, ModHost, Modulation, SvfFilterConfig, SvfFilterNode, SvfType};
/// use tutti_graph::{NodeKey, ParamIn};
/// use tutti_units::{Bipolar, Hz, ParamAddr, ParamId, PhaseIncrement, SampleRate, Samples, UnitParam, Q};
/// use tutti_rt::Param;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// let filter = NodeKey(1);
/// let params = editor.insert(
///     filter,
///     "svf",
///     SvfFilterNode::<f32>::from(
///         SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
///             .freq(Hz::new(1_000.0))
///             .q(Q::new(0.7)),
///     ),
/// )
/// .controls;
///
/// struct Host(NodeKey);
/// impl ModHost for Host {
///     fn node(&self, target: u64) -> Option<NodeKey> {
///         (target == 1).then_some(self.0)
///     }
///     fn feed(&self, _: SourceKey) -> Option<Feed> {
///         None
///     }
/// }
/// let host = Host(filter);
/// // The node's own cell is the lane's knob; no plugin is hosted.
/// let cell = |at: ParamIn| params.cell(at.param);
/// let mirror = |_: NodeKey, _: ParamId| None;
///
/// let cutoff = TargetKey::param(1, ParamAddr::Unit(UnitParam::Cutoff));
/// let sources = [(
///     SourceKey(1),
///     Source::Lfo {
///         wave: Waveform::Shape { shape: LfoShape::Sine, seed: 0 },
///         clock: LayerClock::Seconds { rate: Hz::new(2.0), phase: PhaseIncrement::new(0.0), anchor: None },
///     },
/// )];
/// let routes = [(
///     RouteKey(1),
///     RouteDecl {
///         source: SourceKey(1),
///         to: RouteTarget::Param(cutoff),
///         depth: Param::new(Bipolar::of(0.3)),
///         polarity: Polarity::Bipolar,
///         curve: Law::Linear,
///     },
/// )];
/// let targets = [(cutoff, TargetDecl { min: 20.0, max: 20_000.0, automation: None })];
///
/// let mut modulation = Modulation::new();
/// let decl = Declarations { sources: &sources, routes: &routes, targets: &targets };
/// let applied = modulation.apply(&decl, &mut editor, &host, &cell, &mirror).expect("applies");
/// assert!(applied.commit, "a new lane is a commit");
/// // (The executor would adopt the first commit before this one is sent.)
/// # editor.collect();
///
/// // A depth write is the lane's to read from the route's cell: the apply
/// // finds nothing changed.
/// routes[0].1.depth.store(Bipolar::of(0.5));
/// let decl = Declarations { sources: &sources, routes: &routes, targets: &targets };
/// let applied = modulation.apply(&decl, &mut editor, &host, &cell, &mirror).expect("applies");
/// assert!(!applied.commit);
/// assert!(applied.republished.is_empty());
/// assert_eq!(modulation.skipped(), 1);
/// ```
#[derive(Default, Debug)]
pub struct Modulation {
    prev: Option<Compiled>,
    lanes: BTreeMap<TargetKey, Lane>,
    sources: BTreeMap<SourceKey, SignalNode>,
    /// Event edges as wired: source → target.
    edges: BTreeSet<(SourceKey, TargetKey)>,
    /// Note envelopes owed a resync at the next commit.
    resync: BTreeSet<SourceKey>,
    /// Resyncs the editor refused for good (no such port, a resolution
    /// too coarse, a poisoned executor) and that were dropped.
    resyncs_dropped: u64,
    /// What the last apply was a function of, when it left nothing
    /// unresolved: an apply over the same is skipped.
    memo: Option<Memo>,
    /// Applies skipped so.
    skipped: u64,
}

/// Every input of an [`apply`](Modulation::apply) whose result was kept,
/// as it stood then. An apply over equal inputs would compile the same
/// programs, resolve the same binds, and so change nothing: it is skipped.
///
/// The inputs, each compared here:
///
/// - **The declarations**, by value: every source (an LFO's rate, wave and
///   phase, a macro's value, a follower's times, an envelope's ADSR), every
///   route (source, target, depth cell, polarity, curve; a route added, removed
///   or moved; a depth route's depth, by bits, which is read when compiled), every target (its range, by bits, and its automation curve,
///   by identity as a program holds it; a take opened or closed).
/// - **The host's resolutions**, by value, for the targets with a program
///   and the signal sources: which node a key names now (a node added,
///   removed or rebound), a native target's knob cell (a replaced node
///   hands another), a foreign target's node and port (a plugin whose
///   values are not read, or whose list lost the parameter, resolves to
///   nothing), and each source's feed. A foreign knob's value is not
///   compared: it only seeds a new lane, and a standing lane keeps its own.
/// - **The editor**, by [`Editor::revision`]: the spec, the shapes and
///   generations (a node inserted, replaced, crossfaded or removed, by the
///   host or over one of this reconciler's own keys), param ranges and their
///   cells, and the `Prepare` (a sample-rate change). Recorded after
///   the apply's own edits, which its records already hold.
/// - **The play clock's epoch** (`Editor::play_reading`): the compile
///   carries a rate edit's anchor only within its epoch, so a new play
///   start drops it from the program.
///
/// Floats compare by value, except a target's range and an envelope's
/// ADSR, which compare by bits. So elsewhere `0.0` and `-0.0` read as
/// equal, and a NaN never equals itself: an apply over one is never
/// skipped.
///
/// The revision belongs to one editor: a new [`Editor`] counts from 0
/// again, so a `Modulation` must be applied to one editor for its life
/// (a session builds one editor and a restart reuses it).
///
/// Not kept while a target or a source is unresolved: it resolves when its
/// node or edge is added, and a host applies again to find out (and times how
/// long).
#[derive(Debug)]
struct Memo {
    sources: Vec<(SourceKey, ModSource)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    scalers: Vec<(RouteKey, u64)>,
    targets: Vec<(TargetKey, TargetDecl)>,
    resolved: Vec<(TargetKey, Option<Bound>)>,
    feeds: Vec<(SourceKey, Option<Feed>)>,
    revision: u64,
    epoch: u32,
}

/// The depth of each depth route, by bits: the one depth read when compiled
/// (a route into a param is read by its lane from the cell).
fn scalers<'a>(decl: &Declarations<'a>) -> impl Iterator<Item = (RouteKey, u64)> + 'a {
    decl.routes
        .iter()
        .filter(|(_, route)| matches!(route.to, RouteTarget::Depth(_)))
        .map(|(key, route)| (*key, route.depth.load().get().to_bits()))
}

fn same_target(a: &TargetDecl, b: &TargetDecl) -> bool {
    a.min.to_bits() == b.min.to_bits()
        && a.max.to_bits() == b.max.to_bits()
        && match (&a.automation, &b.automation) {
            (Some(x), Some(y)) => std::ptr::addr_eq(Arc::as_ptr(x), Arc::as_ptr(y)),
            (None, None) => true,
            _ => false,
        }
}

impl Memo {
    /// Whether an apply of `decl` now would see what the kept one saw.
    fn holds<F: Float>(
        &self,
        decl: &Declarations<'_>,
        editor: &Editor<F>,
        host: &dyn ModHost,
        cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
        mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<f64>>,
    ) -> bool {
        self.revision == editor.revision()
            && self.epoch == editor.play_reading().epoch
            && self.sources.as_slice() == decl.sources
            && self.routes.as_slice() == decl.routes
            && self.scalers.iter().copied().eq(scalers(decl))
            && self.targets.len() == decl.targets.len()
            && self
                .targets
                .iter()
                .zip(decl.targets)
                .all(|((k, a), (k2, b))| k == k2 && same_target(a, b))
            && self.feeds.iter().all(|(s, feed)| host.feed(*s) == *feed)
            && self
                .resolved
                .iter()
                .all(|(t, was)| match (was, ask(*t, host, cell, mirror)) {
                    (None, None) => true,
                    (Some(was), Some(now)) => was.same(&now),
                    _ => false,
                })
    }
}

impl Modulation {
    /// Nothing applied yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `target`'s lane: its node key and its controls (a plugin lane's knob,
    /// the counters). `None` while it has none.
    pub fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.lanes.get(&target).map(|l| (l.key, l.controls.lane()))
    }

    /// The lanes with a knob that drive the plugin parameters of `node`, by
    /// parameter id.
    pub fn plugin_lanes(&self, node: NodeKey) -> impl Iterator<Item = (ParamId, &LaneControls)> {
        self.lanes
            .values()
            .filter_map(move |lane| match lane.bound.drive {
                Drive::Plugin { input, id } if input.node() == node => {
                    let controls = lane.controls.lane();
                    controls.value().is_some().then_some((id, controls))
                }
                _ => None,
            })
    }

    /// Frees, on this thread, the programs every lane has let go of since
    /// its last publish ([`LaneControls::reclaim`]), without publishing;
    /// returns how many are still held. A lane republished once and then
    /// left alone would otherwise keep a program a block held across that
    /// publish until the lane is published again. Control thread, once per
    /// tick of the host's loop (a session's `Reclaim` phase).
    pub fn reclaim(&self) -> usize {
        self.lanes
            .values()
            .map(|lane| lane.controls.lane().reclaim())
            .sum()
    }

    /// Signal source `source`'s node, `None` while it has none.
    pub fn source(&self, source: SourceKey) -> Option<NodeKey> {
        self.sources.get(&source).map(|s| s.key)
    }

    /// Note envelopes owed a resync: the next [`commit`](Self::commit) (or
    /// [`send_resyncs`](Self::send_resyncs)) schedules them.
    pub fn resyncs_owed(&self) -> usize {
        self.resync.len()
    }

    /// How many applies found every input the last one kept unchanged (the
    /// declarations, the host's resolutions, the editor's
    /// [`revision`](Editor::revision) and the play clock's epoch) and so
    /// did nothing at all: no compile, no diff.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Resyncs dropped because the editor refused them for good
    /// ([`send_resyncs`](Self::send_resyncs)).
    pub fn resyncs_dropped(&self) -> u64 {
        self.resyncs_dropped
    }

    /// Compiles `decl` and applies the difference to `editor`, resolving
    /// keys through `host`. A native target's knob is `cell(at)`, the node's
    /// own cell for the param (`None`: no knob, the base is automation
    /// alone). A plugin target's lane starts at `mirror(node, id)`, the
    /// plugin's value: `None` while it is not read (the target stays
    /// unresolved, since a lane built without a knob could not take one
    /// later), `Some(None)` for a parameter the node does not list. Control
    /// thread; it may allocate. Commit
    /// afterwards when [`Applied::commit`] or [`Applied::resync`] says so,
    /// through [`commit`](Self::commit), or through `editor.commit()`
    /// followed by [`send_resyncs`](Self::send_resyncs). A lane and its
    /// target staged since the last commit go out in that one plan.
    ///
    /// # Errors
    ///
    /// Each [`ReconcileError`]; nothing changes on any of them.
    #[allow(
        clippy::missing_panics_doc,
        reason = "the expects run after validation: every key they look up was listed by this call, and a declaration that compiled builds its node"
    )]
    pub fn apply<F: Float>(
        &mut self,
        decl: &Declarations<'_>,
        editor: &mut Editor<F>,
        host: &dyn ModHost,
        cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
        mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<f64>>,
    ) -> Result<Applied, ReconcileError> {
        if self
            .memo
            .as_ref()
            .is_some_and(|m| m.holds(decl, editor, host, cell, mirror))
        {
            self.skipped += 1;
            return Ok(Applied {
                resync: !self.resync.is_empty(),
                ..Applied::default()
            });
        }
        self.memo = None;
        let now = editor.play_reading();
        let compiled = compile(
            decl,
            self.prev.as_ref(),
            now,
            editor.prepare().sample_rate(),
        )?;
        let mut applied = Applied::default();
        let mut resolved = Vec::with_capacity(compiled.programs.len());
        let mut feeds = Vec::with_capacity(compiled.nodes.len());

        // Nodes the reconciler built whose key a host has since replaced or
        // removed: their records go, and their wiring is undone.
        let taken_lanes: BTreeSet<TargetKey> = self
            .lanes
            .iter()
            .filter(|(_, l)| taken(editor, l.key, l.generation))
            .map(|(&t, _)| t)
            .collect();
        let taken_sources: BTreeSet<SourceKey> = self
            .sources
            .iter()
            .filter(|(_, n)| taken(editor, n.key, n.generation))
            .map(|(&s, _)| s)
            .collect();

        // Resolve and check, changing nothing.
        // The event sources this reconciler wired into param ports: each
        // lane's key at its bound port. One that stays has its target's
        // bind on that port (two binds on a port are refused below); every
        // other is removed (a lane whose target went, moved or was rebound)
        // or unwired (one whose key a host took) before a new lane
        // connects. Any other event source at a port is someone else's.
        let own: BTreeSet<(ParamIn, NodeKey)> = self
            .lanes
            .values()
            .filter_map(|l| match l.bound.drive {
                Drive::Port(at) => Some((at, l.key)),
                Drive::Plugin { .. } | Drive::Gain(_) => None,
            })
            .collect();
        // The same for the lanes wired into an edge's gain.
        let own_edges: BTreeSet<(EdgeId, NodeKey)> = self
            .lanes
            .values()
            .filter_map(|l| match l.bound.drive {
                Drive::Gain(id) => Some((id, l.key)),
                _ => None,
            })
            .collect();
        let mut bounds: BTreeMap<TargetKey, Bound> = BTreeMap::new();
        let mut claimed: BTreeMap<ParamIn, TargetKey> = BTreeMap::new();
        let mut claimed_edges: BTreeMap<EdgeId, TargetKey> = BTreeMap::new();
        // Edges that have no entry yet: it is made below, once the apply has
        // checked out, and the target binds to its cells there.
        let mut to_enter: Vec<(EdgeId, TargetKey)> = Vec::new();
        for &target in compiled.programs.keys() {
            let asked = ask(target, host, cell, mirror);
            resolved.push((target, asked.clone()));
            if let Some(Bound {
                drive: Drive::Gain(id),
                ..
            }) = &asked
                && editor.spec().has_edge(*id)
                && editor.edge_gain_cell(*id).is_none()
            {
                to_enter.push((*id, target));
            }
            match self.resolve_target(target, asked, &own, &own_edges, editor)? {
                Some(bound) => {
                    match bound.drive {
                        Drive::Port(at) => {
                            if let Some(&first) = claimed.get(&at) {
                                return Err(ReconcileError::SharedPort {
                                    first,
                                    second: target,
                                });
                            }
                            claimed.insert(at, target);
                        }
                        Drive::Gain(id) => {
                            if let Some(&first) = claimed_edges.get(&id) {
                                return Err(ReconcileError::SharedPort {
                                    first,
                                    second: target,
                                });
                            }
                            claimed_edges.insert(id, target);
                        }
                        Drive::Plugin { .. } => {}
                    }
                    bounds.insert(target, bound);
                }
                None => applied.unresolved_targets.push(target),
            }
        }
        let mut plans = BTreeMap::new();
        for (&source, &node) in &compiled.nodes {
            let kept = !taken_sources.contains(&source);
            let feed = host.feed(source);
            feeds.push((source, feed));
            match self.resolve_source(source, node, kept, feed, editor)? {
                Some(plan) => {
                    plans.insert(source, plan);
                }
                None => applied.unresolved_sources.push(source),
            }
        }
        let edges: BTreeSet<(SourceKey, TargetKey)> = compiled
            .edges
            .iter()
            .copied()
            .filter(|(s, t)| plans.contains_key(s) && bounds.contains_key(t))
            .collect();
        let taken_keys: BTreeSet<NodeKey> = taken_lanes
            .iter()
            .map(|t| self.lanes[t].key)
            .chain(taken_sources.iter().map(|s| self.sources[s].key))
            .collect();
        self.check_cycles(decl, editor, &bounds, &plans, &edges, &taken_keys)?;

        // Apply.
        let targets: BTreeMap<TargetKey, (f64, f64)> = decl
            .targets
            .iter()
            .map(|(k, d)| (*k, (d.min, d.max)))
            .collect();
        let commit = &mut applied.commit;
        // The entries of the edges that had none, and the cells the targets
        // bind to.
        for (id, target) in to_enter {
            let (min, max) = targets[&target];
            editor
                .set_edge_gain_range(id, declared_range(min, max))
                .expect("checked: the edge exists");
            if let Some(bound) = bounds.get_mut(&target) {
                bound.knob = editor.edge_gain_cell(id);
            }
            *commit = true;
        }
        // Undo what was wired around a taken node, leaving the host's node
        // and its own wiring alone, then forget it.
        for &(s, t) in &self.edges {
            if !(taken_sources.contains(&s) || taken_lanes.contains(&t)) {
                continue;
            }
            if let (Some(src), Some(lane)) = (self.sources.get(&s), self.lanes.get(&t)) {
                remove_event_edge(
                    editor,
                    EventInputPort::new(lane.key, 0),
                    EventOutputPort::new(src.key, 0),
                );
            }
        }
        for t in &taken_lanes {
            let lane = self.lanes.remove(t).expect("listed");
            let out = EventOutputPort::new(lane.key, 0);
            match lane.bound.drive {
                Drive::Port(at) => {
                    // Only the source it wired: a port another source has
                    // taken since is left alone.
                    editor.spec_mut().disconnect_param(at, out);
                }
                Drive::Plugin { input, .. } => remove_event_edge(editor, input, out),
                Drive::Gain(id) => {
                    editor.spec_mut().disconnect_edge_gain(id, out);
                }
            }
            *commit = true;
        }
        // Rebuilt at another key (their key was taken) or at their own (a
        // declaration or width edit): the lanes that read one hold the old
        // node's last value until the new one sends.
        let mut rebuilt = BTreeSet::new();
        for s in &taken_sources {
            let node = self.sources.remove(s).expect("listed");
            unwire_feed(editor, node.key, node.feed, node.width);
            if plans.contains_key(s) {
                rebuilt.insert(*s);
            }
            *commit = true;
        }
        let gone: Vec<TargetKey> = self
            .lanes
            .iter()
            .filter(|(t, lane)| bounds.get(t).is_none_or(|b| !lane.bound.same(b)))
            .map(|(&t, _)| t)
            .collect();
        for t in gone {
            let lane = self.lanes.remove(&t).expect("listed");
            editor.remove(lane.key);
            *commit = true;
        }
        let gone: Vec<SourceKey> = self
            .sources
            .iter()
            .filter(|(s, n)| {
                plans
                    .get(s)
                    .is_none_or(|p| p.decl != n.decl || p.width != n.width)
            })
            .map(|(&s, _)| s)
            .collect();
        for s in gone {
            let node = self.sources.remove(&s).expect("listed");
            editor.remove(node.key);
            if plans.contains_key(&s) {
                rebuilt.insert(s);
            }
            *commit = true;
        }
        // The edges whose ends survive and are no longer wanted.
        let stale: Vec<(SourceKey, TargetKey)> = self
            .edges
            .iter()
            .copied()
            .filter(|e| !edges.contains(e))
            .collect();
        for (s, t) in stale {
            if let (Some(src), Some(lane)) = (self.sources.get(&s), self.lanes.get(&t)) {
                editor.spec_mut().disconnect_events(
                    EventInputPort::new(lane.key, 0),
                    EventOutputPort::new(src.key, 0),
                );
                *commit = true;
            }
            self.edges.remove(&(s, t));
        }
        // Edges whose source or lane went with a removed node.
        self.edges
            .retain(|(s, t)| self.sources.contains_key(s) && self.lanes.contains_key(t));

        for (source, plan) in plans {
            if let Some(node) = self.sources.get_mut(&source) {
                if node.feed != plan.feed {
                    unwire_feed(editor, node.key, node.feed, node.width);
                    wire_feed(editor, node.key, plan.feed);
                    node.feed = plan.feed;
                    *commit = true;
                }
                continue;
            }
            // A rebuilt source sends where it stands (0) on its first frame: a
            // lane holds the old node's last value until it does.
            let replacing = rebuilt.contains(&source);
            match plan.decl.kind {
                SignalKind::Follower { .. } => {
                    let mut node = EnvelopeFollowerNode::from(
                        EnvelopeFollowerConfig::from_decl(plan.width, &plan.decl)
                            .expect("a follower's declaration"),
                    );
                    if replacing {
                        node = node.replacing();
                    }
                    editor.insert(plan.key, "envelope-follower", ForkByClone(node));
                }
                SignalKind::NoteEnvelope { .. } => {
                    let mut node = NoteEnvelopeNode::from(
                        NoteEnvelopeConfig::from_decl(&plan.decl).expect("a note envelope's"),
                    );
                    if replacing {
                        node = node.replacing();
                    }
                    editor.insert(plan.key, "note-envelope", ForkByClone(node));
                }
            }
            wire_feed(editor, plan.key, plan.feed);
            self.sources.insert(
                source,
                SignalNode {
                    key: plan.key,
                    generation: editor.spec().generation(plan.key),
                    decl: plan.decl,
                    feed: plan.feed,
                    width: plan.width,
                },
            );
            *commit = true;
        }

        for (target, bound) in bounds {
            let program = &compiled.programs[&target];
            let (min, max) = targets[&target];
            let range_bounds = declared_range(min, max);
            let range = (min.to_bits(), max.to_bits());
            if let Some(lane) = self.lanes.get_mut(&target) {
                // Sources the new program reads that the published one did
                // not: the lane frees their slots on adoption (or never had
                // them), so a note envelope among them owes a resync.
                owe_resyncs(&mut self.resync, &compiled, Some(&lane.program), program);
                if lane.program != *program {
                    lane.controls.lane().publish(program.clone());
                    lane.program = program.clone();
                    applied.republished.push(target);
                }
                if lane.range != range {
                    store_range(editor, &lane.bound, &lane.controls, range_bounds);
                    lane.range = range;
                }
                continue;
            }
            owe_resyncs(&mut self.resync, &compiled, None, program);
            let key = unused_key(editor);
            let lane = match bound.drive {
                Drive::Port(at) => {
                    editor
                        .set_param_range(at, range_bounds)
                        .expect("checked: a node, and compile refuses a crossed arc");
                    let cell = editor.param_range_cell(at);
                    let mut config = UnitLaneConfig::new(at.param, program.clone())
                        .range(cell.expect("a bound param has its range cell from its staging"));
                    if let Some(knob) = bound.knob.clone() {
                        config = config.knob(knob);
                    }
                    let node = ControlLaneNode::from(config);
                    let controls = editor.insert(key, "control-lane", node).controls;
                    editor
                        .spec_mut()
                        .connect_param(at, EventOutputPort::new(key, 0));
                    Controls::Unit(controls)
                }
                Drive::Gain(id) => {
                    editor
                        .set_edge_gain_range(id, range_bounds)
                        .expect("checked: the edge");
                    let (knob, range) = editor
                        .edge_gain_cell(id)
                        .zip(editor.edge_gain_range_cell(id))
                        .expect("declared above");
                    let node = ControlLaneNode::from(
                        UnitLaneConfig::new(EdgeGain::ADDR, program.clone())
                            .knob(knob)
                            .range(range),
                    );
                    let controls = editor.insert(key, "control-lane", node).controls;
                    editor
                        .spec_mut()
                        .connect_edge_gain(id, EventOutputPort::new(key, 0));
                    Controls::Unit(controls)
                }
                Drive::Plugin { input, id } => {
                    let mut config = ForeignLaneConfig::new(id, program.clone(), range_bounds);
                    if let Some(knob) = bound.seed {
                        config = config.knob(knob);
                    }
                    let (node, controls) = config.build();
                    editor.insert(key, "control-lane", node);
                    editor
                        .spec_mut()
                        .connect_events(input, EventEdge::Direct(EventOutputPort::new(key, 0)));
                    Controls::Foreign(controls)
                }
            };
            self.lanes.insert(
                target,
                Lane {
                    key,
                    generation: editor.spec().generation(key),
                    controls: lane,
                    bound,
                    program: program.clone(),
                    range,
                },
            );
            *commit = true;
        }

        for &(s, t) in &edges {
            if self.edges.insert((s, t)) {
                let (src, lane) = (self.sources[&s].key, self.lanes[&t].key);
                editor.spec_mut().connect_events(
                    EventInputPort::new(lane, 0),
                    EventEdge::Direct(EventOutputPort::new(src, 0)),
                );
                *commit = true;
            }
        }
        // A resync owed to a source with no node (unresolved, or gone) is
        // owed to nothing.
        let sources = &self.sources;
        self.resync.retain(|s| {
            sources
                .get(s)
                .is_some_and(|n| matches!(n.decl.kind, SignalKind::NoteEnvelope { .. }))
        });
        self.prev = Some(compiled);
        applied.resync = !self.resync.is_empty();
        if applied.unresolved_targets.is_empty() && applied.unresolved_sources.is_empty() {
            self.memo = Some(Memo {
                sources: decl.sources.to_vec(),
                routes: decl.routes.to_vec(),
                scalers: scalers(decl).collect(),
                targets: decl.targets.to_vec(),
                resolved,
                feeds,
                revision: editor.revision(),
                epoch: now.epoch,
            });
        }
        Ok(applied)
    }

    /// Commits `editor`, then [`send_resyncs`](Self::send_resyncs). Call it
    /// when [`Applied::commit`] or [`Applied::resync`] is set (a commit with
    /// nothing changed sends no plan).
    ///
    /// # Errors
    ///
    /// The commit's [`CommitError`]; the resyncs then stay owed. The
    /// resyncs' own outcome is `send_resyncs`': one refused for now stays
    /// owed ([`resyncs_owed`](Self::resyncs_owed) is then not 0: call
    /// `send_resyncs` again on a later frame), one refused for good is
    /// dropped and counted ([`resyncs_dropped`](Self::resyncs_dropped)).
    pub fn commit<F: Float>(&mut self, editor: &mut Editor<F>) -> Result<(), CommitError> {
        editor.commit()?;
        // Not an error of the commit: kept owed or counted, as documented.
        let _ = self.send_resyncs(editor);
        Ok(())
    }

    /// Schedules the owed resyncs ([`EventKind::Resync`] at
    /// [`At::NextBlock`] into each note envelope's input). Call it after the
    /// commit an [`apply`](Self::apply) asked for has been sent: a scheduled
    /// command waits for that commit to land.
    ///
    /// # Errors
    ///
    /// The first [`ScheduleError`]. A resync refused for now
    /// ([`ScheduleError::Backpressure`], or [`ScheduleError::NoPlan`]
    /// before any commit) stays owed for the next call; one refused for good
    /// (no such port, a resolution too coarse, a poisoned executor) is
    /// dropped and counted in [`resyncs_dropped`](Self::resyncs_dropped).
    pub fn send_resyncs<F: Float>(&mut self, editor: &mut Editor<F>) -> Result<(), ScheduleError> {
        let owed = std::mem::take(&mut self.resync);
        let mut first = Ok(());
        for s in owed {
            let Some(node) = self.sources.get(&s) else {
                continue;
            };
            let to = EventInputPort::new(node.key, 0);
            if let Err(e) = editor.schedule(At::NextBlock, to, EventKind::Resync) {
                if e.is_transient() {
                    self.resync.insert(s);
                } else {
                    self.resyncs_dropped += 1;
                }
                if first.is_ok() {
                    first = Err(e);
                }
            }
        }
        first
    }

    /// `target`, as asked of the host (`asked`), checked against `editor`.
    fn resolve_target<F: Float>(
        &self,
        target: TargetKey,
        asked: Option<Bound>,
        own: &BTreeSet<(ParamIn, NodeKey)>,
        own_edges: &BTreeSet<(EdgeId, NodeKey)>,
        editor: &Editor<F>,
    ) -> Result<Option<Bound>, ReconcileError> {
        let Some(mut bound) = asked else {
            return Ok(None);
        };
        match bound.drive {
            Drive::Port(at) => {
                if at.param == NodeMix::ADDR {
                    // A node that is not in the graph (yet): no lane.
                    if !editor.spec().topology.nodes.contains_key(&at.node) {
                        return Ok(None);
                    }
                    editor
                        .check_pass(at.node)
                        .map_err(|error| ReconcileError::Unplaced { target, error })?;
                    bound.knob = Some(
                        editor
                            .mix_cell(at.node)
                            .expect("a node in the spec has its mix cell from its staging"),
                    );
                } else {
                    let Some(shape) = editor.shapes().get(&at.node) else {
                        return Ok(None);
                    };
                    if shape.params.index_of(at.param).is_none() {
                        return Err(ReconcileError::NoSuchParam(target));
                    }
                    bound.generation = editor.spec().generation(at.node);
                }
                // One event source per param port: connecting a lane would
                // replace another's (a host's own), silently.
                let other = editor
                    .spec()
                    .params
                    .get(&at)
                    .is_some_and(|out| !own.contains(&(at, out.node())));
                if other {
                    return Err(ReconcileError::PortTaken(target));
                }
            }
            Drive::Gain(id) => {
                // An edge that is not in the graph (yet): no lane.
                if !editor.spec().has_edge(id) {
                    return Ok(None);
                }
                // One event source per gain: connecting a lane would replace
                // another's (a host's own), silently.
                let other = editor
                    .spec()
                    .gains
                    .get(&id)
                    .and_then(|g| g.source)
                    .is_some_and(|out| !own_edges.contains(&(id, out.node())));
                if other {
                    return Err(ReconcileError::PortTaken(target));
                }
                // An edge with no entry has no cells: the apply declares the
                // entry before it builds the lane.
                bound.knob = editor.edge_gain_cell(id);
            }
            Drive::Plugin { input, .. } => {
                let Some(shape) = editor.shapes().get(&input.node()) else {
                    return Ok(None);
                };
                if input.port() >= shape.event_in {
                    return Err(ReconcileError::NoSuchEventInput(target));
                }
                bound.generation = editor.spec().generation(input.node());
            }
        }
        Ok(Some(bound))
    }

    /// `source`'s node, fed by what the host resolved (`feed`), checked
    /// against `editor`: its key (kept, or fresh), its feed and a
    /// follower's width.
    fn resolve_source<F: Float>(
        &self,
        source: SourceKey,
        decl: SignalSource,
        kept: bool,
        feed: Option<Feed>,
        editor: &Editor<F>,
    ) -> Result<Option<SourcePlan>, ReconcileError> {
        // A feed naming a node not in the graph (removed since) is no feed.
        let feed = feed.filter(|f| {
            let node = match f {
                Feed::Audio(n) => *n,
                Feed::Events(out) => out.node(),
            };
            editor.shapes().contains_key(&node)
        });
        let width = match (decl.kind, feed) {
            (SignalKind::Follower { .. }, None) => return Ok(None),
            (SignalKind::Follower { .. }, Some(Feed::Audio(node))) => {
                editor.shapes()[&node].audio_out()
            }
            (SignalKind::NoteEnvelope { .. }, None) => ChannelLayout::EMPTY,
            (SignalKind::NoteEnvelope { .. }, Some(Feed::Events(from))) => {
                if from.port() >= editor.shapes()[&from.node()].event_out {
                    return Err(ReconcileError::WrongFeed(source));
                }
                ChannelLayout::EMPTY
            }
            _ => return Err(ReconcileError::WrongFeed(source)),
        };
        // Kept while its declaration and width hold; rebuilt at the same
        // key otherwise (the old node is removed first), or at a new one
        // when a host took its key (`kept` false).
        let key = self
            .sources
            .get(&source)
            .filter(|_| kept)
            .map_or_else(|| unused_key(editor), |n| n.key);
        Ok(Some(SourcePlan {
            key,
            decl,
            feed,
            width,
        }))
    }

    /// Refuses, by route, an edge from a signal source to a target whose
    /// node reaches the source's input. The graph is the spec
    /// as it stands without this reconciler's own lanes and source wiring,
    /// plus what this apply would wire: each source's feed, and each edge as
    /// source → target node.
    fn check_cycles<F: Float>(
        &self,
        decl: &Declarations<'_>,
        editor: &Editor<F>,
        bounds: &BTreeMap<TargetKey, Bound>,
        plans: &BTreeMap<SourceKey, SourcePlan>,
        edges: &BTreeSet<(SourceKey, TargetKey)>,
        taken: &BTreeSet<NodeKey>,
    ) -> Result<(), ReconcileError> {
        if edges.is_empty() {
            return Ok(());
        }
        // A key a host has taken holds the host's node now: its edges count.
        let ours: BTreeSet<NodeKey> = self
            .lanes
            .values()
            .map(|l| l.key)
            .chain(self.sources.values().map(|s| s.key))
            .filter(|k| !taken.contains(k))
            .collect();
        let mut next: BTreeMap<NodeKey, BTreeSet<NodeKey>> = BTreeMap::new();
        let mut link = |from: NodeKey, to: NodeKey| {
            if !ours.contains(&from) && !ours.contains(&to) {
                next.entry(from).or_default().insert(to);
            }
        };
        let spec = editor.spec();
        for (at, edge) in spec
            .topology
            .edges
            .iter()
            .flat_map(|(at, es)| es.iter().map(move |e| (at, e)))
        {
            // A feedback edge is a declared cycle, delayed: not a path.
            if let Edge::Direct(PortSource::Node(p)) = edge {
                link(p.node(), at.node());
            }
        }
        for (at, sources) in &spec.events {
            for e in sources {
                if let EventEdge::Direct(from) = e {
                    link(from.node(), at.node());
                }
            }
        }
        for (at, from) in &spec.params {
            link(from.node(), at.node);
        }
        // A lane driving the gain of an edge into a port runs before the
        // node.
        for (id, g) in &spec.gains {
            if let (PortSink::Port(at), Some(from)) = (id.sink, g.source) {
                link(from.node(), at.node());
            }
        }
        // What this apply wires, by plan: the feed into each source, and
        // each source straight to its target's node (the lane between them
        // only passes it on).
        for plan in plans.values() {
            let from = match plan.feed {
                Some(Feed::Audio(n)) => n,
                Some(Feed::Events(p)) => p.node(),
                None => continue,
            };
            next.entry(from).or_default().insert(plan.key);
        }
        for (s, t) in edges {
            if let Some(node) = bounds[t].drive.node() {
                next.entry(plans[s].key).or_default().insert(node);
            }
        }
        let reaches = |from: NodeKey, to: NodeKey| {
            let mut seen = BTreeSet::new();
            let mut stack = vec![from];
            while let Some(n) = stack.pop() {
                if n == to {
                    return true;
                }
                if seen.insert(n) {
                    stack.extend(next.get(&n).into_iter().flatten().copied());
                }
            }
            false
        };
        let mut routes: Vec<&(RouteKey, tutti_modulation::RouteDecl)> =
            decl.routes.iter().collect();
        routes.sort_by_key(|(k, _)| *k);
        for (key, route) in routes {
            let RouteTarget::Param(t) = route.to else {
                continue;
            };
            if !edges.contains(&(route.source, t)) {
                continue;
            }
            if bounds[&t]
                .drive
                .node()
                .is_some_and(|node| reaches(node, plans[&route.source].key))
            {
                return Err(ReconcileError::Cycle { route: *key });
            }
        }
        Ok(())
    }
}

/// What the host's keys name for `target`, and the cells its lane starts from:
/// a native param's own cell (`cell`), a plugin parameter's first value
/// (`mirror`, `None` while it is not read, `Some(None)` for one the node does
/// not list). `None` while the key names nothing, or the plugin's value is not
/// read: a lane built without a knob could not take one later.
fn ask(
    target: TargetKey,
    host: &dyn ModHost,
    cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
    mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<f64>>,
) -> Option<Bound> {
    let (drive, knob, seed) = match target {
        TargetKey::Param { node, param } => {
            let node = host.node(node)?;
            match param.as_unit() {
                Some(param) => {
                    let at = ParamIn { node, param };
                    (Drive::Port(at), cell(at), None)
                }
                None => {
                    let ParamAddr::Id(id) = param else {
                        return None;
                    };
                    let input = EventInputPort::new(node, 0);
                    (Drive::Plugin { input, id }, None, mirror(node, id)?)
                }
            }
        }
        TargetKey::Mix(node) => {
            let at = ParamIn {
                node: host.node(node)?,
                param: NodeMix::ADDR,
            };
            (Drive::Port(at), None, None)
        }
        TargetKey::Edge(edge) => (Drive::Gain(host.edge(edge)?), None, None),
    };
    Some(Bound {
        drive,
        generation: 0,
        knob,
        seed,
    })
}

/// Queues a resync for every note envelope `now` reads that `was` did not
/// (every one `now` reads, for a new lane).
fn owe_resyncs(
    owed: &mut BTreeSet<SourceKey>,
    compiled: &Compiled,
    was: Option<&ControlProgram>,
    now: &ControlProgram,
) {
    let read = |p: &ControlProgram| -> BTreeSet<SourceId> {
        p.inputs().iter().map(|i| i.source).collect()
    };
    let before = was.map(read).unwrap_or_default();
    for id in read(now).difference(&before) {
        let found = compiled.nodes.iter().find(|(_, n)| n.id == *id);
        if let Some((&key, node)) = found
            && matches!(node.kind, SignalKind::NoteEnvelope { .. })
        {
            owed.insert(key);
        }
    }
}

/// A declared target's bounds as a [`ParamRange`]: `compile` refuses a NaN
/// bound (`ProgramCompileError::NanRange`), so a compiled target has none.
fn declared_range(min: f64, max: f64) -> ParamRange {
    ParamRange::new(min, max)
        .expect("a compiled target has no NaN bound: compile refuses it (NanRange)")
}

/// Stores a range edit: the port's cell for a native lane (a store, no
/// commit), the lane's own for a plugin lane.
fn store_range<F: Float>(
    editor: &mut Editor<F>,
    bound: &Bound,
    controls: &Controls,
    range: ParamRange,
) {
    if let Controls::Foreign(c) = controls {
        c.publish_range(range);
        return;
    }
    match bound.drive {
        Drive::Port(at) => {
            editor
                .set_param_range(at, range)
                .expect("checked: a node, and compile refuses a crossed arc");
        }
        Drive::Gain(id) => {
            editor
                .set_edge_gain_range(id, range)
                .expect("checked: the edge");
        }
        Drive::Plugin { .. } => {
            unreachable!("a plugin target's lane is built with ForeignLaneControls")
        }
    }
}

/// Wires `feed` into source node `key`'s input.
fn wire_feed<F: Float>(editor: &mut Editor<F>, key: NodeKey, feed: Option<Feed>) {
    match feed {
        Some(Feed::Audio(from)) => {
            let width = editor
                .shapes()
                .get(&from)
                .map_or(0, |s| s.audio_out().count());
            let t = &mut editor.spec_mut().topology;
            for port in 0..width {
                t.edges.insert(
                    InputPort::new(key, port),
                    vec![Edge::Direct(PortSource::Node(OutputPort::new(from, port)))],
                );
            }
        }
        Some(Feed::Events(from)) => {
            editor
                .spec_mut()
                .connect_events(EventInputPort::new(key, 0), EventEdge::Direct(from));
        }
        None => {}
    }
}

/// Undoes [`wire_feed`].
fn unwire_feed<F: Float>(
    editor: &mut Editor<F>,
    key: NodeKey,
    feed: Option<Feed>,
    width: ChannelLayout,
) {
    match feed {
        Some(Feed::Audio(from)) => {
            // Only the edges it wired: a host that took the key may have
            // wired the port itself since.
            let t = &mut editor.spec_mut().topology;
            for port in 0..width.count() {
                let at = InputPort::new(key, port);
                let ours = Edge::Direct(PortSource::Node(OutputPort::new(from, port)));
                t.disconnect(at, ours);
            }
        }
        Some(Feed::Events(from)) => {
            editor
                .spec_mut()
                .disconnect_events(EventInputPort::new(key, 0), from);
        }
        None => {}
    }
}

/// A key for a node the reconciler builds: [`NodeKey::fresh`], past any the
/// editor already holds. A host that writes its own keys (a literal, an
/// entity's bits) may have taken one the counter has not reached yet;
/// inserting there would replace the host's node.
fn unused_key<F: Float>(editor: &Editor<F>) -> NodeKey {
    loop {
        let key = NodeKey::fresh();
        if !editor.spec().topology.nodes.contains_key(&key) {
            return key;
        }
    }
}

/// Whether the node the reconciler built at `key` (generation `generation` at its
/// insert) is gone from it: removed, or replaced by a host's insert.
fn taken<F: Float>(editor: &Editor<F>, key: NodeKey, generation: u32) -> bool {
    let spec = editor.spec();
    !spec.topology.nodes.contains_key(&key) || spec.generation(key) != generation
}

/// Removes the direct event edge `from` → `at` this reconciler wired, and
/// no other edge between them (a host's feedback edge from a node it put
/// at a taken key stays). An identical direct edge the host wired itself
/// cannot be told apart, and goes too.
fn remove_event_edge<F: Float>(editor: &mut Editor<F>, at: EventInputPort, from: EventOutputPort) {
    let events = &mut editor.spec_mut().events;
    if let Some(sources) = events.get_mut(&at) {
        sources.retain(|e| *e != EventEdge::Direct(from));
        if sources.is_empty() {
            events.remove(&at);
        }
    }
}
