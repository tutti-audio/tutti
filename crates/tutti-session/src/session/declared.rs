//! What [`Session::patch`] does with the declarations of [`crate::patch`]:
//! the session keeps the last patch's declarations by app key, and a patch is
//! diffed against them, never against the graph. A node `Graph::add` made is
//! not in them: a patch neither keeps nor removes it.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use thiserror::Error;
use tutti_buffers::Float;
use tutti_graph::{
    BusOutputPort, CycleEdge, Edge, EdgeId, GraphSpec, HostValues, Initials, IntoNode, NodeConfig,
    NodeKey, NodeSpec, PortSink, PortSource, Shape, Shapes,
};
use tutti_units::{Gain, Samples};

use crate::graph::Split;
use crate::key::BusEnd;
use crate::patch::{
    Built, Declaration, Declarations, Deps, Fading, IntoPatch, KeyError, PatchTarget,
    PendingSection, Place, WireDecl, WireEnds, WireId,
};
use crate::value::{declared_param, declared_published, declared_setting};
use crate::{
    AnyAssign, AppKey, BusInputPort, Clock, ConnectError, Graph, Key, ReplaceRefused, Session,
};

mod wire;
use wire::{KeptWire, Ports, Resolved};

/// The session's graph is where declarations go: a config is built at the
/// graph's float.
impl<F: Float> PatchTarget for Graph<F> {
    type Refused = ReplaceRefused;
}

impl<F: Float, C> Place<C> for Graph<F>
where
    C: NodeConfig<F>,
    C::Node: IntoNode<F, Controls: Send + 'static>,
{
    fn node_type() -> TypeId {
        TypeId::of::<C::Node>()
    }

    fn initials(config: &C) -> Initials {
        config.initials()
    }

    fn same_structure(a: &C, b: &C) -> bool {
        a.same_structure(b)
    }

    fn build(config: &C, host: &HostValues) -> Built {
        let split = Split::<C::Node, F>::of(config.build(host));
        let shape: Shape = split.shape();
        Built {
            node: Box::new(split),
            shape,
        }
    }

    fn add(&mut self, id: NodeKey, built: Built) {
        self.add_split(id, split_of::<C, F>(built));
    }

    fn swap(
        &mut self,
        key: NodeKey,
        built: Built,
        fade: Option<tutti_graph::Fade>,
    ) -> Result<(), ReplaceRefused> {
        self.replace_split(key, split_of::<C, F>(built), fade)
            .map(drop)
    }
}

/// The node `C` built, out of its erased form.
fn split_of<C, F: Float>(built: Built) -> Split<C::Node, F>
where
    C: NodeConfig<F>,
    C::Node: IntoNode<F, Controls: Send + 'static>,
{
    *built
        .node
        .downcast::<Split<C::Node, F>>()
        .expect("a node is placed by the config that built it")
}

/// Why [`Session::patch`](crate::Session::patch) took nothing. Nothing is
/// changed.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PatchError<K> {
    /// The key is declared twice in one patch.
    #[error("{0:?} is declared twice in one patch")]
    DuplicateKey(K),
    /// The key names two sections in one patch.
    #[error("{0:?} names two sections in one patch")]
    DuplicateSection(K),
    /// The key's config gives an initial value that is NaN or infinite.
    #[error("{0:?} declares an initial value that is not finite")]
    NotFinite(K),
    /// A wire names a key that no part of the patch declares.
    #[error("{0:?} is mentioned by a wire but not declared in the patch")]
    Undeclared(K),
    /// A wire closes a cycle the graph would not compile, with the edges the
    /// graph holds and the wires declared: audio, event and bus. A feedback
    /// edge is not one.
    #[error("the wire {ends:?} closes a cycle of direct edges")]
    Cycle {
        /// The wire.
        ends: WireEnds<K>,
    },
    /// A feedback wire whose delay is shorter than the graph's largest block,
    /// which the graph would not compile.
    #[error("the wire {ends:?} is delayed {delay} frames, less than the block of {max_block}")]
    FeedbackTooShort {
        /// The wire.
        ends: WireEnds<K>,
        /// Its delay.
        delay: Samples,
        /// The graph's largest block.
        max_block: Samples,
    },
    /// A wire puts a second edge on ports that hold one: a direct edge and a
    /// feedback edge, or two delays, from the patch's wires or the edges the
    /// graph already holds. A pair of ports holds one edge.
    #[error("the wire {ends:?} puts a second edge on ports that hold one")]
    TwoEdges {
        /// The wire.
        ends: WireEnds<K>,
    },
    /// A wire the graph would refuse: a port or a node the declared shape does
    /// not have, a width or a fold that does not fit.
    #[error("the wire {ends:?} is refused: {cause}")]
    Wire {
        /// The wire.
        ends: WireEnds<K>,
        /// What [`Graph::connect`](crate::Graph::connect) would refuse with.
        cause: ConnectError,
    },
}

