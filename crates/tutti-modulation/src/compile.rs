//! [`compile`]: declarations in, one [`ControlProgram`] per target out.
//!
//! A modulation matrix is what a host *says*: sources, routes from a source
//! to a target param (or to another route's depth), and per target a range
//! and an automation curve. [`compile`] turns that into what the engine
//! *runs*: a program per target for its control lane, a description of the
//! node each signal-driven source needs, and the event edges from those
//! nodes to the lanes. It is pure: no clock, no state, no `Publisher`. Equal
//! declarations (in any order) compile to equal [`Compiled`]s, so a host
//! republishes only the programs that changed.
//!
//! - Stable ids. A layer's [`LayerId`] derives from its route's key and
//!   its depth-by route's key, so a depth edit keeps every id, and the sum
//!   order with it. A signal source keeps its [`SourceId`] across compiles
//!   (`prev`'s); a new one takes the lowest id free.
//! - Rate edits are dated: given the previous compile and the executor's
//!   play reading, a seconds-clocked layer whose rate changed gets a
//!   [`RateAnchor`] [`RATE_ANCHOR_MARGIN`] frames ahead, at the phase the
//!   previous program reaches there, so its phase is continuous.
//! - Refusals are by name ([`ProgramCompileError`]).

use crate::ControlProgramBuilder;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use tutti_curve::Law;
use tutti_music::Adsr;
use tutti_units::{
    Ballistics, Beat, BeatsPerCycle, Bipolar, ParamAddr, ParamId, PhaseIncrement, PlayReading,
    SamplePosition, SampleRate, Samples, Seconds,
};

use tutti_rt::{Param, SplitMix64};

use crate::curve::Curve;
use crate::program::{
    Amount, ControlProgram, DepthBy, InputLayer, Layer, LayerClock, LayerId, ProgramError,
    ProgramTime, RateAnchor, SourceId, Waveform,
};
use crate::shape::Polarity;

/// The largest block a rate edit allows for, in frames: a block cap of 4096
/// frames. A frame count (a buffer size), not a duration.
const LARGEST_BLOCK_FRAMES: usize = 4096;

/// How far ahead of the executor's play reading a rate edit takes effect,
/// in frames: three of the largest block, that is two blocks plus one block
/// of slack for the publish to land, so every lane reads the new program
/// before its anchor and switches on the same frame. A frame count, so its
/// length in time follows the rate.
pub const RATE_ANCHOR_MARGIN: Samples = Samples::new(3 * LARGEST_BLOCK_FRAMES);

/// How far ahead of the play reading a pending anchor must still be for a
/// second rate edit to reuse its frame: two of the largest block, the margin
/// less its publish slack. Nearer than that, a publish
/// that lands late would reach the lanes after the anchor, and they would
/// jump by the difference of the two new rates times the lag; the edit
/// re-anchors at [`RATE_ANCHOR_MARGIN`] instead (see `anchored`).
const ANCHOR_REUSE_MIN: Samples = Samples::new(2 * LARGEST_BLOCK_FRAMES);

/// A source's key: the declarer's identity for it. Only compared. The session
/// names a source with a minted `ModSourceId` and keys it by that number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceKey(pub u64);

/// A route's key; see [`SourceKey`]. A layer's [`LayerId`] derives from it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RouteKey(pub u64);

/// A target: one param of one node, one
/// edge's gain, or one node's mix. A plugin parameter is `ParamAddr::Id`.
///
/// An edge is named by the declarer's own key for it (`Edge`): what it stands
/// for (the sink and the source of a graph edge) is the declarer's to resolve,
/// as a node's key is, and the compile never looks inside it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TargetKey {
    /// A param of a node.
    Param {
        /// The node, in the host's identity.
        node: u64,
        /// The param on it.
        param: ParamAddr,
    },
    /// The gain of an edge, by the host's key for the edge.
    Edge(u64),
    /// The mix of a node, by the host's key for the node: the node's blend with its aligned input,
    /// whose range is `0..=1`.
    Mix(u64),
}

