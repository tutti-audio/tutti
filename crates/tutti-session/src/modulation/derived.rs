//! The derived nodes of the modulation declarations, and the diff that keeps
//! them: [`Derived`] is the key table (a lane per target, a node per signal
//! source), [`Derived::apply`] resolves the declarations against the editor,
//! [`lower`]s them and writes the difference through the editor's typed calls.
//!
//! Per apply:
//!
//! | what changed | step |
//! |---|---|
//! | absent before | insert, with its wiring |
//! | structure equal, program differs | [`LaneControls::publish`], no commit |
//! | plugin lane, range differs | [`ForeignLaneControls::publish_range`], no commit |
//! | a lane's structure differs | removed, then inserted at a fresh key |
//! | a source's declaration or width differs | removed, then inserted at its key |
//! | gone | removed |
//!
//! A refusal ([`Refusal`]) changes nothing: it is found before the first write.
//!
//! **Resync.** A lane whose program newly reads a note envelope (a new lane,
//! or a route to the envelope added on an existing lane) would read it as 0
//! until the envelope next moves, since a source sends only when it moves and
//! a lane frees an input slot its program stopped reading. A source the apply
//! rebuilds, of either kind, is in the same case: a lane holds the old node's
//! last value until the new one sends. So each is owed an
//! [`EventKind::Resync`], which [`Derived::send_resyncs`] schedules after the
//! commit that placed it: a scheduled command waits for the commit sent before
//! it. A new follower route owes none: a follower sends on every grid point
//! while it is not silent.
//!
//! **When it runs.** [`Derived::stale`]: the declarations were rebuilt, the
//! editor's revision or the play epoch moved since the last apply, or a plugin
//! target waits for its value.

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{
    Edge, EdgeGain, EdgeId, Editor, EventEdge, EventInputPort, EventKind, EventOutputPort,
    ForkByClone, GraphSpec, InputPort, NodeKey, NodeSpec, OutputPort, ParamIn, ParamRange,
    PortSource, ScheduleError,
};
use tutti_modulation::{
    Compiled, ControlProgram, Declarations, SignalKind, SourceId, SourceKey, TargetKey, compile,
};
use tutti_nodes::automation::{
    ControlLaneNode, ForeignLaneConfig, ForeignLaneControls, LaneControls, UnitLaneConfig,
};
use tutti_nodes::{
    EnvelopeFollowerConfig, EnvelopeFollowerNode, NoteEnvelopeConfig, NoteEnvelopeNode,
};
use tutti_rt::ParamCell;
use tutti_units::{At, Normalized, ParamAddr, ParamId, PlayReading};

use super::OwnMods;
use super::lower::{Drive, Feed, LanePlan, Lowered, Refusal, Resolved, SourcePlan, lower};

/// What one apply did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The editor's spec or units changed: a lane or a source was added,
    /// removed or rebuilt, or an edge moved. Commit it. A program, depth,
    /// range or knob edit alone needs none.
    pub(crate) commit: bool,
    /// Targets with a program whose node or edge is not in the editor (never
    /// added, or removed since): no lane until a later apply resolves them,
    /// and a lane they had is removed.
    pub(crate) unresolved_targets: Vec<TargetKey>,
    /// Followers with no feed, or whose feed node is not in the graph: no node
    /// until a later apply.
    pub(crate) unresolved_sources: Vec<SourceKey>,
    /// A plugin target waits for its parameter's value to be read: nothing
    /// the editor holds says when, so the next update applies again.
    pub(crate) awaiting_values: bool,
}

/// The controls of the lane built for a target: a port or edge lane's, or a
/// plugin lane's, which also hold the knob and the range it owns.
#[derive(Debug)]
enum Controls {
    Lane(LaneControls),
    Foreign(ForeignLaneControls),
}

impl Controls {
    fn lane(&self) -> &LaneControls {
        match self {
            Self::Lane(c) => c,
            Self::Foreign(c) => c.lane(),
        }
    }