/// What [`Session::patch`](crate::Session::patch) did to nodes it had already
/// declared. A key added for the first time is in none of the lists.
///
/// A node whose swap was refused keeps running as it was: its key is in
/// [`unconverged`](Self::unconverged), and the next patch that declares it
/// tries again.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Patched<K> {
    /// Nodes swapped for one of another structure, under the same key.
    pub rebuilt: Vec<K>,
    /// Nodes whose swap the graph refused, with why: a crossfade across
    /// another shape, a poisoned graph.
    pub unconverged: Vec<(K, ReplaceRefused)>,
    /// Nodes of the last patch that this one does not declare, taken out.
    pub removed: Vec<K>,
    /// Keys declared before whose node is not the one declared now: it was
    /// removed by hand, or the key's config is of another type. A new node is
    /// added in its place.
    pub readded: Vec<K>,
    /// Wires the patch declares that the graph does not hold yet: their ports
    /// are on the declared node but not on the one running (its swap was
    /// refused or is parked), with the graph's cause. The next patch that
    /// declares them tries again.
    pub dormant: Vec<(WireEnds<K>, ConnectError)>,
}

/// One declaration with what the plan decided: its fate, and the key its node
/// has (a new key for a new node).
struct PlannedNode<K, F: Float> {
    key: K,
    declaration: Declaration<Graph<F>>,
    fate: Fate,
    id: NodeKey,
}

/// A patch's declarations planned: the shapes of the nodes it builds, and the
/// node each key names once it is applied.
struct Planned<K, F: Float> {
    nodes: Vec<PlannedNode<K, F>>,
    shapes: Shapes,
    at: BTreeMap<K, NodeKey>,
}

/// What a patch declared of one key, and the node it made.
pub(crate) struct Stored<F: Float> {
    pub(crate) node: NodeKey,
    pub(crate) declaration: Declaration<Graph<F>>,
}

/// The declarations of the last patch, by app key. Control side only.
pub(crate) struct Kept<K, F: Float> {
    nodes: BTreeMap<K, Stored<F>>,
    /// The wires the last patch made, by their ends and delay.
    wires: BTreeMap<WireId<K>, KeptWire>,
    /// The sections the last patch evaluated or carried, by their keys.
    sections: BTreeMap<K, KeptSection<K>>,
}

/// What the last patch made of one section: its deps while it converged, and
/// the keys, wires and sections its body declared itself (not those of the
/// sections inside it).
struct KeptSection<K> {
    /// `None` when a key of the section's subtree was left unconverged or a
    /// wire dormant: the next patch runs the body.
    deps: Option<Box<dyn Deps>>,
    nodes: Vec<K>,
    wires: Vec<WireId<K>>,
    sections: Vec<K>,
}

/// The record of `key` and of the sections inside it, outermost first.
fn subtree<'a, K: Ord>(
    sections: &'a BTreeMap<K, KeptSection<K>>,
    key: &K,
) -> Vec<(&'a K, &'a KeptSection<K>)> {
    let mut found = Vec::new();
    let mut stack = vec![key];
    while let Some(key) = stack.pop() {
        if let Some((name, record)) = sections.get_key_value(key) {
            found.push((name, record));
            stack.extend(&record.sections);
        }
    }
    found
}

impl<K, F: Float> Default for Kept<K, F> {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
            wires: BTreeMap::new(),
            sections: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone, F: Float> Kept<K, F> {
    /// Replaces the section records: the carried subtrees keep theirs, the
    /// evaluated sections get theirs (in the order they ran, so a section's
    /// inner ones are judged first), and a section the patch does not declare
    /// loses its record.
    fn keep_sections(
        &mut self,
        carried: &[K],
        evaluated: Vec<(K, KeptSection<K>)>,
        patched: &Patched<K>,
    ) {
        let mut old = std::mem::take(&mut self.sections);
        let unconverged: BTreeSet<&K> = patched.unconverged.iter().map(|(key, _)| key).collect();
        let dormant: BTreeSet<&WireEnds<K>> =
            patched.dormant.iter().map(|(ends, _)| ends).collect();
        for key in carried {
            let names: Vec<K> = subtree(&old, key)
                .into_iter()
                .map(|(name, _)| name.clone())
                .collect();
            for name in names {
                if let Some(record) = old.remove(&name) {
                    self.sections.insert(name, record);
                }
            }
        }
        for (key, mut record) in evaluated.into_iter().rev() {
            let converged = record.nodes.iter().all(|key| !unconverged.contains(key))
                && record.wires.iter().all(|id| !dormant.contains(&id.ends))
                && record
                    .sections
                    .iter()
                    .all(|inner| self.sections.get(inner).is_some_and(|r| r.deps.is_some()));
            if !converged {
                record.deps = None;
            }
            self.sections.insert(key, record);
        }
    }
}

/// What a declaration does to the node under its key, judged before any node is
/// built.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Readd,
    Values,
    Swap,
}

/// A patch's parts with its sections decided: the parts of the sections that
/// ran joined to the patch's own, and the sections carried whole.
struct Decided<K, F: Float> {
    nodes: Vec<(K, Declaration<Graph<F>>)>,
    wires: Vec<WireDecl<K>>,
    /// The carried sections, the outermost of each carried subtree.
    carried: Vec<K>,
    /// The keys and wires the carried subtrees declare.
    carried_nodes: Vec<K>,
    carried_wires: Vec<WireId<K>>,
    /// The records of the sections that ran, in the order they ran.
    evaluated: Vec<(K, KeptSection<K>)>,
    /// Sections met and not yet judged.
    queue: VecDeque<PendingSection<K, Graph<F>>>,
    /// The section keys met.
    names: BTreeSet<K>,
    /// The sections that are carried unless a key they mention fails.
    candidates: BTreeMap<K, PendingSection<K, Graph<F>>>,
    /// The node keys of the candidates' subtrees.
    standing: BTreeSet<K>,
    /// The index in `nodes` of each key a section that ran declares.
    declared_at: BTreeMap<K, usize>,
}