impl TargetKey {
    /// `param` of the node whose key is `node`.
    #[must_use]
    pub const fn param(node: u64, param: ParamAddr) -> Self {
        Self::Param { node, param }
    }

    /// The gain of the edge the host keys `edge`.
    #[must_use]
    pub const fn edge(edge: u64) -> Self {
        Self::Edge(edge)
    }

    /// The mix of the node whose key is `node`.
    #[must_use]
    pub const fn mix(node: u64) -> Self {
        Self::Mix(node)
    }

    /// The node a param or mix target is on; `None` for an edge.
    #[must_use]
    pub const fn node(self) -> Option<u64> {
        match self {
            Self::Param { node, .. } | Self::Mix(node) => Some(node),
            Self::Edge(_) => None,
        }
    }
}

/// A source with no node: a closed-form function of time that feeds a route's layers.
///
/// Exhaustive: [`compile`] handles each kind its own way, so a new kind is a
/// change every consumer has to see. A source that is a node in the graph is a
/// [`SignalKind`].
#[derive(Clone, Debug, PartialEq)]
pub enum TimeSource {
    /// A wave on a clock: a closed-form layer, no node. The clock's anchor
    /// is the compiler's to set; a declared one is refused
    /// ([`ProgramCompileError::AnchorDeclared`]).
    Lfo {
        /// One period.
        wave: Waveform,
        /// The clock it runs on.
        clock: LayerClock,
    },
    /// A constant the user moves, a raw value (`shape` saturates past ±1).
    /// Moving it recompiles what it reaches: a layer of [`Waveform::Const`].
    Macro {
        /// The raw value.
        value: f64,
    },
}

/// What scales a route's depth: a time function whose value, through its own
/// amount, multiplies the route's offset ([`DepthBy`]).
///
/// A route has at most one, so a scale of a scale cannot be written. Both
/// sources must be [`TimeSource`]s declared in [`Declarations::time`]; a
/// [`SourceKey`] that names a signal source is refused
/// ([`ProgramCompileError::DepthBySignal`]).
#[derive(Clone, Debug)]
pub struct Scale {
    /// What scales.
    pub source: SourceKey,
    /// A fraction of the route's scale, read when compiled.
    pub depth: Param<Bipolar>,
    /// Both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: Law,
}

impl PartialEq for Scale {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.depth.ptr_eq(&other.depth)
            && self.polarity == other.polarity
            && self.curve == other.curve
    }
}

/// A route: a source into a target, shaped by an amount.
///
/// Plain data, built with a struct literal: every field is independent, and
/// [`compile`] refuses a combination it cannot run ([`ProgramCompileError`]).
/// Two routes are equal when their depth is the same cell and every other
/// field is equal, so a write to the depth is no change of the declaration.
#[derive(Clone, Debug)]
pub struct RouteDecl {
    /// What modulates.
    pub source: SourceKey,
    /// What it modulates.
    pub to: TargetKey,
    /// A fraction of the target's range. The cell is bound to the route's
    /// layer: the lane reads it at every evaluation, so a write to it
    /// republishes no program. A host keeps the cell across applies: a route
    /// rebuilt with a new cell is a changed route (every lane it feeds is
    /// recompiled and republished).
    pub depth: Param<Bipolar>,
    /// Both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: Law,
    /// What scales the depth, if anything.
    pub scale: Option<Scale>,
}

impl PartialEq for RouteDecl {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.to == other.to
            && self.depth.ptr_eq(&other.depth)
            && self.polarity == other.polarity
            && self.curve == other.curve
            && self.scale == other.scale
    }
}

impl RouteDecl {
    /// The route's amount, with no depth: its cell is bound to the program,
    /// so a write to it changes no program.
    fn amount(&self) -> Amount {
        Amount {
            depth: Bipolar::ZERO,
            polarity: self.polarity,
            curve: self.curve,
        }
    }
}

impl Scale {
    /// The scale's amount, with its depth as the cell holds it when compiled.
    fn amount(&self) -> Amount {
        Amount {
            depth: self.depth.load(),
            polarity: self.polarity,
            curve: self.curve,
        }
    }
}