    /// Frees what the lane's cells hold retired: its programs, and a plugin
    /// lane's ranges.
    fn reclaim(&self) -> usize {
        match self {
            Self::Lane(c) => c.reclaim(),
            Self::Foreign(c) => c.reclaim(),
        }
    }
}

/// A target's lane as built: its node, its structure (what it was resolved
/// to) and its controls. The program is the controls' own.
#[derive(Debug)]
struct Lane {
    key: NodeKey,
    resolved: Resolved,
    controls: Controls,
    /// The range last stored, as bits.
    range: (u64, u64),
}

/// A signal source's node as built.
#[derive(Debug)]
struct Signal {
    key: NodeKey,
    plan: SourcePlan,
}

/// What the last apply saw, which a later one compares against: the editor's
/// revision after its writes, and the epoch of the reading it compiled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    revision: u64,
    epoch: u32,
}

/// The derived nodes of the session's modulation: the key table, and what is
/// owed to it.
#[derive(Debug, Default)]
pub(crate) struct Derived {
    lanes: BTreeMap<TargetKey, Lane>,
    sources: BTreeMap<SourceKey, Signal>,
    /// Event edges as wired: source -> target.
    wired: BTreeSet<(SourceKey, TargetKey)>,
    /// The last compile: sources keep their ids and rate edits are dated.
    prev: Option<Compiled>,
    /// Sources owed a resync at the next commit: a note envelope newly read
    /// by a lane, and a source rebuilt.
    owed: BTreeSet<SourceKey>,
    /// Resyncs the editor refused for good and that were dropped.
    resyncs_dropped: u64,
    seen: Option<Seen>,
}

