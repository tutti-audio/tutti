//! Modulation in the session: the reconcile loop of `tutti-nodes`'
//! [`Modulation`](tutti_nodes::Modulation), run by [`Phase::Modulation`] over
//! the session's own declarations
//! ([`Session::mods_mut`](crate::Session::mods_mut)).
//!
//! Per update:
//!
//! 1. **Apply** (`Modulation`): the declarations onto the editor. A program
//!    edit republishes its lanes and stages nothing; a new lane, a signal
//!    node or a moved edge is staged for the commit. A target is bound to the
//!    cells its node declared when it was staged, so a lane resolves in the
//!    update its node is added in.
//! 2. **Commit** (`Commit`): one plan carries the lane and its target. A note
//!    envelope's resync follows the plan it was owed for: a scheduled
//!    command waits for the plan sent before it.
//!
//! **Why is nothing modulating** has one answer, [`ModState::problems`]: a
//! [`Problem`] each for a refusal (which changes nothing in the graph), a
//! target still unresolved and a follower that cannot be fed, each naming
//! the node and the param. A refusal is reported once, when it starts or
//! changes, as [`SessionEvent::Modulation`]; a target that stays unresolved
//! for
//! [`SessionBuilder::unresolved_warning`](crate::SessionBuilder::unresolved_warning)
//! (4 s by default) of [`PhaseCx::now`](crate::PhaseCx::now) is logged and
//! reported once the same way.
//!
//! [`Phase::Modulation`]: crate::Phase::Modulation
//! [`SessionEvent::Modulation`]: crate::SessionEvent::Modulation

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_graph::{EdgeId, EventOutputPort, NodeKey};
use tutti_modulation::{
    Curve, Declarations, Follower, Law, Lfo, Macro, NoteEnvelope, Polarity, RouteDecl, RouteKey,
    Source, SourceKey, TargetDecl, TargetKey,
};
use tutti_nodes::automation::{AutomationRecorder, LaneControls, RecordMode, Trajectory};
use tutti_nodes::{Feed, ReconcileError};
use tutti_rt::Param;
use tutti_units::{Bipolar, ParamAddr, ParamId, UnitAddr, UnitParam};

use crate::ChannelEdge;
use crate::DeclarationError;
use crate::ParamValue;
use tutti_buffers::Float;

use crate::graph::Graph;
use crate::value::{
    Automation, AutomationCell, AutomationField, Bounds, Declared, DepthKey, DepthSlot, Edited,
    Field, MacroField, PublishedSlot, RangeSlot, RateField, ShapeKey, SlotId, ValueTable,
};

/// How long a modulation target may stay unresolved before it is logged:
/// 240 frames at 60 Hz. A target waits for its node to be added, and a
/// plugin's for its values' read; one still waiting after this long is likely
/// never to resolve (a key no node is ever bound to).
pub(crate) const UNRESOLVED_WARNING: Duration = Duration::from_secs(4);

/// One modulatable parameter of a node in the session's graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamRef {
    /// The node with the param.
    pub node: NodeKey,
    /// The param's address on that node.
    pub param: ParamAddr,
}

impl ParamRef {
    /// `param` on `node`.
    pub fn new(node: impl Into<NodeKey>, param: impl Into<ParamAddr>) -> Self {
        let node = node.into();
        Self {
            node,
            param: param.into(),
        }
    }

    /// A native param of `node`, at index 0.
    pub fn unit(node: impl Into<NodeKey>, param: UnitParam) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::Unit(param),
        }
    }

    /// A native param of `node` at `index` among the node's params of that
    /// kind (a multiband compressor's threshold of band `index`).
    /// [`unit`](Self::unit) is index 0.
    pub fn unit_at(node: impl Into<NodeKey>, param: UnitParam, index: u8) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::unit(UnitAddr::new(param, index)),
        }
    }

    /// Parameter `id` of the plugin hosted on `node`.
    pub fn plugin(node: impl Into<NodeKey>, id: ParamId) -> Self {
        Self {
            node: node.into(),
            param: ParamAddr::Id(id),
        }
    }

    /// The reconciler's key: the node's key bits and the param. What a
    /// refusal of the session's own declarations names.
    #[must_use]
    pub fn key(self) -> TargetKey {
        TargetKey::param(self.node.0, self.param)
    }

    /// The `ParamRef` a key of the session's own names; `None` for the key
    /// of an edge ([`ChannelEdge`]) or a mix ([`MixRef`]).
    #[must_use]
    pub fn from_key(target: TargetKey) -> Option<Self> {
        match target {
            TargetKey::Param { node, param } => Some(Self {
                node: NodeKey(node),
                param,
            }),
            TargetKey::Edge(_) | TargetKey::Mix(_) => None,
        }
    }
}

/// The mix of a node in the session's graph: the node's blend with its aligned input, which
/// [`MixTarget`](crate::MixTarget) writes and a modulation route onto it
/// ([`ModTarget`]) drives. Its range is `0..=1`: a route's depth is a fraction of the range
/// declared for it (`Mods::range`), and the value is clamped to the blend.
/// The mix is the lane's knob (one cell), so a route modulates around the mix
/// the graph holds and removing it returns to that mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MixRef {
    /// The node whose mix it is.
    pub node: NodeKey,
}

impl MixRef {
    /// The mix of `node`.
    #[must_use]
    pub fn of(node: impl Into<NodeKey>) -> Self {
        Self { node: node.into() }
    }

    /// The reconciler's key: the node's key bits. What a refusal of the
    /// session's own declarations names.
    #[must_use]
    pub fn key(self) -> TargetKey {
        TargetKey::mix(self.node.0)
    }
}

/// What a modulation route, range or automation names: a param of a node
/// ([`ParamRef`]), the gain of an edge ([`ChannelEdge`]) or the mix of a node
/// ([`MixRef`]). Each converts: `mods.route(source, param_ref, depth)`,
/// `mods.route(source, edge_ref, depth)` and `mods.route(source, mix_ref,
/// depth)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModTarget {
    /// A param of a node.
    Param(ParamRef),
    /// The gain of an edge.
    Edge(ChannelEdge),
    /// The mix of a node.
    Mix(MixRef),
}

impl From<MixRef> for ModTarget {
    fn from(m: MixRef) -> Self {
        Self::Mix(m)
    }
}

impl From<ParamRef> for ModTarget {
    fn from(p: ParamRef) -> Self {
        Self::Param(p)
    }
}

impl<U> From<crate::ParamTarget<U>> for ModTarget {
    fn from(p: crate::ParamTarget<U>) -> Self {
        Self::Param(p.into())
    }
}

impl From<ChannelEdge> for ModTarget {
    fn from(e: ChannelEdge) -> Self {
        Self::Edge(e)
    }
}

impl std::fmt::Display for ModTarget {
    /// The param and its node (`Cutoff of node 3`), the mix of a node, or an
    /// edge's gain.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Param(p) => match p.param {
                ParamAddr::Unit(unit) => write!(f, "{unit:?} of node {}", p.node.0),
                ParamAddr::Indexed(unit, at) => {
                    write!(f, "{unit:?} #{at} of node {}", p.node.0)
                }
                ParamAddr::Id(id) => write!(f, "plugin parameter {id} of node {}", p.node.0),
            },
            Self::Mix(m) => write!(f, "the mix of node {}", m.node.0),
            Self::Edge(e) => write!(f, "the gain of edge {e:?}"),
        }
    }
}

/// A modulation target whose value is in a unit tutti knows: what
/// [`Mods::range`] takes a range of. A [`ParamTarget`](crate::ParamTarget) (the unit its node
/// declares), a [`ChannelEdge`] ([`Gain`](tutti_units::Gain)) and a
/// [`MixRef`] ([`Mix`](tutti_units::Mix)). Sealed.
pub trait Ranged: Into<ModTarget> + ranged::Sealed {
    /// The unit of the target's value.
    type Unit: crate::Scalar;
}

mod ranged {
    pub trait Sealed {}
}

impl<U: crate::Scalar> ranged::Sealed for crate::ParamTarget<U> {}
impl<U: crate::Scalar> Ranged for crate::ParamTarget<U> {
    type Unit = U;
}
impl ranged::Sealed for ChannelEdge {}
impl Ranged for ChannelEdge {
    type Unit = tutti_units::Gain;
}
impl ranged::Sealed for MixRef {}
impl Ranged for MixRef {
    type Unit = tutti_units::Mix;
}