/// A target's range and automation. The knob is a cell, not a declaration,
/// and so is the range the lane reads: the range here is checked, and the
/// host stores it into the target's range cell.
///
/// Plain data, built with a struct literal; [`compile`] checks it.
#[derive(Clone)]
pub struct TargetDecl {
    /// The range's bottom.
    pub min: f64,
    /// The range's top.
    pub max: f64,
    /// Replaces the knob where it has a value. A curve that is not frozen
    /// (one whose [`Curve::frozen`] returns a copy) is refused: a program
    /// holds the curve itself, compared by identity.
    pub automation: Option<Arc<dyn Curve>>,
}

impl fmt::Debug for TargetDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetDecl")
            .field("min", &self.min)
            .field("max", &self.max)
            .field("automation", &self.automation.as_ref().map(|_| "<curve>"))
            .finish()
    }
}

/// Everything a host declares, as slices of `(key, value)` in any order.
///
/// Plain data, built with a struct literal: four borrowed slices with no
/// invariant of their own beyond one [`SourceKey`] naming one source across `time` and
/// `signals`. The limits ([`MAX_LANE_LAYERS`](crate::MAX_LANE_LAYERS),
/// [`MAX_LANE_INPUTS`](crate::MAX_LANE_INPUTS)) are on what they compile to,
/// and [`compile`] refuses a declaration past them.
#[derive(Clone, Copy, Debug)]
pub struct Declarations<'a> {
    /// The sources that are functions of time.
    pub time: &'a [(SourceKey, TimeSource)],
    /// The sources that are nodes in the graph: each kind holds params already
    /// in their domain.
    pub signals: &'a [(SourceKey, SignalKind)],
    /// The routes.
    pub routes: &'a [(RouteKey, RouteDecl)],
    /// Per target: range and automation.
    pub targets: &'a [(TargetKey, TargetDecl)],
}

/// The node a signal-driven source needs, described: what its constructor
/// takes (`EnvelopeFollowerConfig`'s and `NoteEnvelopeConfig`'s conversions
/// in `tutti-nodes`), and nothing a running node keeps.
///
/// Equality is bitwise, so it is [`Eq`].
#[derive(Clone, Copy, Debug)]
pub struct SignalSource {
    /// The address its events carry (`ParamAddr::Id(id.0)`) on each lane's
    /// event input.
    pub id: SourceId,
    /// Which node, and its params.
    pub kind: SignalKind,
}

/// An ADSR per note: the source half of a note envelope, whose MIDI the host
/// wires.
///
/// The stages are valid by construction: [`new`](Self::new) refuses an ADSR whose
/// times are not finite and non-negative or whose sustain is outside `[0, 1]`
/// (a note envelope's raw value stays in `[0, 1]`), so a [`SignalKind`] never
/// holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteEnvelope {
    adsr: Adsr,
}

impl NoteEnvelope {
    /// An envelope of `adsr`, or `None` when a time is negative, NaN or infinite or the
    /// sustain is outside `[0, 1]`. Never clamps.
    #[must_use]
    pub fn new(adsr: Adsr) -> Option<Self> {
        let time = |s: Seconds| s.get() >= 0.0 && s.get().is_finite();
        let valid = time(adsr.attack)
            && time(adsr.decay)
            && time(adsr.release)
            && (0.0..=1.0).contains(&adsr.sustain.get());
        valid.then_some(Self { adsr })
    }

    /// The stages.
    #[must_use]
    pub const fn adsr(self) -> Adsr {
        self.adsr
    }
}

/// A signal-driven source's node kind and params, each already in its domain.
///
/// Exhaustive: each kind needs a node in `tutti-nodes`, so a new kind is a
/// change that crate has to see.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SignalKind {
    /// An envelope follower: one-pole attack and release on its audio
    /// input's level, sent raw in `[0, 1]` on the play-clock grid.
    Follower(Ballistics),
    /// A note envelope on its MIDI input, sent raw in `[0, 1]`.
    NoteEnvelope(NoteEnvelope),
}