impl Derived {
    /// `target`'s lane: its node key and its controls. `None` while it has
    /// none.
    pub(crate) fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.lanes.get(&target).map(|l| (l.key, l.controls.lane()))
    }

    /// The lanes with a knob that drive the plugin parameters of `node`, by
    /// parameter id.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin_lanes(
        &self,
        node: NodeKey,
    ) -> impl Iterator<Item = (ParamId, &ForeignLaneControls)> {
        self.lanes
            .values()
            .filter_map(move |lane| match (&lane.resolved.drive, &lane.controls) {
                (Drive::Plugin { input, id }, Controls::Foreign(controls))
                    if input.node() == node =>
                {
                    controls.value().is_some().then_some((*id, controls))
                }
                _ => None,
            })
    }

    /// Signal source `source`'s node, `None` while it has none.
    pub(crate) fn source(&self, source: SourceKey) -> Option<NodeKey> {
        self.sources.get(&source).map(|s| s.key)
    }

    /// Frees, on this thread, the programs (and a plugin lane's ranges) every
    /// lane has let go of since its last publish, without publishing; returns
    /// how many are still held. A lane republished once and then left alone
    /// would otherwise keep a program a block held across that publish until
    /// the lane is published again. Control thread, once per tick.
    pub(crate) fn reclaim(&self) -> usize {
        self.lanes
            .values()
            .map(|lane| lane.controls.reclaim())
            .sum()
    }

    /// Sources owed a resync.
    pub(crate) fn resyncs_owed(&self) -> usize {
        self.owed.len()
    }

    /// Resyncs the editor refused for good, counted.
    pub(crate) fn resyncs_dropped(&self) -> u64 {
        self.resyncs_dropped
    }

    /// Whether an apply would find something to do: the declarations were
    /// rebuilt (`declared`), a plugin target waits for its value
    /// (`awaiting_values`), or the editor's revision or the play epoch is not
    /// what the last apply left (a node or an edge a target waited for, added;
    /// a node replaced, removed or rebound, a range cell, a sample rate, a play
    /// start).
    pub(crate) fn stale(
        &self,
        declared: bool,
        awaiting_values: bool,
        (revision, play): (u64, PlayReading),
    ) -> bool {
        let epoch = play.epoch;
        declared || awaiting_values || self.seen != Some(Seen { revision, epoch })
    }

    /// The revision the last apply left, when it is the one the editor is at:
    /// the commit that follows may move it by its own edits and no others.
    pub(crate) fn seen_revision(&self) -> Option<u64> {
        self.seen.map(|s| s.revision)
    }

    /// Adopts `revision`, the editor's after the commit of an apply's own
    /// edits.
    pub(crate) fn adopt_revision(&mut self, revision: u64) {
        if let Some(seen) = &mut self.seen {
            seen.revision = revision;
        }
    }

    /// Resolves `decl` against `editor`, lowers it and writes the difference
    /// from what is built. A native target's knob is `cell(at)`, the node's
    /// own cell for the param (`None`: no knob, the base is automation
    /// alone). A plugin target's lane starts at `mirror(node, id)`: `None`
    /// while it is not read (the target stays unresolved, since a lane built
    /// without a knob could not take one later), `Some(None)` for a parameter
    /// the node does not list. `now` is the play reading the compile dates
    /// its anchors by, and the one [`stale`](Self::stale) compares the next
    /// update's against: the audio thread moves the editor's own meanwhile.
    /// Control thread; it may allocate.
    ///
    /// # Errors
    ///
    /// The [`Refusal`]; nothing is written.
    pub(crate) fn apply<F: Float>(
        &mut self,
        decl: &Declarations<'_>,
        own: &OwnMods,
        editor: &mut Editor<F>,
        cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
        mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<Normalized>>,
        now: PlayReading,
    ) -> Result<Applied, Refusal> {
        let applied = self.resolve_and_diff(decl, own, editor, cell, mirror, now);
        self.seen = Some(Seen {
            revision: editor.revision(),
            epoch: now.epoch,
        });
        applied
    }

    fn resolve_and_diff<F: Float>(
        &mut self,
        decl: &Declarations<'_>,
        own: &OwnMods,
        editor: &mut Editor<F>,
        cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
        mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<Normalized>>,
        now: PlayReading,
    ) -> Result<Applied, Refusal> {
        let compiled = compile(
            decl,
            self.prev.as_ref(),
            now,
            editor.prepare().sample_rate(),
        )
        .map_err(Refusal::Uncompilable)?;
        let mut applied = Applied::default();
        let mut targets = BTreeMap::new();
        for &target in compiled.programs.keys() {
            let Some(asked) = ask(target, own, cell, mirror, editor) else {
                applied.awaiting_values = true;
                applied.unresolved_targets.push(target);
                continue;
            };
            match resolve_target(target, asked, editor)? {
                Some(resolved) => {
                    targets.insert(target, resolved);
                }
                None => applied.unresolved_targets.push(target),
            }
        }
        let mut sources = BTreeMap::new();
        for (&source, &node) in &compiled.nodes {
            match resolve_source(source, node, own.feed_of(source), editor)? {
                Some(plan) => {
                    sources.insert(source, plan);
                }
                None => applied.unresolved_sources.push(source),
            }
        }
        let lowered = lower(decl, &compiled, targets, sources);
        let keys = self.keys(&lowered, editor);
        check_cycle(&self.lanes, &self.sources, &lowered, &keys, decl, editor)?;
        self.diff(&lowered, &compiled, keys, editor, &mut applied);
        self.prev = Some(compiled);
        Ok(applied)
    }

    /// The node keys of the lowered nodes: a kept node's own, a new one's
    /// unused.
    fn keys<F: Float>(&self, lowered: &Lowered, editor: &Editor<F>) -> Keys {
        let mut taken = BTreeSet::new();
        let mut fresh = || {
            loop {
                let key = NodeKey::fresh();
                if !editor.spec().topology.nodes.contains_key(&key) && taken.insert(key) {
                    return key;
                }
            }
        };
        Keys {
            lanes: lowered
                .lanes
                .iter()
                .map(|(&t, plan)| {
                    let kept = self
                        .lanes
                        .get(&t)
                        .filter(|l| l.resolved.same(&plan.resolved))
                        .map(|l| l.key);
                    (t, kept.unwrap_or_else(&mut fresh))
                })
                .collect(),
            sources: lowered
                .sources
                .keys()
                .map(|&s| (s, self.sources.get(&s).map_or_else(&mut fresh, |n| n.key)))
                .collect(),
        }
    }

    fn diff<F: Float>(
        &mut self,
        lowered: &Lowered,
        compiled: &Compiled,
        keys: Keys,
        editor: &mut Editor<F>,
        applied: &mut Applied,
    ) {
        let commit = &mut applied.commit;
        // Lanes that went, or whose structure changed.
        let gone: Vec<TargetKey> = self
            .lanes
            .iter()
            .filter(|(t, lane)| {
                lowered
                    .lanes
                    .get(t)
                    .is_none_or(|plan| !lane.resolved.same(&plan.resolved))
            })
            .map(|(&t, _)| t)
            .collect();
        for t in gone {
            let lane = self.lanes.remove(&t).expect("listed");
            editor.remove(lane.key);
            *commit = true;
        }
        // Signal nodes that went, or whose declaration or width changed.
        let mut rebuilt = BTreeSet::new();
        let gone: Vec<SourceKey> = self
            .sources
            .iter()
            .filter(|(s, node)| {
                lowered
                    .sources
                    .get(s)
                    .is_none_or(|p| p.decl != node.plan.decl || p.width != node.plan.width)
            })
            .map(|(&s, _)| s)
            .collect();
        for s in gone {
            let node = self.sources.remove(&s).expect("listed");
            editor.remove(node.key);
            if lowered.sources.contains_key(&s) {
                rebuilt.insert(s);
            }
            *commit = true;
        }
        // The edges whose ends survive and are no longer wanted.
        let stale: Vec<(SourceKey, TargetKey)> = self
            .wired
            .iter()
            .copied()
            .filter(|e| !lowered.edges.contains(e))
            .collect();
        for (s, t) in stale {
            if let (Some(src), Some(lane)) = (self.sources.get(&s), self.lanes.get(&t)) {
                editor.disconnect_events(
                    EventInputPort::new(lane.key, 0),
                    EventEdge::Direct(EventOutputPort::new(src.key, 0)),
                );
                *commit = true;
            }
            self.wired.remove(&(s, t));
        }
        // Edges whose source or lane went with a removed node.
        self.wired
            .retain(|(s, t)| self.sources.contains_key(s) && self.lanes.contains_key(t));

        for (&source, plan) in &lowered.sources {
            if let Some(node) = self.sources.get_mut(&source) {
                if node.plan.feed != plan.feed {
                    unwire_feed(editor, node.key, node.plan.feed, node.plan.width);
                    wire_feed(editor, node.key, plan.feed, plan.width);
                    node.plan.feed = plan.feed;
                    *commit = true;
                }
                continue;
            }
            let key = keys.sources[&source];
            // A rebuilt source is owed a resync: a lane holds the old node's
            // last value until the new one sends.
            if rebuilt.contains(&source) {
                self.owed.insert(source);
            }
            insert_source(editor, key, plan);
            wire_feed(editor, key, plan.feed, plan.width);
            self.sources.insert(source, Signal { key, plan: *plan });
            *commit = true;
        }

        for (&target, plan) in &lowered.lanes {
            let program = &plan.program;
            let range = range_bits(plan.range);
            if let Some(lane) = self.lanes.get_mut(&target) {
                // Sources the new program reads that the published one did
                // not: the lane frees their slots on adoption, so a note
                // envelope among them owes a resync.
                let published = lane.controls.lane().program();
                owe_resyncs(&mut self.owed, compiled, Some(&published), program);
                if *published != *program {
                    lane.controls.lane().publish(program.clone());
                }
                if lane.range != range {
                    store_range(editor, &lane.resolved.drive, &lane.controls, plan.range);
                    lane.range = range;
                }
                continue;
            }
            owe_resyncs(&mut self.owed, compiled, None, program);
            let key = keys.lanes[&target];
            self.lanes.insert(target, build_lane(editor, key, plan));
            *commit = true;
        }

        for &(s, t) in &lowered.edges {
            if self.wired.insert((s, t)) {
                let (src, lane) = (self.sources[&s].key, self.lanes[&t].key);
                editor
                    .connect_events(
                        EventInputPort::new(lane, 0),
                        EventEdge::Direct(EventOutputPort::new(src, 0)),
                    )
                    .expect("checked: the lane's input, and the source's output");
                *commit = true;
            }
        }
        // A resync owed to a source with no node (unresolved, or gone) is
        // owed to nothing.
        let sources = &self.sources;
        self.owed.retain(|s| sources.contains_key(s));
    }

    /// Schedules the owed resyncs ([`EventKind::Resync`] at
    /// [`At::NextBlock`] into each owed source's event input). Call it after the
    /// commit an [`apply`](Self::apply) asked for has been sent: a scheduled
    /// command waits for that commit to land.
    ///
    /// # Errors
    ///
    /// The first [`ScheduleError`]. A resync refused for now
    /// ([`ScheduleError::Backpressure`], or [`ScheduleError::NoPlan`] before
    /// any commit) stays owed for the next call; one refused for good (no such
    /// port, a resolution too coarse, a poisoned executor) is dropped and
    /// counted ([`resyncs_dropped`](Self::resyncs_dropped)).
    pub(crate) fn send_resyncs<F: Float>(
        &mut self,
        editor: &mut Editor<F>,
    ) -> Result<(), ScheduleError> {
        let owed = std::mem::take(&mut self.owed);
        let mut first = Ok(());
        for s in owed {
            let Some(node) = self.sources.get(&s) else {
                continue;
            };
            let to = EventInputPort::new(node.key, 0);
            if let Err(e) = editor.schedule(At::NextBlock, to, EventKind::Resync) {
                if e.is_transient() {
                    self.owed.insert(s);
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
}

/// The node keys of what was lowered.
struct Keys {
    lanes: BTreeMap<TargetKey, NodeKey>,
    sources: BTreeMap<SourceKey, NodeKey>,
}

/// What the keys of `target` name, and the cells its lane starts from: a
/// native param's own cell (`cell`), a plugin parameter's first value
/// (`mirror`, `None` while it is not read, `Some(None)` for one the node does
/// not list). The outer `None`: the plugin's value is not read yet, and a lane
/// built without a knob could not take one later; the inner, the key names
/// nothing.
fn ask<F: Float>(
    target: TargetKey,
    own: &OwnMods,
    cell: &dyn Fn(ParamIn) -> Option<ParamCell>,
    mirror: &dyn Fn(NodeKey, ParamId) -> Option<Option<Normalized>>,
    editor: &Editor<F>,
) -> Option<Option<Resolved>> {
    let (drive, knob, seed) = match target {
        TargetKey::Param { node, param } => {
            let node = NodeKey(node);
            match param.as_unit() {
                Some(param) => {
                    let at = ParamIn { node, param };
                    (Drive::Port(at), cell(at), None)
                }
                None => {
                    let ParamAddr::Id(id) = param else {
                        return Some(None);
                    };
                    let input = EventInputPort::new(node, 0);
                    (Drive::Plugin { input, id }, None, mirror(node, id)?)
                }
            }
        }
        TargetKey::Mix(node) => (Drive::Mix(NodeKey(node)), None, None),
        TargetKey::Edge(edge) => {
            let Some(id) = own.edge_of(edge, editor) else {
                return Some(None);
            };
            (Drive::Gain(id), None, None)
        }
    };
    Some(Some(Resolved {
        drive,
        generation: 0,
        knob,
        seed,
    }))
}

/// `target`, as asked of the declarations (`asked`), checked against `editor`.
fn resolve_target<F: Float>(
    target: TargetKey,
    asked: Option<Resolved>,
    editor: &Editor<F>,
) -> Result<Option<Resolved>, Refusal> {
    let Some(mut resolved) = asked else {
        return Ok(None);
    };
    match resolved.drive {
        Drive::Port(at) => {
            let Some(shape) = editor.shapes().get(&at.node) else {
                return Ok(None);
            };
            if shape.params.index_of(at.param).is_none() {
                return Err(Refusal::NoSuchParam(target));
            }
            resolved.generation = editor.spec().generation(at.node);
        }
        Drive::Mix(node) => {
            // A node that is not in the graph (yet): no lane.
            if !editor.spec().topology.nodes.contains_key(&node) {
                return Ok(None);
            }
            editor
                .check_pass(node)
                .map_err(|error| Refusal::Unplaced { target, error })?;
            resolved.generation = editor.spec().generation(node);
            resolved.knob = Some(
                editor
                    .mix_cell(node)
                    .expect("a node in the spec has its mix cell from its staging"),
            );
        }
        Drive::Gain(id) => {
            // An edge that is not in the graph (yet): no lane.
            if !editor.spec().has_edge(id) {
                return Ok(None);
            }
            // An edge with no entry has no cells: building the lane declares
            // the entry, and records the cell it bound.
            resolved.knob = editor.edge_gain_cell(id);
        }
        Drive::Plugin { input, .. } => {
            let Some(shape) = editor.shapes().get(&input.node()) else {
                return Ok(None);
            };
            if input.port() >= shape.event_in {
                return Err(Refusal::NoSuchEventInput(target));
            }
            resolved.generation = editor.spec().generation(input.node());
        }
    }
    Ok(Some(resolved))
}

/// `source`'s node, fed by what the declarations name (`feed`), checked
/// against `editor`: its feed and a follower's width. `None` while a follower
/// has no feed.
fn resolve_source<F: Float>(
    source: SourceKey,
    decl: tutti_modulation::SignalSource,
    feed: Option<Feed>,
    editor: &Editor<F>,
) -> Result<Option<SourcePlan>, Refusal> {
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
                return Err(Refusal::WrongFeed(source));
            }
            ChannelLayout::EMPTY
        }
        _ => return Err(Refusal::WrongFeed(source)),
    };
    Ok(Some(SourcePlan { decl, feed, width }))
}

