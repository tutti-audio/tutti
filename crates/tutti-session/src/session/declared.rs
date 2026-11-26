//! What [`Session::patch`] does with the declarations of [`crate::patch`]:
//! the session keeps the last patch's declarations by app key, and a patch is
//! diffed against them, never against the graph. A node `Graph::add` made is
//! not in them: a patch neither keeps nor removes it.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use tutti_buffers::Float;
use tutti_graph::{Fade, HostValues, Initials, IntoNode, NodeConfig, NodeKey};

use crate::patch::{
    Declaration, Declarations, Fading, IntoPatch, KeyError, PatchError, PatchTarget, Place,
};
use crate::value::{declared_param, declared_published, declared_setting};
use crate::{AnyAssign, AppKey, Clock, Graph, Key, ReplaceRefused, Replacement, Session};

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

    fn add(&mut self, config: &C, host: &HostValues) -> NodeKey {
        Graph::add(self, config.build(host)).erase()
    }

    fn swap(
        &mut self,
        key: NodeKey,
        config: &C,
        host: &HostValues,
        fade: Option<Fade>,
    ) -> Result<(), ReplaceRefused> {
        let incoming = Replacement::new(config.build(host));
        let incoming = match fade {
            Some(fade) => incoming.fade(fade),
            None => incoming,
        };
        self.replace(key, incoming).map(drop)
    }
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
}

/// What a patch declared of one key, and the node it made.
pub(crate) struct Stored<F: Float> {
    pub(crate) node: NodeKey,
    pub(crate) declaration: Declaration<Graph<F>>,
}

/// The declarations of the last patch, by app key. Control side only.
pub(crate) struct Kept<K, F: Float> {
    nodes: BTreeMap<K, Stored<F>>,
}

impl<K, F: Float> Default for Kept<K, F> {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }
}

impl<K: fmt::Debug, F: Float> fmt::Debug for Kept<K, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.nodes.iter().map(|(key, s)| (key, s.node)))
            .finish()
    }
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
    /// app's keys. A key declared before and again now keeps its node unless
    /// the declaration says it must change ([`NodeDecl`](crate::NodeDecl) has
    /// the rules); a key no longer declared loses its node, over the fade its
    /// last declaration chose. [`Patched`] reports what happened to the keys
    /// that were declared before.
    ///
    /// A node that [`Graph::add`](crate::Graph::add) made is not declared: a
    /// patch never removes or changes it.
    ///
    /// # Errors
    ///
    /// [`PatchError::DuplicateKey`] when a key is declared twice in `patch`, and
    /// [`PatchError::NotFinite`] when a config gives an initial that is NaN or
    /// infinite; nothing changes.
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
        if let Some((key, _)) = patch.nodes.iter().find(|(key, _)| !seen.insert(key)) {
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
        let host = self.host_values();
        let mut kept = std::mem::take(&mut self.declared);
        let mut patched = Patched {
            rebuilt: Vec::new(),
            unconverged: Vec::new(),
            removed: Vec::new(),
            readded: Vec::new(),
        };
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
        for (key, declaration) in patch.nodes {
            self.declare(
                &mut kept,
                key,
                declaration,
                &host,
                &mut patched,
                &mut writes,
            );
        }
        self.declared = kept;
        // One call, one gesture: every changed initial lands in one batch.
        self.values
            .set(writes)
            .expect("an initial of a config's own node is addressed, finite and not refused");
        Ok(patched)
    }

    /// Makes `key`'s node match `declaration`.
    fn declare(
        &mut self,
        kept: &mut Kept<K, C::Float>,
        key: K,
        declaration: Declaration<Graph<C::Float>>,
        host: &HostValues,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) {
        let Some(old) = kept.nodes.get_mut(&key) else {
            let node = declaration.config.add(&mut self.graph, host);
            kept.nodes.insert(key, Stored { node, declaration });
            return;
        };
        let new = &declaration.config;
        let same_type = old.declaration.config.config_type() == new.config_type();
        if !self.graph.contains(old.node) || !same_type {
            // The node is gone, or the config is another type: a new node
            // under the key. Taking out a node that is gone finds nothing.
            self.take_out(old);
            let node = new.add(&mut self.graph, host);
            patched.readded.push(key.clone());
            kept.nodes.insert(key, Stored { node, declaration });
        } else if old.declaration.config.same_structure(&**new) {
            declared_writes(
                &self.graph,
                old.node,
                new.node_type(),
                &old.declaration.config.initials(),
                &declaration.config.initials(),
                writes,
            );
            old.declaration = declaration;
        } else {
            // A replace seeds each param from the outgoing node only where
            // both declared one initial (`ValueTable::address`), so an initial
            // this patch changed is already the incoming node's.
            match new.replace(&mut self.graph, host, old.node, declaration.fading) {
                Ok(()) => {
                    patched.rebuilt.push(key);
                    old.declaration = declaration;
                }
                Err(refused) => patched.unconverged.push((key, refused)),
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