/// Both kinds hold finite values only, so equality is reflexive.
impl Eq for SignalKind {}

impl PartialEq for SignalSource {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.kind == other.kind
    }
}

impl Eq for SignalSource {}

/// What [`compile`] produces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compiled {
    /// One program per target with a route or automation; absent: no lane.
    pub programs: BTreeMap<TargetKey, ControlProgram>,
    /// A node per signal-driven source declared, with its [`SourceId`];
    /// built by the host.
    pub nodes: BTreeMap<SourceKey, SignalSource>,
    /// Event edges, a source's node → a target's lane.
    pub edges: BTreeSet<(SourceKey, TargetKey)>,
    /// Each LFO's dated clock (its rate anchor), routed or
    /// not: every layer and depth scaler it drives uses this one clock, so
    /// one LFO stays phase-coherent across its targets, and the next
    /// compile dates its rate edits from it.
    pub clocks: BTreeMap<SourceKey, LayerClock>,
    /// The id the next new signal source takes: one past every id this
    /// lineage of compiles has given, so no id is ever given twice. It wraps after `2^32`
    /// sources, skipping the ids still held.
    pub next_source: SourceId,
}

/// A clock holds no NaN: `compile` refuses a rate, period or phase that is
/// not finite, and a phase whose magnitude exceeds [`MAX_CLOCK_PHASE`]. An
/// anchor's phase is a finite `LayerClock::cycles` plus phase jumps, each a
/// difference of two phases within that bound, so at most `2 * MAX_CLOCK_PHASE`
/// (2^41): the sum stays finite for fewer than 2^900 edits, never `inf − inf`.
impl Eq for Compiled {}

/// The largest magnitude of a declared clock phase, in cycles (2^40): far past
/// any musical offset, and small enough that a carried anchor's phase jumps
/// (differences of two phases) stay finite.
pub const MAX_CLOCK_PHASE: f64 = 1_099_511_627_776.0;

/// Why [`compile`] refused, by name.
///
/// This is the modulation compiler's error; the graph compiler's is
/// `tutti_graph::CompileError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProgramCompileError {
    /// Two sources with one key.
    #[error("two sources share key {}", .0.0)]
    DuplicateSource(SourceKey),
    /// Two routes with one key.
    #[error("two routes share key {}", .0.0)]
    DuplicateRoute(RouteKey),
    /// Two targets with one key.
    #[error("two targets share key {0:?}")]
    DuplicateTarget(TargetKey),
    /// A route from a source not declared.
    #[error("route {}: its source is not declared", .route.0)]
    UnknownSource {
        /// The route.
        route: RouteKey,
    },
    /// A route to a target not declared: a route needs the target's range.
    #[error("route {}: its target is not declared", .route.0)]
    UndeclaredTarget {
        /// The route.
        route: RouteKey,
    },
    /// A source whose params are out of their domain: a NaN anywhere, a
    /// clock's rate, period or phase or a constant that is not finite, a negative or
    /// infinite time, a sustain outside `[0, 1]`, a drawn wave
    /// whose points do not ascend in `[0, 1)` with finite values.
    #[error("source {}: a param out of its domain", .0.0)]
    BadSource(SourceKey),
    /// A source whose clock phase is finite but past
    /// [`MAX_CLOCK_PHASE`] cycles in magnitude: a phase jump between two such
    /// edits could overflow.
    #[error("source {}: a clock phase past MAX_CLOCK_PHASE cycles", .0.0)]
    PhaseOutOfRange(SourceKey),
    /// An LFO declared with a rate anchor: anchors are the compiler's.
    #[error("source {}: a rate anchor is the compiler's to set", .0.0)]
    AnchorDeclared(SourceKey),
    /// A target range with a NaN bound.
    #[error("target {0:?}: a NaN range bound")]
    NanRange(TargetKey),
    /// A circular target ([`tutti_units::UnitParam::is_circular`], `Azimuth`) whose
    /// `min` is above its `max`: an arc is the bounds in order, across ±180°
    /// as `170..190` (`tutti_graph::ParamRange::arc` writes one), since
    /// ordering `170..-170` would read the opposite arc.
    #[error("target {0:?}: a circular target's range is an arc, min at most max")]
    CrossedArc(TargetKey),
    /// An automation curve that is not frozen (its `Curve::frozen` returns
    /// a copy): a program compares it by identity, and reads it on the
    /// audio thread.
    #[error("target {0:?}: an automation curve not frozen")]
    LiveCurve(TargetKey),
    /// A target with more than [`MAX_LANE_LAYERS`](crate::MAX_LANE_LAYERS)
    /// time-function layers.
    #[error(
        "target {target:?}: {count} layers, past MAX_LANE_LAYERS ({})",
        crate::MAX_LANE_LAYERS
    )]
    TooManyLayers {
        /// The target.
        target: TargetKey,
        /// How many.
        count: usize,
    },
    /// A target with more than [`MAX_LANE_INPUTS`](crate::MAX_LANE_INPUTS)
    /// signal-source routes.
    #[error(
        "target {target:?}: {count} input layers, past MAX_LANE_INPUTS ({})",
        crate::MAX_LANE_INPUTS
    )]
    TooManyInputs {
        /// The target.
        target: TargetKey,
        /// How many.
        count: usize,
    },
    /// A scale from a signal-driven source, or of a route from one: a depth
    /// is scaled by a time function only.
    #[error("route {}: depth_by takes a time function on both sides", .route.0)]
    DepthBySignal {
        /// The scaled route.
        route: RouteKey,
    },
    /// Two layers of one target derived the same [`LayerId`] (a 64-bit hash
    /// collision of their route keys).
    #[error("target {0:?}: two layers share an id")]
    LayerIdCollision(TargetKey),
}