/// Refuses, by route, an edge from a signal source whose input is downstream
/// of its target: the editor's spec as it stands without the derived nodes,
/// plus what `lowered` would wire, is searched for a cycle
/// ([`GraphSpec::cycle`]) before anything is written. Only a cycle through a
/// derived edge is the route's; any other the commit reports.
fn check_cycle<F: Float>(
    lanes: &BTreeMap<TargetKey, Lane>,
    sources: &BTreeMap<SourceKey, Signal>,
    lowered: &Lowered,
    keys: &Keys,
    decl: &Declarations<'_>,
    editor: &Editor<F>,
) -> Result<(), Refusal> {
    if lowered.edges.is_empty() {
        return Ok(());
    }
    let mut spec = editor.spec().clone();
    strip(
        &mut spec,
        &lanes
            .values()
            .map(|l| l.key)
            .chain(sources.values().map(|s| s.key))
            .collect(),
    );
    let node = || NodeSpec::new("derived", ChannelLayout::EMPTY, ChannelLayout::EMPTY);
    for (source, plan) in &lowered.sources {
        let key = keys.sources[source];
        spec.topology.nodes.insert(key, node());
        match plan.feed {
            Some(Feed::Audio(from)) => {
                for port in 0..plan.width.count() {
                    spec.connect(
                        InputPort::new(key, port),
                        Edge::Direct(PortSource::Node(OutputPort::new(from, port))),
                    );
                }
            }
            Some(Feed::Events(from)) => {
                spec.connect_events(EventInputPort::new(key, 0), EventEdge::Direct(from));
            }
            None => {}
        }
    }
    for (target, plan) in &lowered.lanes {
        let key = keys.lanes[target];
        let out = EventOutputPort::new(key, 0);
        spec.topology.nodes.insert(key, node());
        match plan.resolved.drive {
            Drive::Port(_) | Drive::Mix(_) => {
                if let Some(at) = plan.resolved.drive.port() {
                    spec.connect_param(at, out);
                }
            }
            Drive::Gain(id) => {
                spec.connect_edge_gain(id, out);
            }
            Drive::Plugin { input, .. } => {
                spec.connect_events(input, EventEdge::Direct(out));
            }
        }
    }
    for &(s, t) in &lowered.edges {
        spec.connect_events(
            EventInputPort::new(keys.lanes[&t], 0),
            EventEdge::Direct(EventOutputPort::new(keys.sources[&s], 0)),
        );
    }
    let Some(cycle) = spec.cycle() else {
        return Ok(());
    };
    let closes = |s: SourceKey, t: TargetKey| {
        cycle.contains(&tutti_graph::CycleEdge::Event {
            at: EventInputPort::new(keys.lanes[&t], 0),
            from: EventOutputPort::new(keys.sources[&s], 0),
        })
    };
    let mut routes: Vec<_> = decl.routes.iter().collect();
    routes.sort_by_key(|(k, _)| *k);
    for (key, route) in routes {
        let t = route.to;
        if lowered.edges.contains(&(route.source, t)) && closes(route.source, t) {
            return Err(Refusal::Cycle { route: *key });
        }
    }
    Ok(())
}

