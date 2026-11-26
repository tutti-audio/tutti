//! What [`Session::patch`] does with the declarations of [`crate::patch`]:
//! the session keeps the last patch's declarations by app key, and a patch is
//! diffed against them, never against the graph. A node `Graph::add` made is
//! not in them: a patch neither keeps nor removes it.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use thiserror::Error;
use tutti_buffers::Float;
use tutti_graph::{
    BusOutputPort, Edge, GraphSpec, HostValues, Initials, IntoNode, NodeConfig, NodeKey, NodeSpec,
    PortSink, PortSource, Shape, Shapes,
};
use tutti_units::Gain;

use crate::graph::Split;
use crate::patch::{
    Built, Declaration, Declarations, Fading, IntoPatch, KeyError, PatchTarget, Place, WireEnds,
};
use crate::value::{declared_param, declared_published, declared_setting};
use crate::{AnyAssign, AppKey, Clock, ConnectError, Graph, Key, ReplaceRefused, Session};

mod wire;
use wire::{KeptWire, Resolved};

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
    /// The wires the last patch made, by their ends.
    wires: BTreeMap<WireEnds<K>, KeptWire>,
}

impl<K, F: Float> Default for Kept<K, F> {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
            wires: BTreeMap::new(),
        }
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
    /// A node or an edge that [`Graph::add`](crate::Graph::add) or
    /// [`Graph::connect`](crate::Graph::connect) made is not declared: a patch
    /// never removes or changes it. Declaring a wire that `connect` already made
    /// adopts that edge: it is kept once, and a later patch that drops the wire
    /// disconnects it. A wire declared and removed by hand is made again.
    ///
    /// # Errors
    ///
    /// [`PatchError::DuplicateKey`] when a key is declared twice in `patch`,
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
        let patch = Declarations::of(patch);
        let mut seen = BTreeSet::new();
        if let Some((key, _)) = patch
            .nodes
            .iter()
            .find(|(key, _)| !seen.insert(key.clone()))
        {
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
        if let Some((key, _)) = patch.nodes.iter().find(|(_, d)| !finite(d)) {
            return Err(PatchError::NotFinite(key.clone()));
        }
        let wires = Self::declared_wires(&patch, &seen)?;
        let host = self.host_values();
        let mut kept = std::mem::take(&mut self.declared);
        let planned = self.plan(&kept, patch.nodes, &host);
        let checked = self
            .check_wires(&planned, &wires)
            .and_then(|()| self.check_acyclic(&planned, &wires, &kept));
        if let Err(refused) = checked {
            self.declared = kept;
            return Err(refused);
        }
        let mut patched = Patched {
            rebuilt: Vec::new(),
            unconverged: Vec::new(),
            removed: Vec::new(),
            readded: Vec::new(),
            dormant: Vec::new(),
        };
        self.unwire(&kept, &wires);
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
        self.declared = kept;
        // One call, one gesture: every changed initial lands in one batch.
        self.values
            .set(writes)
            .expect("an initial of a config's own node is addressed, finite and not refused");
        Ok(patched)
    }

    /// The wires `patch` declares by their ends, each once.
    fn declared_wires(
        patch: &Declarations<K, Graph<C::Float>>,
        declared: &BTreeSet<K>,
    ) -> Result<BTreeMap<WireEnds<K>, KeptWire>, PatchError<K>> {
        let mut wires = BTreeMap::new();
        for wire in &patch.wires {
            let ends = [Some(wire.ends.source()), wire.ends.sink()];
            if let Some(key) = ends.into_iter().flatten().find(|k| !declared.contains(k)) {
                return Err(PatchError::Undeclared(key.clone()));
            }
            if wire.initial.is_some_and(|gain| !gain.get().is_finite()) {
                return Err(PatchError::NotFinite(wire.ends.source().clone()));
            }
            wires.insert(wire.ends.clone(), KeptWire::of(wire));
        }
        Ok(wires)
    }

    /// What each declaration does, with its node built once when it makes a
    /// new one.
    fn plan(
        &self,
        kept: &Kept<K, C::Float>,
        nodes: Vec<(K, Declaration<Graph<C::Float>>)>,
        host: &HostValues,
    ) -> Planned<K, C::Float> {
        let mut planned = Planned {
            nodes: Vec::with_capacity(nodes.len()),
            shapes: Shapes::new(),
            at: BTreeMap::new(),
        };
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
        let Some(old) = kept.nodes.get(key) else {
            return Fate::Add(new.build(host));
        };
        let same_type = old.declaration.config.config_type() == new.config_type();
        if !self.graph.contains(old.node) || !same_type {
            Fate::Readd(new.build(host))
        } else if old.declaration.config.same_structure(&**new) {
            Fate::Values
        } else {
            Fate::Swap(new.build(host))
        }
    }

    /// Refuses a wire the declared nodes cannot make, with nothing changed.
    fn check_wires(
        &self,
        planned: &Planned<K, C::Float>,
        wires: &BTreeMap<WireEnds<K>, KeptWire>,
    ) -> Result<(), PatchError<K>> {
        let pending = Some(&planned.shapes);
        let mut buses = Vec::new();
        let mut bus_ends = Vec::new();
        for ends in wires.keys() {
            let resolved = Resolved::of(ends, |key| planned.at[key]);
            resolved
                .check(&self.graph, pending)
                .map_err(|cause| PatchError::Wire {
                    ends: ends.clone(),
                    cause,
                })?;
            if let Resolved::Bus(from, to) = resolved {
                buses.push((from, to));
                bus_ends.push(ends);
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

    /// Refuses a wire that closes a cycle the graph would not compile: the
    /// graph's own search ([`GraphSpec::cycle`](tutti_graph::GraphSpec::cycle)) over its wiring, less the
    /// edges this patch takes out, plus the declared wires. A feedback edge is
    /// cut by it.
    fn check_acyclic(
        &self,
        planned: &Planned<K, C::Float>,
        wires: &BTreeMap<WireEnds<K>, KeptWire>,
        kept: &Kept<K, C::Float>,
    ) -> Result<(), PatchError<K>> {
        let pending = Some(&planned.shapes);
        let mut spec = self.graph.live_spec();
        // The nodes this patch takes out are not in the graph it makes, nor
        // are the edges into or out of them, hand-made ones included.
        let leaving: BTreeSet<NodeKey> = kept
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
        forget_nodes(&mut spec, &leaving);
        for (&node, shape) in &planned.shapes {
            let declared = NodeSpec::new("declared", shape.audio_in(), shape.audio_out());
            spec.topology.nodes.insert(node, declared);
        }
        for ends in kept.wires.keys().filter(|ends| !wires.contains_key(*ends)) {
            let held = [Some(ends.source()), ends.sink()]
                .into_iter()
                .flatten()
                .all(|key| kept.nodes.contains_key(key));
            if held {
                Resolved::of(ends, |key| kept.nodes[key].node).in_spec(
                    &mut spec,
                    &self.graph,
                    None,
                    false,
                );
            }
        }
        let named: Vec<_> = wires
            .keys()
            .map(|ends| {
                let resolved = Resolved::of(ends, |key| planned.at[key]);
                (
                    ends,
                    resolved.in_spec(&mut spec, &self.graph, pending, true),
                )
            })
            .collect();
        let Some(cycle) = spec.cycle() else {
            return Ok(());
        };
        // Only a cycle through a declared wire is the patch's; any other the
        // commit reports.
        match named
            .into_iter()
            .find(|(_, edges)| edges.iter().any(|edge| cycle.contains(edge)))
        {
            Some((ends, _)) => Err(PatchError::Cycle { ends: ends.clone() }),
            None => Ok(()),
        }
    }

    /// Disconnects the wires the last patch made and this one does not.
    fn unwire(&mut self, kept: &Kept<K, C::Float>, wires: &BTreeMap<WireEnds<K>, KeptWire>) {
        for (ends, old) in &kept.wires {
            if wires.contains_key(ends) {
                continue;
            }
            // A node that is not kept took its wires with it, and a wire the
            // graph no longer has (its node was removed by hand) has nothing
            // to take out.
            let nodes = [Some(ends.source()), ends.sink()];
            if nodes
                .into_iter()
                .flatten()
                .all(|key| kept.nodes.contains_key(key))
            {
                let resolved = Resolved::of(ends, |key| kept.nodes[key].node);
                let _ = resolved.disconnect(&mut self.graph, old.unfaded);
            }
        }
    }

    /// Makes the wires of this patch, and the lanes and initials they declare,
    /// and returns what the patch now holds of them.
    fn wire(
        &mut self,
        kept: &Kept<K, C::Float>,
        old: BTreeMap<WireEnds<K>, KeptWire>,
        wires: BTreeMap<WireEnds<K>, KeptWire>,
        fresh: &BTreeSet<K>,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) -> BTreeMap<WireEnds<K>, KeptWire> {
        let mut made = BTreeMap::new();
        for (ends, options) in wires {
            let resolved = Resolved::of(&ends, |key| kept.nodes[key].node);
            if let Err(cause) = resolved.check(&self.graph, None) {
                patched.dormant.push((ends, cause));
                continue;
            }
            let touches_fresh = [Some(ends.source()), ends.sink()]
                .into_iter()
                .flatten()
                .any(|key| fresh.contains(key));
            match old.get(&ends) {
                Some(before) if !touches_fresh && resolved.held(&self.graph) => {
                    self.regain(resolved, before.initial, options.initial, writes);
                }
                _ => {
                    let connected =
                        resolved.connect(&mut self.graph, options.unfaded, options.initial);
                    if let Err(cause) = connected {
                        patched.dormant.push((ends, cause));
                        continue;
                    }
                }
            }
            made.insert(ends, options);
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