tutti_rt::id! {
    /// A modulation source the session declared, named. Minted by [`Mods`]
    /// (`lfo`, `macro_`, `follower`, ...), never built: a value is one this
    /// session returned, so it names a source that was declared, and a removed
    /// one is [`NoSuchModSource`], not another source. (A host that declares its
    /// own modulation names its sources with
    /// [`tutti_modulation::SourceKey`], its own minting.)
    ///
    /// There is no way to make one:
    ///
    /// ```compile_fail,E0624
    /// let _ = tutti_session::ModSourceId::of(1);
    /// ```
    ///
    /// Nor mint one:
    ///
    /// ```compile_fail,E0624
    /// let _ = tutti_session::ModSourceId::mint(&tutti_rt::Mint::new());
    /// ```
    pub struct ModSourceId;
}

tutti_rt::id! {
    /// A modulation route the session declared, named; as [`ModSourceId`] is for
    /// a source: minted by [`Mods::route`], found again through
    /// [`ModsView::routes`] and [`ModsView::routes_to`].
    ///
    /// There is no way to make one:
    ///
    /// ```compile_fail,E0624
    /// let _ = tutti_session::ModRouteId::of(1);
    /// ```
    ///
    /// Nor mint one:
    ///
    /// ```compile_fail,E0624
    /// let _ = tutti_session::ModRouteId::mint(&tutti_rt::Mint::new());
    /// ```
    pub struct ModRouteId;
}

/// The source `key` names: a key of the session's own declarations is minted
/// from a [`Mint`](tutti_rt::Mint), so it is never 0.
fn source_id(key: SourceKey) -> ModSourceId {
    ModSourceId::new(key.0).expect("a key the session minted is not 0")
}

/// The route `key` names; see [`source_id`].
fn route_id(key: RouteKey) -> ModRouteId {
    ModRouteId::new(key.0).expect("a key the session minted is not 0")
}

/// What [`Mods::remove_source`] and [`Mods::set_source`] found missing: the
/// source the id named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no modulation source {0:?}")]
pub struct NoSuchModSource(pub ModSourceId);

/// Why [`Mods::set_source`] refused a source; nothing changes.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The id names a source this session does not declare.
    #[error(transparent)]
    NoSuchSource(NoSuchModSource),
    /// The source is of another kind than the one declared, whose feed it
    /// would keep or lack.
    #[error("source {id:?} is a {from}; a {to} is another kind: remove it and declare a new one")]
    ChangesKind {
        /// The source.
        id: ModSourceId,
        /// What it is (an LFO, a macro, a follower, a note envelope). The
        /// names are for the message: match on the variant, not the text.
        from: &'static str,
        /// What the call named.
        to: &'static str,
    },
}

/// Whether a source follows a node, so it is declared with one.
const fn kind_name(source: &Source) -> &'static str {
    match source {
        Source::Lfo { .. } => "LFO",
        Source::Macro { .. } => "macro",
        Source::Follower { .. } => "follower",
        Source::NoteEnvelope { .. } => "note envelope",
    }
}

/// Whether two sources are of one kind.
const fn same_kind(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Lfo { .. }, Source::Lfo { .. })
        | (Source::Macro { .. }, Source::Macro { .. })
        | (Source::Follower { .. }, Source::Follower { .. })
        | (Source::NoteEnvelope { .. }, Source::NoteEnvelope { .. }) => true,
        (
            Source::Lfo { .. }
            | Source::Macro { .. }
            | Source::Follower { .. }
            | Source::NoteEnvelope { .. },
            _,
        ) => false,
    }
}

/// What [`Mods::remove_route`] found missing: the route
/// the id named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no modulation route {0:?}")]
pub struct NoSuchModRoute(pub ModRouteId);

/// What [`Mods::record`] refused: `target` has no automation to record over,
/// and the take was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot record on {target}: it has no automation (declare one with `Mods::automate`)")]
pub struct NoAutomation {
    /// The target the take named.
    pub target: ModTarget,
}

/// Why [`Mods::route`] refused a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RouteError {
    /// `target` has no range: a route's depth is a fraction of one, so
    /// declare it with [`Mods::range`] before routing into it.
    #[error("cannot route into {target}: it has no range (declare one with `Mods::range` first)")]
    UndeclaredRange {
        /// The target the route named.
        target: ModTarget,
    },
    /// The route's source was removed (or is another session's).
    #[error(transparent)]
    NoSuchSource(NoSuchModSource),
    /// The source already has a route into `target`: a route is named by its
    /// source and target ([`DepthTarget::of`](crate::DepthTarget::of)), so a
    /// second between the two would have no name.
    #[error("{target} already has a route from this source")]
    Duplicate {
        /// The target the route named.
        target: ModTarget,
    },
}

/// Why [`Mods::set_scale`] refused a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScaleError {
    /// The route to scale was removed (or is another session's).
    #[error(transparent)]
    NoSuchRoute(NoSuchModRoute),
    /// The scaling source was removed (or is another session's).
    #[error(transparent)]
    NoSuchSource(NoSuchModSource),
    /// A depth is scaled by a time function (an LFO or a macro) only, and
    /// only a route from one: the scale's source or the route's is a
    /// follower or a note envelope.
    #[error("the depth of route {route:?} is scaled by a time function only, on both sides")]
    SignalSource {
        /// The route a scale was asked of.
        route: ModRouteId,
    },
}

/// What scales a route's depth: `source`'s value, through `depth`
/// ([`Mods::set_scale`]). Bipolar and linear.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    source: ModSourceId,
    depth: Bipolar,
}

impl Scale {
    /// A scale by `source` at `depth` of it.
    #[must_use]
    pub const fn new(source: ModSourceId, depth: Bipolar) -> Self {
        Self { source, depth }
    }
}

/// Why the session refused modulation. What it names is in the reconciler's
/// keys, which a [`Problem`] puts in terms of nodes and params.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ModRefusal {
    /// The declarations do not apply. The graph keeps what the last apply
    /// that succeeded built.
    #[error("{0}")]
    Apply(ReconcileError),
}

impl ModRefusal {
    /// The route it names: a route whose follower is downstream of its own
    /// target, or one the compile refused.
    pub fn route(&self) -> Option<RouteKey> {
        use tutti_modulation::ProgramCompileError as C;
        match self {
            Self::Apply(ReconcileError::Cycle { route })
            | Self::Apply(ReconcileError::Compile(
                C::DuplicateRoute(route)
                | C::UnknownSource { route }
                | C::UndeclaredTarget { route }
                | C::DepthBySignal { route },
            )) => Some(*route),
            _ => None,
        }
    }

    /// The target it names.
    pub fn target(&self) -> Option<TargetKey> {
        use tutti_modulation::ProgramCompileError as C;
        match self {
            Self::Apply(
                ReconcileError::Unplaced { target: t, .. }
                | ReconcileError::NoSuchParam(t)
                | ReconcileError::NoSuchEventInput(t)
                | ReconcileError::PortTaken(t)
                | ReconcileError::SharedPort { first: t, .. }
                | ReconcileError::Compile(
                    C::DuplicateTarget(t)
                    | C::NanRange(t)
                    | C::CrossedArc(t)
                    | C::LiveCurve(t)
                    | C::LayerIdCollision(t)
                    | C::TooManyLayers { target: t, .. }
                    | C::TooManyInputs { target: t, .. },
                ),
            ) => Some(*t),
            _ => None,
        }
    }

    /// The source it names.
    pub fn source(&self) -> Option<SourceKey> {
        use tutti_modulation::ProgramCompileError as C;
        match self {
            Self::Apply(
                ReconcileError::WrongFeed(s)
                | ReconcileError::Compile(
                    C::DuplicateSource(s) | C::BadSource(s) | C::AnchorDeclared(s),
                ),
            ) => Some(*s),
            _ => None,
        }
    }
}