/// `spec` without the nodes `gone` and every edge they ends.
fn strip(spec: &mut GraphSpec, gone: &BTreeSet<NodeKey>) {
    for key in gone {
        spec.topology.nodes.remove(key);
    }
    spec.topology.edges.retain(|at, edges| {
        edges.retain(
            |e| !matches!(e, Edge::Direct(PortSource::Node(p)) if gone.contains(&p.node())),
        );
        !gone.contains(&at.node())
    });
    spec.events.retain(|at, edges| {
        edges.retain(|e| !gone.contains(&e.from().node()));
        !gone.contains(&at.node())
    });
    spec.params
        .retain(|at, from| !gone.contains(&at.node) && !gone.contains(&from.node()));
    for gain in spec.gains.values_mut() {
        if gain.source.is_some_and(|from| gone.contains(&from.node())) {
            gain.source = None;
        }
    }
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

/// A range as a lane records it.
fn range_bits(range: ParamRange) -> (u64, u64) {
    (range.min().to_bits(), range.max().to_bits())
}

/// Stores a range edit: the port's cell for a port lane or an edge's gain (a
/// store, no commit), the lane's own for a plugin lane.
fn store_range<F: Float>(
    editor: &mut Editor<F>,
    drive: &Drive,
    controls: &Controls,
    range: ParamRange,
) {
    match (drive, controls) {
        (Drive::Gain(id), _) => {
            editor
                .set_edge_gain_range(*id, range)
                .expect("checked: the edge");
        }
        (Drive::Plugin { .. }, Controls::Foreign(controls)) => controls.publish_range(range),
        (Drive::Port(_) | Drive::Mix(_) | Drive::Plugin { .. }, _) => {
            if let Some(at) = drive.port() {
                editor
                    .set_param_range(at, range)
                    .expect("checked: a node, and compile refuses a crossed arc");
            }
        }
    }
}

/// Inserts the lane of `plan` at `key`, wired into its drive, with the cells
/// it bound: an edge's gain entry is declared here, so its cell is not the
/// plan's.
fn build_lane<F: Float>(editor: &mut Editor<F>, key: NodeKey, plan: &LanePlan) -> Lane {
    let program = plan.program.clone();
    let out = EventOutputPort::new(key, 0);
    let mut resolved = plan.resolved.clone();
    let controls = match resolved.drive {
        Drive::Plugin { input, id } => {
            let mut config = ForeignLaneConfig::new(id, program, plan.range);
            if let Some(seed) = resolved.seed {
                config = config.knob(seed);
            }
            let (node, controls) = config.build();
            editor.insert(key, "control-lane", node);
            editor
                .connect_events(input, EventEdge::Direct(out))
                .expect("checked: the plugin's event input, and the lane's output");
            Controls::Foreign(controls)
        }
        Drive::Gain(id) => {
            // An edge with no entry gets one, and the lane binds to its cells.
            editor
                .set_edge_gain_range(id, plan.range)
                .expect("checked: the edge");
            let (knob, range) = editor
                .edge_gain_cell(id)
                .zip(editor.edge_gain_range_cell(id))
                .expect("declared above");
            resolved.knob = Some(knob.clone());
            let node = ControlLaneNode::from(
                UnitLaneConfig::new(EdgeGain::ADDR, program)
                    .knob(knob)
                    .range(range),
            );
            let controls = editor.insert(key, "control-lane", node).controls;
            editor
                .connect_edge_gain(id, out)
                .expect("checked: the edge, and the lane's output");
            Controls::Lane(controls)
        }
        Drive::Port(_) | Drive::Mix(_) => {
            let at = resolved
                .drive
                .port()
                .expect("a port or mix drive has its port");
            editor
                .set_param_range(at, plan.range)
                .expect("checked: a node, and compile refuses a crossed arc");
            let cell = editor.param_range_cell(at);
            let mut config = UnitLaneConfig::new(at.param, program)
                .range(cell.expect("a bound param has its range cell from its staging"));
            if let Some(knob) = resolved.knob.clone() {
                config = config.knob(knob);
            }
            let controls = editor
                .insert(key, "control-lane", ControlLaneNode::from(config))
                .controls;
            editor
                .connect_param(at, out)
                .expect("checked: the port's node, and the lane's output");
            Controls::Lane(controls)
        }
    };
    Lane {
        key,
        resolved,
        controls,
        range: range_bits(plan.range),
    }
}

/// Inserts the node of `plan` at `key`.
fn insert_source<F: Float>(editor: &mut Editor<F>, key: NodeKey, plan: &SourcePlan) {
    match plan.decl.kind {
        SignalKind::Follower { .. } => {
            let node = EnvelopeFollowerNode::from(
                EnvelopeFollowerConfig::from_decl(plan.width, &plan.decl)
                    .expect("a follower's declaration"),
            );
            editor.insert(key, "envelope-follower", ForkByClone(node));
        }
        SignalKind::NoteEnvelope { .. } => {
            let node = NoteEnvelopeNode::from(
                NoteEnvelopeConfig::from_decl(&plan.decl).expect("a note envelope's"),
            );
            editor.insert(key, "note-envelope", ForkByClone(node));
        }
    }
}

/// Wires `feed` into source node `key`'s input: a follower's `width`
/// channels, or an envelope's MIDI.
fn wire_feed<F: Float>(
    editor: &mut Editor<F>,
    key: NodeKey,
    feed: Option<Feed>,
    width: ChannelLayout,
) {
    match feed {
        Some(Feed::Audio(from)) => {
            for port in 0..width.count() {
                editor
                    .connect_edge(EdgeId::into_port(
                        InputPort::new(key, port),
                        Edge::Direct(PortSource::Node(OutputPort::new(from, port))),
                    ))
                    .expect("checked: the feed's node, and the follower's inputs");
            }
        }
        Some(Feed::Events(from)) => {
            editor
                .connect_events(EventInputPort::new(key, 0), EventEdge::Direct(from))
                .expect("checked: the feed's event output, and the envelope's input");
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
            for port in 0..width.count() {
                editor.disconnect_edge(EdgeId::into_port(
                    InputPort::new(key, port),
                    Edge::Direct(PortSource::Node(OutputPort::new(from, port))),
                ));
            }
        }
        Some(Feed::Events(from)) => {
            editor.disconnect_events(EventInputPort::new(key, 0), EventEdge::Direct(from));
        }
        None => {}
    }
}

#[cfg(test)]
mod tests;