/// The clock a macro's constant layer carries: it reads none, and a beat
/// clock has no corners for a constant.
const MACRO_CLOCK: LayerClock = LayerClock::Beats {
    per_cycle: BeatsPerCycle::of(1.0),
    phase: PhaseIncrement::new(0.0),
};

/// The [`LayerId`] of `route`'s layer, scaled by the source `depth_by` (if any).
/// Stable: keys only, so an edit that keeps both keys keeps the id.
pub fn layer_id(route: RouteKey, depth_by: Option<SourceKey>) -> LayerId {
    let h = SplitMix64::mix(route.0);
    LayerId(match depth_by {
        None => h,
        Some(d) => SplitMix64::mix(h ^ SplitMix64::mix(d.0 ^ 0xD1B5_4A32_D192_ED03)),
    })
}

/// Compiles `decl` to a program per target, a node per signal source and
/// the edges between them.
///
/// `prev` (the last compile) and `now` (the executor's next play reading,
/// `Editor::play_reading`) and `rate` (the graph's sample rate) are read
/// only to keep signal sources' ids and to date rate-edit anchors; the function is pure.
/// With equal inputs it returns equal
/// results, whatever order the declarations come in.
///
/// # Errors
///
/// Each [`ProgramCompileError`], by name.
pub fn compile(
    decl: &Declarations<'_>,
    prev: Option<&Compiled>,
    now: PlayReading,
    rate: SampleRate,
) -> Result<Compiled, ProgramCompileError> {
    let time = index(decl.time, ProgramCompileError::DuplicateSource)?;
    let signals = index(decl.signals, ProgramCompileError::DuplicateSource)?;
    if let Some(&key) = signals.keys().find(|k| time.contains_key(k)) {
        return Err(ProgramCompileError::DuplicateSource(key));
    }
    let routes = index(decl.routes, ProgramCompileError::DuplicateRoute)?;
    let targets = index(decl.targets, ProgramCompileError::DuplicateTarget)?;

    for (&key, source) in &time {
        check_time_source(key, source)?;
    }
    for (&key, route) in &routes {
        if !time.contains_key(&route.source) && !signals.contains_key(&route.source) {
            return Err(ProgramCompileError::UnknownSource { route: key });
        }
    }
    for (&key, t) in &targets {
        if t.min.is_nan() || t.max.is_nan() {
            return Err(ProgramCompileError::NanRange(key));
        }
        if t.min > t.max
            && matches!(key, TargetKey::Param { param, .. }
                if param.as_unit().is_some_and(|a| a.param.is_circular()))
        {
            return Err(ProgramCompileError::CrossedArc(key));
        }
        if t.automation.as_ref().is_some_and(|c| c.frozen().is_some()) {
            return Err(ProgramCompileError::LiveCurve(key));
        }
    }

    // Scales: a time function scales a route of a time function.
    for (&key, route) in &routes {
        let Some(scale) = &route.scale else {
            continue;
        };
        if signals.contains_key(&scale.source) || signals.contains_key(&route.source) {
            return Err(ProgramCompileError::DepthBySignal { route: key });
        }
        if !time.contains_key(&scale.source) {
            return Err(ProgramCompileError::UnknownSource { route: key });
        }
    }

    // Signal sources' nodes: `prev`'s ids kept, a new one the next never
    // given (`prev.next_source` on). An id is never given twice, so a lane
    // can never read a removed source's last segment as a new source's,
    // however many programs it skips: a lane that adopts a
    // program also forgets the inputs it does not read, so a removed
    // source's slot is free at once.
    let mut nodes = BTreeMap::new();
    let kept = |k: &SourceKey| prev.and_then(|p| p.nodes.get(k)).map(|n| n.id);
    for (&key, &&kind) in &signals {
        if let Some(id) = kept(&key) {
            nodes.insert(key, SignalSource { id, kind });
        }
    }
    let mut taken: BTreeSet<SourceId> = nodes.values().map(|n| n.id).collect();
    let mut next = prev.map_or(0, |p| p.next_source.get().get());
    for (&key, &&kind) in &signals {
        if nodes.contains_key(&key) {
            continue;
        }
        // Past `u32::MAX` the ids wrap; one still held is skipped.
        while taken.contains(&next_id(next)) {
            next = next.wrapping_add(1);
        }
        taken.insert(next_id(next));
        nodes.insert(
            key,
            SignalSource {
                id: next_id(next),
                kind,
            },
        );
        next = next.wrapping_add(1);
    }

    // Layers and input layers, per target.
    let mut layers: BTreeMap<TargetKey, (Vec<Layer>, Vec<InputLayer>)> = BTreeMap::new();
    // Each such layer's depth cell: the route's own.
    let mut depths: BTreeMap<TargetKey, Vec<(LayerId, Param<Bipolar>)>> = BTreeMap::new();
    // Each layer's source (and its depth source), to name one a program
    // refuses.
    let mut origin: BTreeMap<LayerId, SourceKey> = BTreeMap::new();
    let mut edges = BTreeSet::new();
    // One dated clock per LFO, from its own clock in `prev`: every layer it
    // drives reads the same anchor, whichever target, route or depth slot.
    let mut clocks = BTreeMap::new();
    for (&key, source) in &time {
        if let TimeSource::Lfo { clock, .. } = source {
            let old = prev.and_then(|p| p.clocks.get(&key));
            clocks.insert(key, anchored(*clock, old, now, rate));
        }
    }
    let dated = |key: SourceKey| {
        let (clock, wave) = match time[&key] {
            TimeSource::Lfo { wave, clock } => (*clock, wave.clone()),
            TimeSource::Macro { value } => (MACRO_CLOCK, Waveform::Const(*value)),
        };
        (clocks.get(&key).copied().unwrap_or(clock), wave)
    };
    for (&key, route) in &routes {
        let target = route.to;
        if !targets.contains_key(&target) {
            return Err(ProgramCompileError::UndeclaredTarget { route: key });
        }
        let id = layer_id(key, route.scale.as_ref().map(|s| s.source));
        let entry = layers.entry(target).or_default();
        origin.insert(id, route.source);
        depths
            .entry(target)
            .or_default()
            .push((id, route.depth.handle()));
        if let Some(node) = nodes.get(&route.source) {
            entry.1.push(InputLayer {
                id,
                source: node.id,
                amount: route.amount(),
            });
            edges.insert((route.source, target));
        } else {
            let (clock, wave) = dated(route.source);
            let depth_by = route.scale.as_ref().map(|by| {
                let (clock, wave) = dated(by.source);
                DepthBy {
                    clock,
                    wave,
                    amount: by.amount(),
                }
            });
            entry.0.push(Layer {
                id,
                clock,
                wave,
                amount: route.amount(),
                depth_by,
            });
        }
    }

    let mut programs = BTreeMap::new();
    for (&target, decl) in &targets {
        let (l, i) = layers.remove(&target).unwrap_or_default();
        if l.is_empty() && i.is_empty() && decl.automation.is_none() {
            continue;
        }
        let mut builder = ControlProgramBuilder::new().layers(l).inputs(i);
        if let Some(automation) = decl.automation.clone() {
            builder = builder.automation(automation);
        }
        let mut program = builder.build().map_err(|e| match e {
            ProgramError::TooManyLayers { count } | ProgramError::TooManyDepths { count } => {
                ProgramCompileError::TooManyLayers { target, count }
            }
            ProgramError::TooManyInputs { count } => {
                ProgramCompileError::TooManyInputs { target, count }
            }
            ProgramError::DuplicateLayer(_) => ProgramCompileError::LayerIdCollision(target),
            // Sources and routes are checked above, so these do not reach
            // here; named by the layer's source if they do.
            ProgramError::BadDrawnWave(id) | ProgramError::NotANumber(id) => origin
                .get(&id)
                .map_or(ProgramCompileError::LayerIdCollision(target), |&s| {
                    ProgramCompileError::BadSource(s)
                }),
        })?;
        let cells = depths.remove(&target).unwrap_or_default();
        let count = cells.len();
        program
            .bind_depths(cells)
            .map_err(|_| ProgramCompileError::TooManyLayers { target, count })?;
        programs.insert(target, program);
    }

    Ok(Compiled {
        programs,
        nodes,
        edges,
        clocks,
        next_source: next_id(next),
    })
}