/// The targets of a set of declarations, as `tutti_modulation` takes them, from
/// ranges and automations: a range and an automation on one target
/// make one [`TargetDecl`]; an automation with no range is unclamped
/// (`±∞`); an automation whose curve is `None` (a recording take is open on
/// it) declares the target with no automation, so its knob plays.
///
/// **A target takes one range and one automation.** A second of either is
/// never merged (which one won would be the host's iteration order): both go
/// in, and the compile refuses the pair as
/// `ProgramCompileError::DuplicateTarget`, the one refusal path.
fn declare_targets(
    ranges: impl IntoIterator<Item = (TargetKey, f64, f64)>,
    automation: impl IntoIterator<Item = (TargetKey, Option<Arc<dyn Curve>>)>,
) -> Vec<(TargetKey, TargetDecl)> {
    let mut targets: BTreeMap<TargetKey, Vec<TargetDecl>> = BTreeMap::new();
    for (key, min, max) in ranges {
        targets.entry(key).or_default().push(TargetDecl {
            min,
            max,
            automation: None,
        });
    }
    let mut automated = BTreeSet::new();
    for (key, curve) in automation {
        let decls = targets.entry(key).or_default();
        if automated.insert(key) {
            match decls.first_mut() {
                Some(decl) => decl.automation = curve,
                None => decls.push(unclamped(curve)),
            }
        } else {
            // A second automation: its own entry, which the compile refuses.
            decls.push(unclamped(curve));
        }
    }
    targets
        .into_iter()
        .flat_map(|(key, decls)| decls.into_iter().map(move |d| (key, d)))
        .collect()
}

fn unclamped(automation: Option<Arc<dyn Curve>>) -> TargetDecl {
    TargetDecl {
        min: f64::NEG_INFINITY,
        max: f64::INFINITY,
        automation,
    }
}

/// A recording take on an automated target (doc 014 §8.2, "Recording").
/// While it is open the target declares no automation, so its knob (the
/// gesture) drives it; when it is finished, its trajectory becomes the target's
/// automation.
///
/// It is finished once [stopped](Self::stop), or, in a mode that stops on
/// release ([`RecordMode::Touch`]), once the recorder is released after a
/// touch. Feed the recorder through [`recorder_mut`](Self::recorder_mut) as
/// the gesture moves.
///
/// A take is typed over the unit `U` of the value it records, which is the unit
/// of the target it is opened on.
#[derive(Debug)]
pub struct AutomationTake<U: crate::Scalar> {
    recorder: AutomationRecorder<U>,
    touched: bool,
    closed: bool,
}

impl<U: crate::Scalar> AutomationTake<U> {
    /// A take recording into `recorder` (a fresh one, or
    /// `AutomationRecorder::overdub` over the lane's trajectory).
    #[must_use]
    pub fn new(recorder: AutomationRecorder<U>) -> Self {
        Self {
            recorder,
            touched: false,
            closed: false,
        }
    }

    /// A fresh `mode` take over `min..=max`.
    pub fn record(mode: RecordMode, min: U, max: U) -> Self {
        Self::new(AutomationRecorder::new(mode, min, max))
    }

    /// The recorder.
    pub fn recorder(&self) -> &AutomationRecorder<U> {
        &self.recorder
    }

    /// The recorder, to touch, record and release.
    pub fn recorder_mut(&mut self) -> &mut AutomationRecorder<U> {
        &mut self.recorder
    }

    /// Stops the take: on the next modulation phase its trajectory becomes the
    /// automation. A touch take stops by itself on release; a write or latch
    /// take stops here.
    pub fn stop(&mut self) {
        self.closed = true;
    }

    /// Whether the take is finished: stopped ([`stop`](Self::stop)), or a touch
    /// take released after a touch. Notes a touch in progress, so call it once
    /// per update.
    pub fn is_finished(&mut self) -> bool {
        if self.recorder.is_taking() {
            self.touched = true;
        }
        self.closed
            || (self.touched
                && !self.recorder.is_taking()
                && self.recorder.mode().stops_on_release())
    }

    /// The finished trajectory: the automation curve it becomes.
    #[must_use]
    pub fn finish(self) -> Trajectory<U> {
        self.recorder.finish()
    }
}

/// An open take with its unit erased: what a lane holds, since a lane's target
/// is named by [`ModTarget`] and not by a unit. [`Mods::take_mut`] names the unit
/// again.
trait OpenTake: std::fmt::Debug + Send {
    fn is_finished(&mut self) -> bool;
    /// Makes the trajectory the curve of `cell`, which automates the take's
    /// unit (`Mods::record` took no other).
    fn finish_into(self: Box<Self>, cell: &dyn AutomationCell);
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl<U: crate::Scalar> OpenTake for AutomationTake<U> {
    fn is_finished(&mut self) -> bool {
        AutomationTake::is_finished(self)
    }
    fn finish_into(self: Box<Self>, cell: &dyn AutomationCell) {
        if let Some(cell) = cell.as_any().downcast_ref::<Automation<U>>() {
            cell.set(Arc::new(AutomationTake::finish(*self)));
        }
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// A modulation target as a [`Problem`] names it: a node's param, the mix of
/// a node or an edge's gain, as the session's declarations
/// ([`Session::mods_mut`](crate::Session::mods_mut)) named it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum TargetName {
    /// A target the session's declarations named.
    Own(ModTarget),
}

impl std::fmt::Display for TargetName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own(target) => target.fmt(f),
        }
    }
}

/// A modulation source as a [`Problem`] names it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum SourceName {
    /// A source the session declared, and the node that feeds it.
    Own {
        /// The source.
        id: ModSourceId,
        /// The node it follows (a follower's input, a note envelope's MIDI).
        fed_by: Option<NodeKey>,
    },
}

impl std::fmt::Display for SourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own {
                fed_by: Some(node), ..
            } => write!(f, "the source fed by node {}", node.0),
            Self::Own { id, fed_by: None } => write!(f, "source {id}"),
        }
    }
}

/// One reason a modulation is not doing what was declared, as
/// [`ModState::problems`] lists it and
/// [`SessionEvent::Modulation`](crate::SessionEvent::Modulation) reports it.
/// Its `Display` names the node and the param (or the source).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Problem {
    /// The declarations are refused and nothing new applied; the graph keeps
    /// what the last apply that succeeded built.
    Refused {
        /// Why.
        cause: ModRefusal,
        /// The target the cause names, or the one of the route it names.
        at: Option<TargetName>,
    },
    /// A target the last apply could not resolve: no node, a node not
    /// committed yet, or a plugin's values not read yet.
    Unresolved {
        /// The target.
        target: TargetName,
        /// How long it has been unresolved, as of the last apply.
        waited: Duration,
    },
    /// A follower the last apply could not feed.
    Unfed {
        /// The source.
        source: SourceName,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { cause, at: None } => write!(f, "modulation refused: {cause}"),
            Self::Refused {
                cause,
                at: Some(at),
            } => write!(f, "modulation refused at {at}: {cause}"),
            Self::Unresolved { target, waited } => write!(
                f,
                "{target} is unresolved after {waited:?}: no node is bound to it, or the \
                 plugin's values were never read"
            ),
            Self::Unfed { source } => write!(f, "{source} cannot be fed: its node is not there"),
        }
    }
}

impl std::error::Error for Problem {}

impl OwnMods {
    /// The target a reconciler key names, as a [`Problem`] reports it.
    fn target_name(&self, key: TargetKey) -> Option<TargetName> {
        self.target_of(key).map(TargetName::Own)
    }

    fn source_name(&self, key: SourceKey) -> SourceName {
        SourceName::Own {
            id: source_id(key),
            fed_by: self.sources.get(&key).and_then(|s| match s.feed {
                Some(OwnFeed::Audio(node) | OwnFeed::Events(node, _)) => Some(node),
                None => None,
            }),
        }
    }

    fn refused(&self, cause: ModRefusal) -> Problem {
        let at = cause
            .target()
            .and_then(|t| self.target_name(t))
            .or_else(|| {
                let route = cause.route()?;
                self.target_name(self.routes.get(&route)?.decl.to)
            });
        Problem::Refused { cause, at }
    }
}

/// The session's modulation as applied: the reconciler, and what its last
/// apply left refused or unresolved. Read it with
/// [`Session::mod_state`](crate::Session::mod_state); [`problems`](Self::problems)
/// is where to ask why nothing modulates.
#[derive(Debug)]
pub struct ModState {
    modulation: tutti_nodes::Modulation,
    /// Why the declarations are refused now: a [`Problem::Refused`].
    refusal: Option<Problem>,
    /// What the last apply left unresolved: [`Problem::Unresolved`] and
    /// [`Problem::Unfed`].
    unresolved: Vec<Problem>,
    /// Since when each unresolved target has been unresolved, and whether it
    /// was reported: the warning's clock.
    unresolved_since: BTreeMap<TargetKey, (Instant, bool)>,
    warn_after: Duration,
    /// An apply has run: from then on every modulation phase applies, so
    /// taking every declaration away tears down what they built.
    applied: bool,
}