impl<K: AppKey, F: Float> Decided<K, F> {
    fn new(patch: Declarations<K, Graph<F>>) -> Self {
        let declared_at = patch
            .nodes
            .iter()
            .enumerate()
            .map(|(at, (key, _))| (key.clone(), at))
            .collect();
        Self {
            nodes: patch.nodes,
            wires: patch.wires,
            carried: Vec::new(),
            carried_nodes: Vec::new(),
            carried_wires: Vec::new(),
            evaluated: Vec::new(),
            queue: patch.sections.into(),
            names: BTreeSet::new(),
            candidates: BTreeMap::new(),
            standing: BTreeSet::new(),
            declared_at,
        }
    }

    /// Runs `section`'s body: what it declares joins the patch, and the
    /// sections inside it wait to be judged.
    fn run(&mut self, section: PendingSection<K, Graph<F>>) {
        let PendingSection { key, deps, body } = section;
        let inner = body(&*deps);
        let record = KeptSection {
            deps: Some(deps),
            nodes: inner.nodes.iter().map(|(key, _)| key.clone()).collect(),
            wires: inner.wires.iter().map(WireDecl::id).collect(),
            sections: inner.sections.iter().map(|s| s.key.clone()).collect(),
        };
        for (key, declaration) in inner.nodes {
            self.declared_at.insert(key.clone(), self.nodes.len());
            self.nodes.push((key, declaration));
        }
        self.wires.extend(inner.wires);
        self.queue.extend(inner.sections);
        self.evaluated.push((key, record));
    }
}

/// What a declaration does to the node under its key, with the node it builds
/// (built once, in the plan).
enum Fate {
    /// The key is new.
    Add(Built),
    /// The node is gone or the config is of another type: a new node.
    Readd(Built),
    /// The structure is the same: the differing values are written.
    Values,
    /// Another structure: the node is swapped.
    Swap(Built),
}

impl<K: fmt::Debug, F: Float> fmt::Debug for Kept<K, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.nodes.iter().map(|(key, s)| (key, s.node)))
            .finish()
    }
}

/// Takes `gone` out of `spec`: the nodes, and every edge into or out of them
/// (audio, event, param, gain and bus).
fn forget_nodes(spec: &mut GraphSpec, gone: &BTreeSet<NodeKey>) {
    let from_gone =
        |source: PortSource| matches!(source, PortSource::Node(p) if gone.contains(&p.node()));
    let edge_gone = |edge: &Edge| match *edge {
        Edge::Direct(source) => from_gone(source),
        Edge::Feedback(f) => gone.contains(&f.from.node()),
    };
    for node in gone {
        spec.topology.nodes.remove(node);
    }
    spec.topology.edges.retain(|at, edges| {
        edges.retain(|e| !edge_gone(e));
        !gone.contains(&at.node()) && !edges.is_empty()
    });
    for sources in &mut spec.topology.outputs {
        sources.retain(|source| !from_gone(*source));
    }
    spec.events.retain(|at, edges| {
        edges.retain(|e| !gone.contains(&e.from().node()));
        !gone.contains(&at.node()) && !edges.is_empty()
    });
    spec.params
        .retain(|at, from| !gone.contains(&at.node) && !gone.contains(&from.node()));
    spec.gains.retain(|id, _| {
        let sink = matches!(id.sink, PortSink::Port(at) if gone.contains(&at.node()));
        !sink && !edge_gone(&id.from)
    });
    let bus_gone = |source: &BusOutputPort| matches!(source, BusOutputPort::Node(bus) if gone.contains(&bus.node()));
    spec.bus_edges.retain(|at, feeds| {
        feeds.retain(|feed| !bus_gone(&feed.source));
        !gone.contains(&at.node()) && !feeds.is_empty()
    });
    spec.main_output_sources
        .retain(|feed| !bus_gone(&feed.source));
}

/// The edges leaving the graph that would close a cycle with the patch's
/// wiring: cut at the patch, not faded.
#[derive(Default)]
struct Cuts {
    edges: Vec<EdgeId>,
    buses: Vec<(BusOutputPort, BusInputPort)>,
}