/// `items` keyed, refusing a duplicate key.
/// The source id numbered `n`: the allocator counts in `u32` and hands out ids.
fn next_id(n: u32) -> SourceId {
    SourceId::new(ParamId::new(n))
}

fn index<K: Ord + Copy, V>(
    items: &[(K, V)],
    dup: impl Fn(K) -> ProgramCompileError,
) -> Result<BTreeMap<K, &V>, ProgramCompileError> {
    let mut out = BTreeMap::new();
    for (k, v) in items {
        if out.insert(*k, v).is_some() {
            return Err(dup(*k));
        }
    }
    Ok(out)
}

/// A time source's params in their domain, or the refusal.
fn check_time_source(key: SourceKey, source: &TimeSource) -> Result<(), ProgramCompileError> {
    let bad = Err(ProgramCompileError::BadSource(key));
    match source {
        TimeSource::Lfo { wave, clock } => {
            let ok_clock = match *clock {
                LayerClock::Beats { phase, .. } => phase.get().is_finite(),
                LayerClock::Seconds {
                    rate,
                    phase,
                    anchor,
                } => {
                    if anchor.is_some() {
                        return Err(ProgramCompileError::AnchorDeclared(key));
                    }
                    rate.get().is_finite() && phase.get().is_finite()
                }
            };
            let phase = match *clock {
                LayerClock::Beats { phase, .. } | LayerClock::Seconds { phase, .. } => phase.get(),
            };
            if ok_clock && phase.abs() > MAX_CLOCK_PHASE {
                return Err(ProgramCompileError::PhaseOutOfRange(key));
            }
            let ok_wave = match wave {
                Waveform::Shape { .. } => true,
                Waveform::Const(v) => v.is_finite(),
                Waveform::Drawn(points) => {
                    points
                        .iter()
                        .all(|p| (0.0..1.0).contains(&p.at.get()) && p.value.is_finite())
                        && points.windows(2).all(|w| w[0].at < w[1].at)
                }
            };
            if ok_clock && ok_wave { Ok(()) } else { bad }
        }
        TimeSource::Macro { value } => {
            if value.is_finite() {
                Ok(())
            } else {
                bad
            }
        }
    }
}