impl ModState {
    pub(crate) fn new(warn_after: Duration) -> Self {
        Self {
            modulation: tutti_nodes::Modulation::new(),
            refusal: None,
            unresolved: Vec::new(),
            unresolved_since: BTreeMap::new(),
            warn_after,
            applied: false,
        }
    }

    /// The lanes' retired programs, freed (`Phase::Reclaim`).
    pub(crate) fn reclaim(&self) -> usize {
        self.modulation.reclaim()
    }

    /// Everything the last apply found wrong: the refusal if the declarations
    /// are refused, then each target not yet resolved and each follower not
    /// fed. Empty while the declarations apply and every lane is built.
    pub fn problems(&self) -> impl Iterator<Item = &Problem> {
        self.refusal.iter().chain(&self.unresolved)
    }

    /// `target`'s lane: its node and its controls (the knob, the range, the
    /// counters). `None` while it has none.
    pub fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.modulation.lane(target)
    }

    /// The lanes with a knob that drive `node`'s plugin parameters, by id.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin_lanes(&self, node: NodeKey) -> crate::plugin_params::Knobs {
        self.modulation
            .plugin_lanes(node)
            .map(|(id, lane)| (id, lane.clone()))
            .collect()
    }

    /// Signal source `source`'s node (a follower's, a note envelope's).
    pub fn source_node(&self, source: ModSourceId) -> Option<NodeKey> {
        self.modulation.source(SourceKey(source.get()))
    }

    /// Note-envelope resyncs owed to the next commit.
    pub fn pending_resyncs(&self) -> usize {
        self.modulation.resyncs_owed()
    }

    /// Resyncs refused for good (a full queue), counted.
    pub fn resyncs_dropped(&self) -> u64 {
        self.modulation.resyncs_dropped()
    }

    /// Records an apply's outcome.
    fn applied(
        &mut self,
        applied: Result<tutti_nodes::Applied, ReconcileError>,
        names: &OwnMods,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.applied = true;
        match applied {
            Ok(applied) => {
                self.refusal = None;
                let warn_after = self.warn_after;
                let mut since = std::mem::take(&mut self.unresolved_since);
                let mut unresolved = Vec::new();
                self.unresolved_since = applied
                    .unresolved_targets
                    .iter()
                    .filter_map(|&target| {
                        // A target whose declaration is gone has nothing to name.
                        let name = names.target_name(target)?;
                        let (from, mut warned) = since.remove(&target).unwrap_or((now, false));
                        let waited = now.saturating_duration_since(from);
                        let problem = Problem::Unresolved {
                            target: name,
                            waited,
                        };
                        if !warned && waited >= warn_after {
                            warned = true;
                            tracing::warn!("{problem}");
                            out.push(crate::SessionEvent::Modulation(problem.clone()));
                        }
                        unresolved.push(problem);
                        Some((target, (from, warned)))
                    })
                    .collect();
                unresolved.extend(applied.unresolved_sources.iter().map(|&source| {
                    Problem::Unfed {
                        source: names.source_name(source),
                    }
                }));
                self.unresolved = unresolved;
            }
            Err(error) => {
                self.refuse(names.refused(ModRefusal::Apply(error)), out);
            }
        }
    }

    /// Holds `refusal` (a [`Problem::Refused`]), reporting it when it starts
    /// or changes.
    fn refuse(&mut self, refusal: Problem, out: &mut Vec<crate::SessionEvent>) {
        if self.refusal.as_ref() != Some(&refusal) {
            tracing::error!("{refusal}");
            self.refusal = Some(refusal.clone());
            out.push(crate::SessionEvent::Modulation(refusal));
        }
    }
}

/// What feeds one of the session's own signal sources.
#[derive(Clone, Copy, Debug)]
enum OwnFeed {
    Audio(NodeKey),
    Events(NodeKey, u16),
}

#[derive(Debug)]
struct OwnSource {
    /// The source in force: its rate or macro value is the target of
    /// `ModSourceId::rate` or `value`.
    source: Arc<Declared<Source>>,
    feed: Option<OwnFeed>,
}

/// A route the session declared: the reconciler's declaration, and the target
/// the caller named (the declaration has only its key). Its polarity and
/// curve are the targets' (`ModRouteId::polarity`, `curve`): `decl` holds
/// their values as declared, and [`decl`](Self::decl) the ones in force. Its
/// depth is `decl.depth`, the cell the route's lane reads
/// ([`DepthTarget`](crate::DepthTarget)); its scale, if any, is
/// `decl.scale` ([`Mods::set_scale`]).
#[derive(Debug)]
struct OwnRoute {
    decl: RouteDecl,
    /// What the route modulates.
    target: ModTarget,
    polarity: Arc<Declared<Polarity>>,
    curve: Arc<Declared<Law>>,
    /// The polarity and curve of the scale, the targets of
    /// `PolarityTarget::scale_of` and `CurveTarget::scale_of`; `Some` when
    /// `decl.scale` is.
    scale: Option<OwnScale>,
}

/// The polarity and curve a route's scale is declared with.
#[derive(Debug)]
struct OwnScale {
    polarity: Arc<Declared<Polarity>>,
    curve: Arc<Declared<Law>>,
}

impl OwnRoute {
    /// The reconciler's declaration, with the polarity and curve in force.
    fn decl(&self) -> RouteDecl {
        RouteDecl {
            polarity: self.polarity.get(),
            curve: self.curve.get(),
            scale: self
                .decl
                .scale
                .clone()
                .zip(self.scale.as_ref())
                .map(|(scale, shape)| tutti_modulation::Scale {
                    polarity: shape.polarity.get(),
                    curve: shape.curve.get(),
                    ..scale
                }),
            ..self.decl.clone()
        }
    }
}

struct OwnAutomation {
    /// The curve in force: the target of `ParamTarget::automation`.
    cell: Arc<dyn AutomationCell>,
    take: Option<Box<dyn OpenTake>>,
}

impl std::fmt::Debug for OwnAutomation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnAutomation")
            .field("take", &self.take)
            .finish_non_exhaustive()
    }
}

/// The session's own declarations, made through [`Mods`].
#[derive(Debug)]
pub(crate) struct OwnMods {
    /// Where the targets of the declarations are registered.
    table: Arc<ValueTable>,
    /// Set by a target's landing: the declarations are built again at the
    /// next apply.
    edited: Edited,
    /// Names sources, routes and edges alike.
    minted: tutti_rt::Mint,
    sources: BTreeMap<SourceKey, OwnSource>,
    routes: BTreeMap<RouteKey, OwnRoute>,
    ranges: BTreeMap<TargetKey, Arc<Bounds>>,
    automation: BTreeMap<TargetKey, OwnAutomation>,
    /// The key the reconciler is given for each edge a target names (an
    /// edge has no node bits to be its key): minted on first use, kept for
    /// the edge's life as a target.
    edge_keys: BTreeMap<ChannelEdge, u64>,
    /// The inverse, for [`OwnKeys`].
    edge_refs: BTreeMap<u64, ChannelEdge>,
    /// The declarations as the reconciler takes them, built from the maps
    /// above on the first apply after they may have changed: cleared when
    /// [`Mods`] is handed out (whatever it then edits), when a take closes
    /// and when a node is forgotten.
    built: Option<OwnDeclarations>,
}

/// The session's own declarations as the reconciler takes them, built
/// from [`OwnMods`] and kept until they may change.
#[derive(Debug)]
pub(crate) struct OwnDeclarations {
    sources: Vec<(SourceKey, Source)>,
    routes: Vec<(RouteKey, RouteDecl)>,
    targets: Vec<(TargetKey, TargetDecl)>,
}

impl OwnDeclarations {
    fn borrow(&self) -> Declarations<'_> {
        Declarations {
            sources: &self.sources,
            routes: &self.routes,
            targets: &self.targets,
        }
    }
}