/// The assignments of what `new` gives that `old` did not give or gave
/// another value, to the node at `node`, of the node type `owner`.
fn declared_writes<F: Float>(
    graph: &Graph<F>,
    node: NodeKey,
    owner: TypeId,
    old: &Initials,
    new: &Initials,
    writes: &mut Vec<AnyAssign>,
) {
    writes.extend(
        new.params()
            .iter()
            .filter(|given| !old.params().contains(given))
            .map(|&(addr, raw)| declared_param(node, addr, owner, raw)),
    );
    for &(kind, word) in new.settings() {
        if old.settings().contains(&(kind, word)) {
            continue;
        }
        let of = graph
            .setting_enum(node, kind)
            .expect("a kinded initial names a setting the node holds");
        writes.push(declared_setting(node, kind, word, of));
    }
    // A kind the session does not address has no target to write: its node
    // is left out of the session's values, as at insertion.
    writes.extend(
        new.published_values()
            .iter()
            .filter(|given| !old.published_values().contains(given))
            .filter_map(|initial| declared_published(node, initial)),
    );
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Makes the graph's declared nodes the ones `patch` declares, under the
    /// app's keys, and the wires between them the ones it declares. A key
    /// declared before and again now keeps its node unless the declaration
    /// says it must change ([`NodeDecl`](crate::NodeDecl) has the rules); a key
    /// no longer declared loses its node, over the fade its last declaration
    /// chose. [`Patched`] reports what happened to the keys that were declared
    /// before.
    ///
    /// A wire the last patch declared and this one does not is disconnected
    /// (faded unless it was declared `unfaded`); a new one is connected; a
    /// kept one changes only in its initial gain. A wire is checked against
    /// the shapes of the nodes as declared, before anything changes.
    ///
    /// A [`Section`](crate::Section) whose deps equal the last patch's, whose
    /// keys converged and whose nodes and wires the graph still holds, is
    /// carried: its body is not run, nothing of it is compared or written, and
    /// the node and wires it made stay. A section whose wires mention a key
    /// outside it that the patch no longer declares, or declares with another
    /// structure or config type, runs.
    ///
    /// A node or an edge that [`Graph::add`](crate::Graph::add) or
    /// [`Graph::connect`](crate::Graph::connect) made is not declared: a patch
    /// never removes or changes it. Declaring a wire that `connect` already made
    /// adopts that edge: it is kept once, and a later patch that drops the wire
    /// disconnects it. A wire declared and removed by hand is made again.
    ///
    /// # Errors
    ///
    /// [`PatchError::DuplicateKey`] when a key is declared twice in `patch`,
    /// [`PatchError::DuplicateSection`] when a key names two sections,
    /// [`PatchError::NotFinite`] when a config or a wire gives an initial that
    /// is NaN or infinite, [`PatchError::Undeclared`] for a wire that names a
    /// key `patch` does not declare, and [`PatchError::Wire`] for a wire the
    /// declared nodes cannot make; nothing changes.
    ///
    /// # Panics
    ///
    /// If the calling thread holds the value lock (a `modify` closure calling
    /// `patch`): the changed initials are written under it.
    pub fn patch(
        &mut self,
        patch: impl IntoPatch<K, Graph<C::Float>>,
    ) -> Result<Patched<K>, PatchError<K>> {
        let Decided {
            nodes,
            wires: declared,
            carried,
            carried_nodes,
            carried_wires,
            evaluated,
            ..
        } = self.decide(Declarations::of(patch))?;
        let mut seen = BTreeSet::new();
        let again = nodes
            .iter()
            .map(|(key, _)| key)
            .chain(&carried_nodes)
            .find(|key| !seen.insert((*key).clone()));
        if let Some(key) = again {
            return Err(PatchError::DuplicateKey(key.clone()));
        }
        let finite = |declaration: &Declaration<Graph<C::Float>>| {
            declaration
                .config
                .initials()
                .params()
                .iter()
                .all(|(_, raw)| raw.is_finite())
        };
        if let Some((key, _)) = nodes.iter().find(|(_, d)| !finite(d)) {
            return Err(PatchError::NotFinite(key.clone()));
        }
        let mut wires = Self::declared_wires(&declared, &seen)?;
        // A carried wire is kept as it was made.
        for id in carried_wires {
            if let Some(&kept) = self.declared.wires.get(&id) {
                wires.entry(id).or_insert(kept);
            }
        }
        let host = self.host_values();
        let mut kept = std::mem::take(&mut self.declared);
        let planned = self.plan(&kept, nodes, &carried_nodes, &host);
        let checked = self
            .check_wires(&planned, &wires)
            .and_then(|()| self.check_feedback(&planned, &wires, &kept))
            .and_then(|()| self.check_acyclic(&planned, &wires, &kept));
        let cuts = match checked {
            Ok(cuts) => cuts,
            Err(refused) => {
                self.declared = kept;
                return Err(refused);
            }
        };
        let mut patched = Patched {
            rebuilt: Vec::new(),
            unconverged: Vec::new(),
            removed: Vec::new(),
            readded: Vec::new(),
            dormant: Vec::new(),
        };
        self.unwire(&kept, &wires);
        // What would close a cycle beside the new wiring goes at once, not
        // over a fade.
        self.graph.cut_edges(&cuts.edges);
        for &(from, to) in &cuts.buses {
            self.graph.remove_bus_edge(from, to, true);
        }
        let gone: Vec<K> = kept
            .nodes
            .keys()
            .filter(|key| !seen.contains(key))
            .cloned()
            .collect();
        drop(seen);
        for key in gone {
            let removed = kept.nodes.remove(&key).is_some_and(|s| self.take_out(&s));
            if removed {
                patched.removed.push(key);
            }
        }
        let mut writes = Vec::new();
        let mut fresh = BTreeSet::new();
        for planned in planned.nodes {
            if matches!(planned.fate, Fate::Add(_) | Fate::Readd(_)) {
                fresh.insert(planned.key.clone());
            }
            self.declare(&mut kept, planned, &mut patched, &mut writes);
        }
        let old = std::mem::take(&mut kept.wires);
        kept.wires = self.wire(&kept, old, wires, &fresh, &mut patched, &mut writes);
        kept.keep_sections(&carried, evaluated, &patched);
        self.declared = kept;
        // One call, one gesture: every changed initial lands in one batch.
        self.values
            .set(writes)
            .expect("an initial of a config's own node is addressed, finite and not refused");
        Ok(patched)
    }

    /// The wires `patch` declares by their ends, each once.
    fn declared_wires(
        declarations: &[WireDecl<K>],
        declared: &BTreeSet<K>,
    ) -> Result<BTreeMap<WireId<K>, KeptWire>, PatchError<K>> {
        let mut wires = BTreeMap::new();
        for wire in declarations {
            let ends = [Some(wire.ends.source()), wire.ends.sink()];
            if let Some(key) = ends.into_iter().flatten().find(|k| !declared.contains(k)) {
                return Err(PatchError::Undeclared(key.clone()));
            }
            if wire.initial.is_some_and(|gain| !gain.get().is_finite()) {
                return Err(PatchError::NotFinite(wire.ends.source().clone()));
            }
            wires.insert(wire.id(), KeptWire::of(wire));
        }
        Ok(wires)
    }

    /// What each declaration does, with its node built once when it makes a
    /// new one; a carried key names the node it had.
    fn plan(
        &self,
        kept: &Kept<K, C::Float>,
        nodes: Vec<(K, Declaration<Graph<C::Float>>)>,
        carried: &[K],
        host: &HostValues,
    ) -> Planned<K, C::Float> {
        let mut planned = Planned {
            nodes: Vec::with_capacity(nodes.len()),
            shapes: Shapes::new(),
            at: BTreeMap::new(),
        };
        for key in carried {
            planned.at.insert(key.clone(), kept.nodes[key].node);
        }
        for (key, declaration) in nodes {
            let fate = self.fate(kept, &key, &declaration, host);
            let id = match &fate {
                Fate::Add(_) | Fate::Readd(_) => NodeKey::fresh(),
                Fate::Values | Fate::Swap(_) => kept.nodes[&key].node,
            };
            if let Fate::Add(built) | Fate::Readd(built) | Fate::Swap(built) = &fate {
                planned.shapes.insert(id, built.shape);
            }
            planned.at.insert(key.clone(), id);
            planned.nodes.push(PlannedNode {
                key,
                declaration,
                fate,
                id,
            });
        }
        planned
    }

    fn fate(
        &self,
        kept: &Kept<K, C::Float>,
        key: &K,
        declaration: &Declaration<Graph<C::Float>>,
        host: &HostValues,
    ) -> Fate {
        let new = &declaration.config;
        match self.kind(kept, key, declaration) {
            Kind::Add => Fate::Add(new.build(host)),
            Kind::Readd => Fate::Readd(new.build(host)),
            Kind::Values => Fate::Values,
            Kind::Swap => Fate::Swap(new.build(host)),
        }
    }

    /// What `declaration` does to the node under `key`, with nothing built.
    fn kind(
        &self,
        kept: &Kept<K, C::Float>,
        key: &K,
        declaration: &Declaration<Graph<C::Float>>,
    ) -> Kind {
        let new = &declaration.config;
        let Some(old) = kept.nodes.get(key) else {
            return Kind::Add;
        };
        let same_type = old.declaration.config.config_type() == new.config_type();
        if !self.graph.contains(old.node) || !same_type {
            Kind::Readd
        } else if old.declaration.config.same_structure(&**new) {
            Kind::Values
        } else {
            Kind::Swap
        }
    }

    /// Decides each section of `patch`: carried whole, or run with the parts it
    /// declares joining the patch. A section is carried only while the keys it
    /// mentions stand, and running one can make another's keys fall, so the
    /// judgement repeats until none changes; each section runs at most once.
    fn decide(
        &self,
        patch: Declarations<K, Graph<C::Float>>,
    ) -> Result<Decided<K, C::Float>, PatchError<K>> {
        let mut decided = Decided::new(patch);
        self.settle(&mut decided)?;
        loop {
            let falling: Vec<K> = decided
                .candidates
                .keys()
                .filter(|key| !self.stands(&decided, key))
                .cloned()
                .collect();
            if falling.is_empty() {
                break;
            }
            // Running one can restore what another mentions: judge again.
            for key in falling {
                if self.stands(&decided, &key) {
                    continue;
                }
                let section = decided
                    .candidates
                    .remove(&key)
                    .expect("a falling section is a candidate");
                for (_, record) in subtree(&self.declared.sections, &key) {
                    for node in &record.nodes {
                        decided.standing.remove(node);
                    }
                }
                decided.run(section);
                self.settle(&mut decided)?;
            }
        }
        for key in decided.candidates.keys() {
            decided.carried.push(key.clone());
            for (name, record) in subtree(&self.declared.sections, key) {
                if name != key && !decided.names.insert(name.clone()) {
                    return Err(PatchError::DuplicateSection(name.clone()));
                }
                decided.carried_nodes.extend(record.nodes.iter().cloned());
                decided.carried_wires.extend(record.wires.iter().cloned());
            }
        }
        Ok(decided)
    }

    /// Judges the sections waiting in `decided`: a section whose record does
    /// not match runs, one that does becomes a candidate to be carried.
    fn settle(&self, decided: &mut Decided<K, C::Float>) -> Result<(), PatchError<K>> {
        while let Some(section) = decided.queue.pop_front() {
            if !decided.names.insert(section.key.clone()) {
                return Err(PatchError::DuplicateSection(section.key));
            }
            if self.matches(&section) {
                for (_, record) in subtree(&self.declared.sections, &section.key) {
                    decided.standing.extend(record.nodes.iter().cloned());
                }
                decided.candidates.insert(section.key.clone(), section);
            } else {
                decided.run(section);
            }
        }
        Ok(())
    }

    /// Whether the last patch made `section` from equal deps and the graph still
    /// holds what it made: every node, every wire, and no key unconverged.
    fn matches(&self, section: &PendingSection<K, Graph<C::Float>>) -> bool {
        let kept = &self.declared;
        let Some(record) = kept.sections.get(&section.key) else {
            return false;
        };
        let equal = record
            .deps
            .as_deref()
            .is_some_and(|last| last.same(&*section.deps));
        equal
            && subtree(&kept.sections, &section.key)
                .into_iter()
                .all(|(_, record)| {
                    let nodes = record.nodes.iter().all(|key| {
                        kept.nodes
                            .get(key)
                            .is_some_and(|s| self.graph.contains(s.node))
                    });
                    nodes
                        && record.wires.iter().all(|id| {
                            let known = [Some(id.source()), id.sink()]
                                .into_iter()
                                .flatten()
                                .all(|key| kept.nodes.contains_key(key));
                            known && Resolved::of(id, |key| kept.nodes[key].node).held(&self.graph)
                        })
                })
    }

    /// Whether every key the candidate `section`'s wires mention outside it
    /// is declared by the rest of the patch, with the node it had.
    fn stands(&self, decided: &Decided<K, C::Float>, section: &K) -> bool {
        let records = subtree(&self.declared.sections, section);
        let own: BTreeSet<&K> = records
            .iter()
            .flat_map(|(_, record)| &record.nodes)
            .collect();
        records
            .iter()
            .flat_map(|(_, record)| &record.wires)
            .flat_map(|id| [Some(id.source()), id.sink()])
            .flatten()
            .filter(|key| !own.contains(key))
            .all(|key| {
                decided.standing.contains(key)
                    || decided.declared_at.get(key).is_some_and(|&at| {
                        let (key, declaration) = &decided.nodes[at];
                        self.kind(&self.declared, key, declaration) == Kind::Values
                    })
            })
    }

    /// Refuses a wire the declared nodes cannot make, with nothing changed.
    fn check_wires(
        &self,
        planned: &Planned<K, C::Float>,
        wires: &BTreeMap<WireId<K>, KeptWire>,
    ) -> Result<(), PatchError<K>> {
        let pending = Some(&planned.shapes);
        let mut buses = Vec::new();
        let mut bus_ends = Vec::new();
        for id in wires.keys() {
            let resolved = Resolved::of(id, |key| planned.at[key]);
            resolved
                .check(&self.graph, pending)
                .map_err(|cause| PatchError::Wire {
                    ends: id.ends.clone(),
                    cause,
                })?;
            if let Resolved::Bus(from, to) = resolved {
                buses.push((from, to));
                bus_ends.push(&id.ends);
            }
        }
        if let Some((at, cause)) = self.graph.first_unlowered(pending, &buses) {
            return Err(PatchError::Wire {
                ends: bus_ends[at].clone(),
                cause: ConnectError::Bus(cause),
            });
        }
        Ok(())
    }

    /// Refuses a feedback wire the graph would not compile, and a wire that
    /// puts a second edge on ports that hold one, with nothing changed.
    fn check_feedback(
        &self,
        planned: &Planned<K, C::Float>,
        wires: &BTreeMap<WireId<K>, KeptWire>,
        kept: &Kept<K, C::Float>,
    ) -> Result<(), PatchError<K>> {
        let pending = Some(&planned.shapes);
        let max_block = self.graph.max_block_samples();
        // What the patch takes out leaves its ports free for another kind.
        let mut leaving = BTreeSet::new();
        for id in kept.wires.keys().filter(|id| !wires.contains_key(*id)) {
            let held = [Some(id.source()), id.sink()]
                .into_iter()
                .flatten()
                .all(|key| kept.nodes.contains_key(key));
            if held {
                let resolved = Resolved::of(id, |key| kept.nodes[key].node);
                leaving.extend(
                    Ports::of(resolved, &self.graph, None)
                        .into_iter()
                        .map(|ports| (ports, id.delay)),
                );
            }
        }
        let mut declared: BTreeMap<Ports, Option<Samples>> = BTreeMap::new();
        for id in wires.keys() {
            if let Some(delay) = id.delay
                && delay < max_block
            {
                return Err(PatchError::FeedbackTooShort {
                    ends: id.ends.clone(),
                    delay,
                    max_block,
                });
            }
            let resolved = Resolved::of(id, |key| planned.at[key]);
            for ports in Ports::of(resolved, &self.graph, pending) {
                let twice = declared
                    .insert(ports, id.delay)
                    .is_some_and(|other| other != id.delay);
                let held = ports
                    .held_delay(&self.graph)
                    .is_some_and(|held| held != id.delay && !leaving.contains(&(ports, held)));
                if twice || held {
                    return Err(PatchError::TwoEdges {
                        ends: id.ends.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Refuses a wire that closes a cycle the graph would not compile, and
    /// names the edges leaving the graph that must be cut for it not to: the
    /// graph's own search ([`GraphSpec::cycle`](tutti_graph::GraphSpec::cycle))
    /// over its whole wiring, the edges it only fades out included (the compile
    /// counts them), plus the declared wires. A feedback edge is cut by the
    /// search. An edge that leaves (dropped by this patch, or still fading)
    /// and closes a cycle is cut; a cycle left through live wires is the
    /// patch's.
    fn check_acyclic(
        &self,
        planned: &Planned<K, C::Float>,
        wires: &BTreeMap<WireId<K>, KeptWire>,
        kept: &Kept<K, C::Float>,
    ) -> Result<Cuts, PatchError<K>> {
        let pending = Some(&planned.shapes);
        let mut spec = self.graph.whole_spec();
        // The nodes this patch takes out are not in the graph it makes, nor
        // are the edges into or out of them, hand-made ones included.
        let gone: BTreeSet<NodeKey> = kept
            .nodes
            .iter()
            .filter(|(key, _)| !planned.at.contains_key(*key))
            .map(|(_, stored)| stored.node)
            .chain(
                planned
                    .nodes
                    .iter()
                    .filter(|p| matches!(p.fate, Fate::Readd(_)))
                    .filter_map(|p| kept.nodes.get(&p.key).map(|stored| stored.node)),
            )
            .collect();
        forget_nodes(&mut spec, &gone);
        for (&node, shape) in &planned.shapes {
            let declared = NodeSpec::new("declared", shape.audio_in(), shape.audio_out());
            spec.topology.nodes.insert(node, declared);
        }
        let (mut leaving, mut leaving_buses) = self.graph.fading_edges();
        for id in kept.wires.keys().filter(|id| !wires.contains_key(*id)) {
            let held = [Some(id.source()), id.sink()]
                .into_iter()
                .flatten()
                .all(|key| kept.nodes.contains_key(key));
            if !held {
                continue;
            }
            let resolved = Resolved::of(id, |key| kept.nodes[key].node);
            match resolved {
                // An event edge is never faded: it goes at once.
                Resolved::Event(..) => {
                    resolved.in_spec(&mut spec, &self.graph, None, false);
                }
                Resolved::Bus(from, to) => {
                    leaving_buses.insert((from, to));
                }
                Resolved::Channel(..) | Resolved::Main(..) => {
                    for ports in Ports::of(resolved, &self.graph, None) {
                        if let Ports::Audio(edge) = ports
                            && let Some(edge) = Graph::<C::Float>::edge_id(edge, id.delay)
                        {
                            leaving.insert(edge);
                        }
                    }
                }
            }
        }
        let named: Vec<_> = wires
            .keys()
            .map(|id| {
                let resolved = Resolved::of(id, |key| planned.at[key]);
                // A declared wire is not leaving, whatever the graph fades.
                for ports in Ports::of(resolved, &self.graph, pending) {
                    if let Ports::Audio(edge) = ports
                        && let Some(edge) = Graph::<C::Float>::edge_id(edge, id.delay)
                    {
                        leaving.remove(&edge);
                    }
                }
                if let Resolved::Bus(from, to) = resolved {
                    leaving_buses.remove(&(from, to));
                }
                (id, resolved.in_spec(&mut spec, &self.graph, pending, true))
            })
            .collect();
        let mut cuts = Cuts::default();
        let cycle = loop {
            let Some(cycle) = spec.cycle() else {
                break None;
            };
            let audio = cycle.iter().find_map(|edge| match edge {
                CycleEdge::Audio(at) => leaving
                    .iter()
                    .find(|id| id.sink == PortSink::Port(*at))
                    .copied(),
                _ => None,
            });
            if let Some(id) = audio {
                leaving.remove(&id);
                if let PortSink::Port(at) = id.sink {
                    spec.disconnect(at, id.from);
                }
                cuts.edges.push(id);
                continue;
            }
            let bus = cycle.iter().find_map(|edge| match edge {
                CycleEdge::Bus { at, from } => leaving_buses
                    .iter()
                    .find(|(f, t)| f == from && t.0 == BusEnd::Node(*at))
                    .copied(),
                _ => None,
            });
            if let Some((from, to)) = bus {
                leaving_buses.remove(&(from, to));
                if let BusEnd::Node(at) = to.0 {
                    spec.disconnect_bus(from, at);
                }
                cuts.buses.push((from, to));
                continue;
            }
            break Some(cycle);
        };
        let Some(cycle) = cycle else {
            return Ok(cuts);
        };
        // Only a cycle through a declared wire is the patch's; any other the
        // commit reports.
        match named
            .into_iter()
            .find(|(_, edges)| edges.iter().any(|edge| cycle.contains(edge)))
        {
            Some((id, _)) => Err(PatchError::Cycle {
                ends: id.ends.clone(),
            }),
            None => Ok(cuts),
        }
    }

    /// Disconnects the wires the last patch made and this one does not.
    fn unwire(&mut self, kept: &Kept<K, C::Float>, wires: &BTreeMap<WireId<K>, KeptWire>) {
        for (id, old) in &kept.wires {
            if wires.contains_key(id) {
                continue;
            }
            // A node that is not kept took its wires with it, and a wire the
            // graph no longer has (its node was removed by hand) has nothing
            // to take out.
            let nodes = [Some(id.source()), id.sink()];
            if nodes
                .into_iter()
                .flatten()
                .all(|key| kept.nodes.contains_key(key))
            {
                let resolved = Resolved::of(id, |key| kept.nodes[key].node);
                let _ = resolved.disconnect(&mut self.graph, old.unfaded);
            }
        }
    }

    /// Makes the wires of this patch, and the lanes and initials they declare,
    /// and returns what the patch now holds of them.
    fn wire(
        &mut self,
        kept: &Kept<K, C::Float>,
        old: BTreeMap<WireId<K>, KeptWire>,
        wires: BTreeMap<WireId<K>, KeptWire>,
        fresh: &BTreeSet<K>,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) -> BTreeMap<WireId<K>, KeptWire> {
        let mut made = BTreeMap::new();
        for (id, options) in wires {
            let resolved = Resolved::of(&id, |key| kept.nodes[key].node);
            if let Err(cause) = resolved.check(&self.graph, None) {
                patched.dormant.push((id.ends, cause));
                continue;
            }
            let touches_fresh = [Some(id.source()), id.sink()]
                .into_iter()
                .flatten()
                .any(|key| fresh.contains(key));
            match old.get(&id) {
                Some(before) if !touches_fresh && resolved.held(&self.graph) => {
                    self.regain(resolved, before.initial, options.initial, writes);
                }
                _ => {
                    let connected =
                        resolved.connect(&mut self.graph, options.unfaded, options.initial);
                    if let Err(cause) = connected {
                        patched.dormant.push((id.ends, cause));
                        continue;
                    }
                }
            }
            made.insert(id, options);
        }
        made
    }

    /// Moves the gain lanes of a kept wire from the initial it was declared
    /// with to the one it is declared with now.
    fn regain(
        &mut self,
        wire: Resolved,
        before: Option<Gain>,
        now: Option<Gain>,
        writes: &mut Vec<AnyAssign>,
    ) {
        if before == now {
            return;
        }
        for edge in wire.edges(&self.graph, None) {
            match (before, now) {
                (None, Some(gain)) => {
                    self.graph
                        .declare_edge_gain(edge, gain)
                        .expect("a declared initial gain is finite");
                }
                (Some(_), None) => {
                    edge.clear_gain(&mut self.graph);
                }
                (Some(_), Some(gain)) => {
                    if let Ok(target) = self.graph.gain_target(edge.source(), edge.sink()) {
                        writes.push(target.to(gain).erase());
                    }
                }
                (None, None) => {}
            }
        }
    }

    /// Makes `key`'s node match its declaration.
    fn declare(
        &mut self,
        kept: &mut Kept<K, C::Float>,
        planned: PlannedNode<K, C::Float>,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) {
        let PlannedNode {
            key,
            declaration,
            fate,
            id,
        } = planned;
        let new = &declaration.config;
        match fate {
            Fate::Add(built) => {
                new.add(&mut self.graph, id, built);
                kept.nodes.insert(
                    key,
                    Stored {
                        node: id,
                        declaration,
                    },
                );
            }
            Fate::Readd(built) => {
                // Taking out a node that is gone finds nothing.
                if let Some(old) = kept.nodes.get(&key) {
                    self.take_out(old);
                }
                new.add(&mut self.graph, id, built);
                patched.readded.push(key.clone());
                kept.nodes.insert(
                    key,
                    Stored {
                        node: id,
                        declaration,
                    },
                );
            }
            Fate::Swap(built) => {
                let old = kept.nodes.get_mut(&key).expect("a swap has its old node");
                // A replace seeds each param from the outgoing node only where
                // both declared one initial (`ValueTable::address`), so an
                // initial this patch changed is already the incoming node's.
                match new.replace(&mut self.graph, built, old.node, declaration.fading) {
                    Ok(()) => {
                        patched.rebuilt.push(key);
                        old.declaration = declaration;
                    }
                    Err(refused) => patched.unconverged.push((key, refused)),
                }
            }
            Fate::Values => {
                let old = kept.nodes.get_mut(&key).expect("a kept key has its node");
                declared_writes(
                    &self.graph,
                    old.node,
                    new.node_type(),
                    &old.declaration.config.initials(),
                    &declaration.config.initials(),
                    writes,
                );
                old.declaration = declaration;
            }
        }
    }

    /// Takes the node of `stored` out of the session over the fade its
    /// declaration chose, and whether it was there.
    fn take_out(&mut self, stored: &Stored<C::Float>) -> bool {
        let Ok(removal) = self.remove(stored.node) else {
            return false;
        };
        if stored.declaration.fading == Fading::Cut {
            let _ = removal.unfaded();
        }
        true
    }

    /// The typed key of the node declared under `key` with a config of type
    /// `Cfg`, to set its values and read them.
    ///
    /// # Errors
    ///
    /// [`KeyError`]: `Undeclared`, `OtherConfig` (declared with a config of
    /// another type) or `Gone` (its node was removed by hand).
    pub fn key<Cfg: NodeConfig<C::Float>>(&self, key: &K) -> Result<Key<Cfg::Node>, KeyError<K>> {
        let Some(stored) = self.declared.nodes.get(key) else {
            return Err(KeyError::Undeclared(key.clone()));
        };
        if stored.declaration.config.config_type() != TypeId::of::<Cfg>() {
            return Err(KeyError::OtherConfig(key.clone()));
        }
        if !self.graph.contains(stored.node) {
            return Err(KeyError::Gone(key.clone()));
        }
        Ok(Key::new(stored.node))
    }

    /// The values the session holds once for many nodes: what a node reads
    /// through a reader registered as it is built.
    fn host_values(&self) -> HostValues {
        let host = HostValues::new();
        #[cfg(feature = "midi")]
        let host = host.provide(self.midi.state.mpe());
        host
    }
}