/// An LFO's `clock` (as declared: no anchor) dated against its own dated
/// clock in the previous compile, `old`. Keyed by source,
/// so a new route, a route moved to this source, or a route whose
/// `depth_by` changed reads the anchor every other layer of it reads.
///
/// - A beat clock, or no seconds clock before: as declared.
/// - The same rate: the old anchor carried, if it is in the current epoch
///   (an anchor from another epoch is ignored by the lane, so it is
///   dropped).
/// - A new rate: an anchor at `now + RATE_ANCHOR_MARGIN`, at the old
///   clock's phase there, moving at the old rate before it.
/// - A new rate while the old clock's own anchor is still at least
///   [`ANCHOR_REUSE_MIN`] ahead of `now`: the edit takes effect on that
///   frame instead, which is where the old rate gives way anyway, and the
///   publish has the room to land before it.
///
/// The error that remains. One anchor holds one rate change, so it
/// cannot express R0 → R1 → R2. When a second edit (to R2) comes while an
/// anchor (R0 → R1 at `a`) is pending but nearer than `ANCHOR_REUSE_MIN`,
/// the new anchor goes at `now + margin`, past `a`, with the old clock's
/// exact phase there and `prev_rate` R1; before it the new clock runs at R1
/// back from that phase. That agrees with the old clock from `a` on, but
/// between `now` and `a` the old clock ran at R0, so a lane that switches
/// to the new program at frame `s < a` steps by `(R1 − R0) · (a − s)`
/// (in cycles, with `a − s` in seconds: under `ANCHOR_REUSE_MIN` frames);
/// one that switches at or after `a` does not step. Reusing `a` instead
/// would step by `(R2 − R1) · lag` for any lane the publish reaches after
/// `a`, unbounded by the margin; re-anchoring bounds the step by the
/// frames that were left.
///
/// A changed declared phase is a deliberate jump: it moves a carried or new
/// anchor's phase by the same amount, as it moves an unanchored clock.
fn anchored(
    clock: LayerClock,
    old: Option<&LayerClock>,
    now: PlayReading,
    rate: SampleRate,
) -> LayerClock {
    let LayerClock::Seconds {
        rate: r, phase: p, ..
    } = clock
    else {
        return clock;
    };
    let Some(&LayerClock::Seconds {
        rate: old_rate,
        phase: old_phase,
        anchor: old_anchor,
    }) = old
    else {
        return clock;
    };
    let live = old_anchor.filter(|a| a.epoch == now.epoch);
    let jump = (p - old_phase).get();
    let shifted = |a: RateAnchor| RateAnchor {
        phase: a.phase + jump,
        ..a
    };
    let anchor = if r.get().to_bits() == old_rate.get().to_bits() {
        live.map(shifted)
    } else {
        match live {
            Some(a)
                if a.at
                    .since(now.play)
                    .is_some_and(|left| left >= ANCHOR_REUSE_MIN) =>
            {
                Some(shifted(a))
            }
            _ => {
                let at = now.after(RATE_ANCHOR_MARGIN).play;
                let t = ProgramTime {
                    beat: Beat::new(0.0),
                    play: SamplePosition::new(at.get() as f64),
                    play_epoch: now.epoch,
                    rate,
                };
                old.and_then(|c| c.cycles(&t)).map(|phase| RateAnchor {
                    epoch: now.epoch,
                    at,
                    phase: phase + jump,
                    prev_rate: old_rate,
                })
            }
        }
    };
    LayerClock::Seconds {
        rate: r,
        phase: p,
        anchor,
    }
}

#[cfg(test)]
mod tests;