impl OwnMods {
    pub(crate) fn new(table: Arc<ValueTable>) -> Self {
        Self {
            table,
            edited: Arc::new(AtomicBool::new(false)),
            minted: tutti_rt::Mint::new(),
            sources: BTreeMap::new(),
            routes: BTreeMap::new(),
            ranges: BTreeMap::new(),
            automation: BTreeMap::new(),
            edge_keys: BTreeMap::new(),
            edge_refs: BTreeMap::new(),
            built: None,
        }
    }

    /// Registers the targets of route `id`.
    fn provide_route(&self, id: ModRouteId, route: &OwnRoute) {
        self.table.provide_staged(
            SlotId::RoutePolarity(ShapeKey::Route(id)),
            PublishedSlot::new(Field(Arc::clone(&route.polarity))),
        );
        self.table.provide_staged(
            SlotId::RouteCurve(ShapeKey::Route(id)),
            PublishedSlot::new(Field(Arc::clone(&route.curve))),
        );
        self.table.provide(
            Self::depth_slot(route),
            Arc::new(DepthSlot(route.decl.depth.handle())) as crate::value::SlotCell<Bipolar>,
        );
        if let (Some(scale), Some(shape)) = (&route.decl.scale, &route.scale) {
            self.provide_scale(id, scale, shape);
        }
    }

    /// The slot of `route`'s depth: named by its source and its target.
    fn depth_slot(route: &OwnRoute) -> SlotId {
        SlotId::RouteDepth(DepthKey::Into(source_id(route.decl.source), route.target))
    }

    /// The slots of the scale of route `id`: its depth, polarity and curve.
    fn scale_slots(id: ModRouteId) -> [SlotId; 3] {
        [
            SlotId::RouteDepth(DepthKey::Scale(id)),
            SlotId::RoutePolarity(ShapeKey::Scale(id)),
            SlotId::RouteCurve(ShapeKey::Scale(id)),
        ]
    }

    /// Registers the targets of the scale of route `id`.
    fn provide_scale(&self, id: ModRouteId, scale: &tutti_modulation::Scale, shape: &OwnScale) {
        let [depth, polarity, curve] = Self::scale_slots(id);
        self.table.provide(
            depth,
            Arc::new(DepthSlot(scale.depth.handle())) as crate::value::SlotCell<Bipolar>,
        );
        self.table.provide_staged(
            polarity,
            PublishedSlot::new(Field(Arc::clone(&shape.polarity))),
        );
        self.table
            .provide_staged(curve, PublishedSlot::new(Field(Arc::clone(&shape.curve))));
    }

    /// Clears the scale of every route that `source` scales.
    fn clear_scales_by(&mut self, source: SourceKey) {
        let slots: Vec<SlotId> = self
            .routes
            .iter_mut()
            .filter(|(_, r)| r.decl.scale.as_ref().is_some_and(|s| s.source == source))
            .flat_map(|(&key, r)| {
                r.decl.scale = None;
                r.scale = None;
                Self::scale_slots(route_id(key))
            })
            .collect();
        self.table.unprovide(&slots);
    }

    /// Registers the target `source` has: an LFO's rate, a macro's value.
    fn provide_source(&self, id: ModSourceId, source: &Arc<Declared<Source>>) {
        match source.get() {
            Source::Lfo { .. } => self.table.provide_staged(
                SlotId::SourceRate(id),
                PublishedSlot::new(RateField(Arc::clone(source))),
            ),
            Source::Macro { .. } => self.table.provide_staged(
                SlotId::SourceValue(id),
                PublishedSlot::new(MacroField(Arc::clone(source))),
            ),
            Source::Follower { .. } | Source::NoteEnvelope { .. } => {}
        }
    }

    /// Forgets the targets of source `id`, and what was written to them.
    fn retire_source(&self, id: ModSourceId) {
        self.table
            .unprovide(&[SlotId::SourceRate(id), SlotId::SourceValue(id)]);
    }

    /// Forgets the targets of the routes `routes`, and what was written to
    /// them.
    fn retire_routes(&self, routes: impl IntoIterator<Item = (RouteKey, OwnRoute)>) {
        let slots: Vec<SlotId> = routes
            .into_iter()
            .flat_map(|(key, route)| {
                let id = route_id(key);
                let [depth, polarity, curve] = Self::scale_slots(id);
                [
                    SlotId::RoutePolarity(ShapeKey::Route(id)),
                    SlotId::RouteCurve(ShapeKey::Route(id)),
                    Self::depth_slot(&route),
                    depth,
                    polarity,
                    curve,
                ]
            })
            .collect();
        self.table.unprovide(&slots);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.routes.is_empty()
            && self.ranges.is_empty()
            && self.automation.is_empty()
    }

    fn mint(&self) -> u64 {
        self.minted.mint().get()
    }

    /// `target`'s key for the reconciler: a param's is its node's bits and
    /// address, a mix's its node's bits, an edge's a key minted for it.
    fn key_of(&mut self, target: ModTarget) -> TargetKey {
        match target {
            ModTarget::Param(p) => p.key(),
            ModTarget::Mix(m) => m.key(),
            ModTarget::Edge(e) => {
                let key = match self.edge_keys.get(&e) {
                    Some(&key) => key,
                    None => {
                        let key = self.mint();
                        self.edge_keys.insert(e, key);
                        self.edge_refs.insert(key, e);
                        key
                    }
                };
                TargetKey::edge(key)
            }
        }
    }

    /// `target`'s key if it has been given one: a read never mints (an edge
    /// no declaration named has no key, so nothing is declared on it).
    fn peek_key(&self, target: ModTarget) -> Option<TargetKey> {
        match target {
            ModTarget::Param(p) => Some(p.key()),
            ModTarget::Mix(m) => Some(m.key()),
            ModTarget::Edge(e) => self.edge_keys.get(&e).map(|&k| TargetKey::edge(k)),
        }
    }

    /// What a reconciler key of this session's own declarations names.
    fn target_of(&self, key: TargetKey) -> Option<ModTarget> {
        match key {
            TargetKey::Param { node, param } => Some(ModTarget::Param(ParamRef {
                node: NodeKey(node),
                param,
            })),
            TargetKey::Mix(node) => Some(ModTarget::Mix(MixRef {
                node: NodeKey(node),
            })),
            TargetKey::Edge(k) => self.edge_refs.get(&k).map(|&e| ModTarget::Edge(e)),
        }
    }

    /// Closes the takes that are finished: each one's trajectory becomes its
    /// target's automation.
    fn close_takes(&mut self) {
        for automation in self.automation.values_mut() {
            if automation
                .take
                .as_mut()
                .is_some_and(|take| take.is_finished())
                && let Some(take) = automation.take.take()
            {
                take.finish_into(&*automation.cell);
                // A take becomes finished through `Mods` (stopped, or
                // released), which already cleared it; cleared here
                // too so the build never depends on that.
                self.built = None;
            }
        }
    }

    /// The declarations may change: built again on the next apply.
    pub(crate) fn edited(&mut self) {
        self.built = None;
    }

    /// Builds the declarations if they may have changed since the last
    /// build ([`built`](Self::built) then holds them).
    fn build(&mut self) {
        // A target landed since the last build: its field is read again.
        if self.edited.swap(false, Ordering::AcqRel) {
            self.built = None;
        }
        if self.built.is_none() {
            self.built = Some(self.declarations());
        }
    }

    fn declarations(&self) -> OwnDeclarations {
        OwnDeclarations {
            sources: self
                .sources
                .iter()
                .map(|(&k, s)| (k, s.source.get()))
                .collect(),
            routes: self.routes.iter().map(|(&k, r)| (k, r.decl())).collect(),
            targets: declare_targets(
                self.ranges.iter().map(|(&k, bounds)| {
                    let (min, max) = bounds.get();
                    (k, min, max)
                }),
                self.automation.iter().map(|(&k, a)| {
                    // An open take: the knob (the gesture) drives the param.
                    (k, a.take.is_none().then(|| a.cell.curve()))
                }),
            ),
        }
    }

    /// Forgets every declaration on `node`'s params: a removed node takes
    /// its ranges, automation and the routes into it with it.
    pub(crate) fn forget_node(&mut self, node: NodeKey) {
        self.built = None;
        let bits = node.0;
        // The edges that touched the node go with it, and their keys.
        let dead: BTreeSet<u64> = self
            .edge_refs
            .iter()
            .filter(|(_, e)| e.touches(node))
            .map(|(&k, _)| k)
            .collect();
        let gone = |t: &TargetKey| match t {
            TargetKey::Param { node: n, .. } | TargetKey::Mix(n) => *n == bits,
            TargetKey::Edge(k) => dead.contains(k),
        };
        // Named before the edges' keys go: a range's target is found by its key.
        let unranged: Vec<SlotId> = self
            .ranges
            .keys()
            .filter(|t| gone(t))
            .filter_map(|&t| self.target_of(t))
            .map(SlotId::Range)
            .collect();
        self.table.unprovide(&unranged);
        self.edge_refs.retain(|k, _| !dead.contains(k));
        self.edge_keys.retain(|_, k| !dead.contains(k));
        self.ranges.retain(|t, _| !gone(t));
        let unautomated: Vec<SlotId> = self
            .automation
            .keys()
            .filter(|t| gone(t))
            .map(|&t| SlotId::Automation(t))
            .collect();
        self.automation.retain(|t, _| !gone(t));
        self.table.unprovide(&unautomated);
        let lost: Vec<RouteKey> = self
            .routes
            .iter()
            .filter(|(_, r)| gone(&r.decl.to))
            .map(|(&k, _)| k)
            .collect();
        let lost = self.take_routes(lost);
        self.retire_routes(lost);
    }

    /// Removes `keys`, returning what went.
    fn take_routes(
        &mut self,
        keys: impl IntoIterator<Item = RouteKey>,
    ) -> Vec<(RouteKey, OwnRoute)> {
        keys.into_iter()
            .filter_map(|key| Some((key, self.routes.remove(&key)?)))
            .collect()
    }
}

/// The session's own resolver: a target's key is its node's key bits, and a
/// follower's feed is the node it was declared with.
struct OwnKeys<'a>(&'a OwnMods);

impl tutti_nodes::ModHost for OwnKeys<'_> {
    fn node(&self, target: u64) -> Option<NodeKey> {
        // A key no node has any more names one the editor does not hold:
        // the reconciler leaves its target unresolved.
        Some(NodeKey(target))
    }

    fn edge(&self, target: u64) -> Option<EdgeId> {
        crate::Graph::<f32>::edge_id(self.0.edge_refs.get(&target).copied()?)
    }

    fn feed(&self, source: SourceKey) -> Option<Feed> {
        Some(match self.0.sources.get(&source)?.feed? {
            OwnFeed::Audio(node) => Feed::Audio(node),
            OwnFeed::Events(node, port) => Feed::Events(EventOutputPort::new(node, port)),
        })
    }
}

/// The session's modulation: what it applied, and its own declarations.
#[derive(Debug)]
pub(crate) struct ModSession {
    pub(crate) state: ModState,
    pub(crate) own: OwnMods,
}

impl ModSession {
    pub(crate) fn new(warn_after: Duration, table: Arc<ValueTable>) -> Self {
        Self {
            state: ModState::new(warn_after),
            own: OwnMods::new(table),
        }
    }

    /// Applies the declarations. What the apply stages is the update's
    /// commit's.
    fn apply<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.own.build();
        let own = self.own.built.as_ref().expect("built");
        let applied = graph.modulate(
            &mut self.state.modulation,
            &own.borrow(),
            &OwnKeys(&self.own),
        );
        self.state.applied(applied, &self.own, now, out);
        // Every lane this apply built or dropped is in `state` now: the
        // plugins' driven parameters follow it in the same update.
        #[cfg(feature = "plugin")]
        graph.refresh_driven(&self.state);
    }

    /// `Phase::Modulation`: closes the session's own takes that are finished,
    /// then applies, when something is declared or was ever applied (taking
    /// every declaration away tears down what they built).
    pub(crate) fn modulation_phase<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        self.own.close_takes();
        if !self.own.is_empty() || self.state.applied {
            self.apply(graph, now, out);
        }
    }

    /// `Phase::Commit`'s resyncs: sent on an update that owes them and has
    /// nothing left to commit.
    pub(crate) fn resync<F: Float>(&mut self, graph: &mut Graph<F>) {
        if self.state.modulation.resyncs_owed() > 0 && !graph.has_uncommitted_edits() {
            graph.send_mod_resyncs(&mut self.state.modulation);
        }
    }
}

/// A route to declare: what [`Mods::route`] takes. A route is `depth` of
/// `target`'s range, bipolar and linear until [`polarity`](Self::polarity) and
/// [`curve`](Self::curve) say otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RouteSpec {
    source: ModSourceId,
    target: ModTarget,
    depth: Bipolar,
    polarity: Polarity,
    curve: Law,
}

impl RouteSpec {
    /// A route from `source` into `target` at `depth` of its range.
    #[must_use]
    pub fn new(source: ModSourceId, target: impl Into<ModTarget>, depth: Bipolar) -> Self {
        Self {
            source,
            target: target.into(),
            depth,
            polarity: Polarity::Bipolar,
            curve: Law::Linear,
        }
    }

    /// How the source's `-1..1` maps onto the route's span.
    #[must_use]
    pub fn polarity(mut self, polarity: Polarity) -> Self {
        self.polarity = polarity;
        self
    }

    /// The law the source's value is shaped by.
    #[must_use]
    pub fn curve(mut self, curve: Law) -> Self {
        self.curve = curve;
        self
    }
}

/// A route as [`ModsView`] reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct RouteInfo {
    /// The route.
    pub id: ModRouteId,
    /// The source it reads.
    pub source: ModSourceId,
    /// What it drives.
    pub target: ModTarget,
    /// The source that scales its depth ([`Mods::set_scale`]), if any.
    pub scale: Option<ModSourceId>,
    /// Its depth, a fraction of the target's range.
    pub depth: Bipolar,
    /// How the source's value maps onto the span.
    pub polarity: Polarity,
    /// The law that shapes it.
    pub curve: Law,
}

/// A source as [`ModsView`] reads it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SourceInfo {
    /// The source.
    pub id: ModSourceId,
    /// What it is (an LFO, a macro at its value, ...).
    pub source: Source,
    /// The node it follows, for a follower or a note envelope.
    pub fed_by: Option<NodeKey>,
}

/// The session's own modulation declarations, read
/// ([`Session::mods`](crate::Session::mods)): reading marks nothing edited.
#[derive(Clone, Copy, Debug)]
pub struct ModsView<'a> {
    own: &'a OwnMods,
}

impl<'a> ModsView<'a> {
    pub(crate) const fn new(own: &'a OwnMods) -> Self {
        Self { own }
    }
}

impl ModsView<'_> {
    /// Every route, in the order they were declared.
    pub fn routes(&self) -> impl Iterator<Item = RouteInfo> + '_ {
        self.own.routes.iter().map(|(&key, route)| route.info(key))
    }

    /// The routes into `target`.
    pub fn routes_to(&self, target: impl Into<ModTarget>) -> impl Iterator<Item = RouteInfo> + '_ {
        let target = target.into();
        self.routes().filter(move |route| route.target == target)
    }

    /// The route `id`, or `None` when it was removed (or went with its
    /// source or its node).
    #[must_use]
    pub fn route(&self, id: ModRouteId) -> Option<RouteInfo> {
        self.own
            .routes
            .get(&RouteKey(id.get()))
            .map(|r| r.info(RouteKey(id.get())))
    }

    /// Every source, in the order they were declared.
    pub fn sources(&self) -> impl Iterator<Item = SourceInfo> + '_ {
        self.own
            .sources
            .iter()
            .map(|(&key, source)| source.info(key))
    }

    /// The source `id`, or `None` when it was removed.
    #[must_use]
    pub fn source(&self, id: ModSourceId) -> Option<SourceInfo> {
        let key = SourceKey(id.get());
        self.own.sources.get(&key).map(|s| s.info(key))
    }

    /// `target`'s range, if one is declared.
    #[must_use]
    pub fn range_of(&self, target: impl Into<ModTarget>) -> Option<std::ops::RangeInclusive<f64>> {
        let key = self.own.peek_key(target.into())?;
        self.own.ranges.get(&key).map(|bounds| {
            let (min, max) = bounds.get();
            min..=max
        })
    }

    /// Whether `target` has automation declared.
    #[must_use]
    pub fn is_automated(&self, target: impl Into<ModTarget>) -> bool {
        self.own
            .peek_key(target.into())
            .is_some_and(|key| self.own.automation.contains_key(&key))
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.own.is_empty()
    }
}

impl OwnRoute {
    fn info(&self, key: RouteKey) -> RouteInfo {
        RouteInfo {
            id: route_id(key),
            source: source_id(self.decl.source),
            target: self.target,
            scale: self.decl.scale.as_ref().map(|s| source_id(s.source)),
            depth: self.decl.depth.load(),
            polarity: self.polarity.get(),
            curve: self.curve.get(),
        }
    }
}

impl OwnSource {
    fn info(&self, key: SourceKey) -> SourceInfo {
        SourceInfo {
            id: source_id(key),
            source: self.source.get(),
            fed_by: match self.feed {
                Some(OwnFeed::Audio(node) | OwnFeed::Events(node, _)) => Some(node),
                None => None,
            },
        }
    }
}

/// The session's own modulation declarations, to edit (design 015 §4.3,
/// §5.1): sources, routes, ranges, automation and recording takes, named by
/// the [`ModSourceId`]s and [`ModRouteId`]s it mints and by each target's
/// [`ParamRef`]. [`Session::mods_mut`](crate::Session::mods_mut) hands it out;
/// the next update applies what it holds. Reading is [`ModsView`]
/// ([`Mods::view`], or [`Session::mods`](crate::Session::mods)).
#[derive(Debug)]
pub struct Mods<'a> {
    pub(crate) own: &'a mut OwnMods,
}

impl Mods<'_> {
    /// The declarations, read.
    #[must_use]
    pub fn view(&self) -> ModsView<'_> {
        ModsView::new(self.own)
    }

    fn add_source(&mut self, source: Source, feed: Option<OwnFeed>) -> ModSourceId {
        let key = SourceKey(self.own.mint());
        let id = source_id(key);
        let source = Declared::new(source, &self.own.edited);
        self.own.provide_source(id, &source);
        self.own.sources.insert(key, OwnSource { source, feed });
        id
    }

    /// An LFO.
    pub fn lfo(&mut self, lfo: Lfo) -> ModSourceId {
        self.add_source(lfo.into(), None)
    }

    /// A macro at `value`.
    pub fn macro_(&mut self, value: f64) -> ModSourceId {
        self.add_source(Macro(value).into(), None)
    }

    /// A follower of every audio output of `input`.
    pub fn follower(&mut self, follower: Follower, input: impl Into<NodeKey>) -> ModSourceId {
        let input = input.into();
        self.add_source(follower.into(), Some(OwnFeed::Audio(input)))
    }

    /// A note envelope on event output `port` of `midi` (a clip node, a
    /// live input).
    pub fn note_envelope(
        &mut self,
        envelope: NoteEnvelope,
        midi: impl Into<NodeKey>,
        port: u16,
    ) -> ModSourceId {
        let midi = midi.into();
        self.add_source(envelope.into(), Some(OwnFeed::Events(midi, port)))
    }

    /// Replaces `id`'s source, keeping what feeds it, as a declaration: of the
    /// same kind (an LFO of another shape, a macro at another value). The value an LFO's
    /// rate or a macro's position later takes is
    /// [`ModSourceId::rate`] or [`ModSourceId::value`]; one staged and not
    /// yet published is dropped with the source it was for. The lanes it
    /// reaches are republished with no commit.
    ///
    /// # Errors
    ///
    /// [`SourceError::NoSuchSource`] when this session declared no source `id`
    /// (removed already); [`SourceError::ChangesKind`] when `source` is of
    /// another kind than `id`'s. Nothing changes.
    pub fn set_source(
        &mut self,
        id: ModSourceId,
        source: impl Into<Source>,
    ) -> Result<(), SourceError> {
        let Some(s) = self.own.sources.get(&SourceKey(id.get())) else {
            return Err(SourceError::NoSuchSource(NoSuchModSource(id)));
        };
        let source = source.into();
        let from = s.source.get();
        if !same_kind(&from, &source) {
            return Err(SourceError::ChangesKind {
                id,
                from: kind_name(&from),
                to: kind_name(&source),
            });
        }
        self.own.retire_source(id);
        s.source.set(source);
        self.own.provide_source(id, &s.source);
        Ok(())
    }

    /// Removes a source and the routes from it.
    ///
    /// # Errors
    ///
    /// [`NoSuchModSource`] when this session declared no source `id` (removed
    /// already); nothing changes.
    pub fn remove_source(&mut self, id: ModSourceId) -> Result<(), NoSuchModSource> {
        let key = SourceKey(id.get());
        if self.own.sources.remove(&key).is_none() {
            return Err(NoSuchModSource(id));
        }
        self.own.retire_source(id);
        self.own.clear_scales_by(key);
        let lost: Vec<RouteKey> = self
            .own
            .routes
            .iter()
            .filter(|(_, r)| r.decl.source == key)
            .map(|(&k, _)| k)
            .collect();
        let lost = self.own.take_routes(lost);
        self.own.retire_routes(lost);
        Ok(())
    }

    /// Declares `spec`: a route from its source into its target.
    ///
    /// **The target needs its [`range`](Self::range) first**: a route is a
    /// fraction of a range, and the apply refuses a route onto a target with
    /// none (and holds back every later edit with it), so the call refuses
    /// here instead.
    ///
    /// # Errors
    ///
    /// [`RouteError::UndeclaredRange`], naming the target, when no range was
    /// declared for it; [`RouteError::NoSuchSource`] when the source was
    /// removed; [`RouteError::Duplicate`] when the source already has a route
    /// into the target. Nothing is added.
    pub fn route(&mut self, spec: RouteSpec) -> Result<ModRouteId, RouteError> {
        let source = SourceKey(spec.source.get());
        if !self.own.sources.contains_key(&source) {
            return Err(RouteError::NoSuchSource(NoSuchModSource(spec.source)));
        }
        let target = spec.target;
        let to = self.own.key_of(target);
        if !self.own.ranges.contains_key(&to) {
            return Err(RouteError::UndeclaredRange { target });
        }
        if self
            .own
            .routes
            .values()
            .any(|r| r.decl.source == source && r.decl.to == to)
        {
            return Err(RouteError::Duplicate { target });
        }
        let key = RouteKey(self.own.mint());
        let route = OwnRoute {
            decl: RouteDecl {
                source,
                to,
                depth: Param::new(spec.depth),
                polarity: spec.polarity,
                curve: spec.curve,
                scale: None,
            },
            target,
            polarity: Declared::new(spec.polarity, &self.own.edited),
            curve: Declared::new(spec.curve, &self.own.edited),
            scale: None,
        };
        self.own.provide_route(route_id(key), &route);
        self.own.routes.insert(key, route);
        Ok(route_id(key))
    }

    /// Removes a route.
    ///
    /// # Errors
    ///
    /// [`NoSuchModRoute`] when this session declared no route `id` (removed
    /// already, or went with its source); nothing changes.
    pub fn remove_route(&mut self, id: ModRouteId) -> Result<(), NoSuchModRoute> {
        let key = RouteKey(id.get());
        if !self.own.routes.contains_key(&key) {
            return Err(NoSuchModRoute(id));
        }
        let lost = self.own.take_routes([key]);
        self.own.retire_routes(lost);
        Ok(())
    }

    /// Sets, replaces (`Some`) or clears (`None`) what scales the depth of
    /// `route`: a route has at most one scale. The scale's depth is
    /// [`DepthTarget::scale_of`](crate::DepthTarget::scale_of), starting at
    /// the one `scale` gives; its polarity and curve are
    /// [`PolarityTarget::scale_of`](crate::PolarityTarget::scale_of) and
    /// [`CurveTarget::scale_of`](crate::CurveTarget::scale_of), starting
    /// bipolar and linear; a replaced scale's are dropped with it. A scale
    /// goes with its route, and with its source when that is removed.
    ///
    /// # Errors
    ///
    /// [`ScaleError::NoSuchRoute`] or [`ScaleError::NoSuchSource`] when the
    /// route or the scale's source was removed; [`ScaleError::SignalSource`]
    /// when either source is a follower or a note envelope. Nothing changes.
    pub fn set_scale(&mut self, route: ModRouteId, scale: Option<Scale>) -> Result<(), ScaleError> {
        let key = RouteKey(route.get());
        let Some(declared) = self.own.routes.get(&key) else {
            return Err(ScaleError::NoSuchRoute(NoSuchModRoute(route)));
        };
        let new = match scale {
            None => None,
            Some(scale) => {
                let source = SourceKey(scale.source.get());
                let Some(by) = self.own.sources.get(&source) else {
                    return Err(ScaleError::NoSuchSource(NoSuchModSource(scale.source)));
                };
                let signal =
                    |s: &Source| matches!(s, Source::Follower { .. } | Source::NoteEnvelope { .. });
                let scaled = self.own.sources.get(&declared.decl.source);
                if signal(&by.source.get()) || scaled.is_some_and(|s| signal(&s.source.get())) {
                    return Err(ScaleError::SignalSource { route });
                }
                Some(tutti_modulation::Scale {
                    source,
                    depth: Param::new(scale.depth),
                    polarity: Polarity::Bipolar,
                    curve: Law::Linear,
                })
            }
        };
        let shape = new.as_ref().map(|_| OwnScale {
            polarity: Declared::new(Polarity::Bipolar, &self.own.edited),
            curve: Declared::new(Law::Linear, &self.own.edited),
        });
        self.own.table.unprovide(&OwnMods::scale_slots(route));
        if let (Some(scale), Some(shape)) = (&new, &shape) {
            self.own.provide_scale(route, scale, shape);
        }
        if let Some(declared) = self.own.routes.get_mut(&key) {
            declared.decl.scale = new;
            declared.scale = shape;
        }
        Ok(())
    }

    /// Declares `target`'s range, `range.start..=range.end` in its own unit:
    /// what a route's depth is a fraction of, and what the lane clamps its
    /// effective value to (both ends inclusive). This is the declaration: it
    /// gives `target` a range with its initial value, and a route needs one.
    /// The value is the [`RangeTarget`](crate::RangeTarget)'s from then on, so
    /// a target that has a range keeps its value and the call answers
    /// `false`.
    ///
    /// `target` is a [`Key`](crate::Key)'s param method (`lp.cutoff()`, in
    /// [`Hz`](tutti_units::Hz)), a [`ChannelEdge`] (its gain, in
    /// [`Gain`](tutti_units::Gain)) or a [`MixRef`] (in
    /// [`Mix`](tutti_units::Mix)), and the range is a range of that unit:
    /// `m.range(lp.cutoff(), Hz::new(100.0)..Hz::new(4000.0))?`. A plugin's param, which
    /// has no unit tutti knows, takes [`range_raw`](Self::range_raw).
    ///
    /// # Errors
    ///
    /// [`DeclarationError::NotFinite`] for a bound that is NaN or infinite and
    /// [`DeclarationError::Backwards`] when the start is above the end;
    /// nothing is declared.
    pub fn range<T: Ranged>(
        &mut self,
        target: T,
        range: std::ops::Range<T::Unit>,
    ) -> Result<bool, DeclarationError> {
        let (min, max) = (range.start.to_cell(), range.end.to_cell());
        let target: ModTarget = target.into();
        let bounds = match self.declare_range(target, min, max)? {
            Some(bounds) => bounds,
            None => return Ok(false),
        };
        self.own
            .table
            .provide(SlotId::Range(target), RangeSlot::<T::Unit>::cell(bounds));
        Ok(true)
    }

    /// [`range`](Self::range) for a target whose unit is not tutti's: a
    /// plugin's param ([`ParamRef::plugin`]), whose range is in whatever the
    /// plugin's own value is (normalized, usually), or any target given as
    /// bare numbers. It is a declaration only: a plugin's param has no range
    /// target until it has a typed one.
    ///
    /// # Errors
    ///
    /// As [`range`](Self::range).
    pub fn range_raw(
        &mut self,
        target: impl Into<ModTarget>,
        min: f64,
        max: f64,
    ) -> Result<bool, DeclarationError> {
        Ok(self.declare_range(target.into(), min, max)?.is_some())
    }

    /// Checks and records the range of `target`, unless it has one; its
    /// bounds when it was declared now.
    fn declare_range(
        &mut self,
        target: ModTarget,
        min: f64,
        max: f64,
    ) -> Result<Option<Arc<Bounds>>, DeclarationError> {
        if !min.is_finite() || !max.is_finite() {
            return Err(DeclarationError::NotFinite);
        }
        if min > max {
            return Err(DeclarationError::Backwards { min, max });
        }
        let key = self.own.key_of(target);
        if self.own.ranges.contains_key(&key) {
            return Ok(None);
        }
        let bounds = Declared::new((min, max), &self.own.edited);
        self.own.ranges.insert(key, Arc::clone(&bounds));
        Ok(Some(bounds))
    }

    /// Takes `target`'s range away.
    pub fn clear_range(&mut self, target: impl Into<ModTarget>) -> bool {
        let target = target.into();
        let key = self.own.key_of(target);
        self.own.table.unprovide(&[SlotId::Range(target)]);
        self.own.ranges.remove(&key).is_some()
    }

    /// Declares `target`'s automation, a lane that follows `curve` (a
    /// trajectory in the unit of the target's value) where it has a value, in
    /// place of its knob: whether it did. A target that is already automated
    /// keeps its curve and answers `false`: the curve then is the target's
    /// value ([`ParamTarget::automation`](crate::ParamTarget::automation)).
    /// That holds for a lane declared in another unit too.
    #[must_use = "`false` says the lane was already declared and its curve kept"]
    pub fn automate<U: crate::Scalar>(
        &mut self,
        target: impl Into<ModTarget>,
        curve: Arc<Trajectory<U>>,
    ) -> bool {
        let key = self.own.key_of(target.into());
        if self.own.automation.contains_key(&key) {
            return false;
        }
        let cell = Automation::<U>::new(curve, &self.own.edited);
        self.own.table.provide_staged(
            SlotId::Automation(key),
            PublishedSlot::new(AutomationField(Arc::clone(&cell))),
        );
        self.own
            .automation
            .insert(key, OwnAutomation { cell, take: None });
        true
    }

    /// Takes `target`'s automation away (and any take on it).
    pub fn clear_automation(&mut self, target: impl Into<ModTarget>) -> bool {
        let key = self.own.key_of(target.into());
        self.own.table.unprovide(&[SlotId::Automation(key)]);
        self.own.automation.remove(&key).is_some()
    }

    /// Opens a recording take on `target`'s automation: until it is finished
    /// the knob plays, then its trajectory replaces the automation.
    ///
    /// # Errors
    ///
    /// [`NoAutomation`] when `target` has no automation; the take is dropped.
    ///
    /// The take's unit is the target's: a [`Ranged`] target names it, so a take
    /// in another unit does not compile. This API is interim until
    /// the typed automation target and the recording of moves replace it.
    #[must_use = "a refused take is dropped, and the target records nothing"]
    pub fn record<T: Ranged>(
        &mut self,
        target: T,
        take: AutomationTake<T::Unit>,
    ) -> Result<(), NoAutomation> {
        let target = target.into();
        let key = self.own.key_of(target);
        match self.own.automation.get_mut(&key) {
            Some(automation) if automation.cell.as_any().is::<Automation<T::Unit>>() => {
                automation.take = Some(Box::new(take));
                Ok(())
            }
            _ => Err(NoAutomation { target }),
        }
    }

    /// The open take on `target`, to feed its recorder; `None` when no take is
    /// open on it. A take is opened only through [`record`](Self::record), whose
    /// take is in the target's own unit, so the take read back is in that unit.
    pub fn take_mut<T: Ranged>(&mut self, target: T) -> Option<&mut AutomationTake<T::Unit>> {
        let key = self.own.key_of(target.into());
        self.own
            .automation
            .get_mut(&key)
            .and_then(|a| a.take.as_mut())
            .and_then(|take| take.as_any_mut().downcast_mut())
    }

    /// The key the reconciler holds `target` under: what
    /// [`ModState::lane`] and [`ModRefusal::target`] speak. A param's is
    /// [`ParamRef::key`], a mix's [`MixRef::key`]; an edge's is minted on first
    /// use and kept.
    pub fn key(&mut self, target: impl Into<ModTarget>) -> TargetKey {
        self.own.key_of(target.into())
    }
}
