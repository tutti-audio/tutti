//! A patch: the nodes an app declares by its own keys, and what
//! [`Session::patch`](crate::Session::patch) does to make the graph match.
//!
//! - [`NodeDecl`] names one node: an app key and the config it is built from.
//! - [`Declarations`] is the declarations of one call, and [`IntoPatch`] is everything
//!   that can be part of one: a declaration, a tuple, array, `Vec` or `Option`
//!   of parts, or a `Declarations`.
//! - [`Patched`] reports what the call did to nodes already declared.
//!
//! The session keeps the declarations of the last patch, and a patch is
//! diffed against them, never against the graph. A node `Graph::add` made is
//! not in them: a patch neither keeps nor removes it.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use thiserror::Error;
use tutti_buffers::Float;
use tutti_graph::{Fade, HostValues, Initials, IntoNode, NodeConfig, NodeKey};
use tutti_units::Samples;

use crate::value::{declared_param, declared_published, declared_setting};
use crate::{AnyAssign, AppKey, Clock, Graph, Key, ReplaceRefused, Replacement, Session};

/// How a declaration is swapped or removed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Fading {
    /// The graph's declick.
    #[default]
    Declick,
    /// A swap over this fade.
    Over(Fade),
    /// At the next block boundary, with no fade.
    Cut,
}

/// One node of a patch: the app's key for it and the config it is built from.
///
/// The key names the node for as long as the app declares it. A patch that
/// declares the key again with an equal config changes nothing; with a config
/// that differs only in its values (params and kinded settings) it writes the
/// differing values to the running node; with another structure it swaps the
/// node, crossfaded unless [`unfaded`](Self::unfaded); with a config of
/// another type it removes the node and adds the new one. A patch without the
/// key removes the node.
#[must_use = "a declaration is part of a patch only once `Session::patch` takes it"]
pub struct NodeDecl<K, C> {
    key: K,
    config: C,
    fading: Fading,
}

impl<K, C> NodeDecl<K, C> {
    /// The node of `config` under `key`, swapped and removed over the graph's
    /// declick.
    pub fn new(key: K, config: C) -> Self {
        Self {
            key,
            config,
            fading: Fading::default(),
        }
    }

    /// Swaps the node over `fade` instead of the declick, when a patch changes
    /// its structure. A removal always uses the graph's declick: only
    /// [`unfaded`](Self::unfaded) cuts it.
    pub fn fade(mut self, fade: Fade) -> Self {
        self.fading = Fading::Over(fade);
        self
    }

    /// Swaps the node at the next block boundary with no fade, and removes it
    /// the same way: a swap across another shape is then taken, not refused.
    pub fn unfaded(mut self) -> Self {
        self.fading = Fading::Cut;
        self
    }
}

impl<K: fmt::Debug, C> fmt::Debug for NodeDecl<K, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeDecl")
            .field("key", &self.key)
            .field("fading", &self.fading)
            .finish_non_exhaustive()
    }
}

/// A config held erased: what a reconcile asks of it, over the node's float.
trait Declared<F: Float>: Send {
    /// The config's own type.
    fn config_type(&self) -> TypeId;
    /// The type of the node it builds: the owner its params are addressed under.
    fn node_type(&self) -> TypeId;
    fn as_any(&self) -> &dyn Any;
    /// Whether `other`, a config of this one's type, makes the same node but
    /// for its values. `false` for a config of another type.
    fn same_structure(&self, other: &dyn Declared<F>) -> bool;
    fn initials(&self) -> Initials;
    /// The node, added as [`Graph::add`] adds one.
    fn add(&self, graph: &mut Graph<F>, host: &HostValues) -> NodeKey;
    /// The node at `node`, swapped for one built from this config.
    fn replace(
        &self,
        graph: &mut Graph<F>,
        host: &HostValues,
        node: NodeKey,
        fading: Fading,
    ) -> Result<(), ReplaceRefused>;
}

struct Held<C>(C);

impl<F: Float, C> Declared<F> for Held<C>
where
    C: NodeConfig<F>,
    C::Node: IntoNode<F, Controls: Send + 'static>,
{
    fn config_type(&self) -> TypeId {
        TypeId::of::<C>()
    }

    fn node_type(&self) -> TypeId {
        TypeId::of::<C::Node>()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn same_structure(&self, other: &dyn Declared<F>) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.0.same_structure(&other.0))
    }

    fn initials(&self) -> Initials {
        self.0.initials()
    }

    fn add(&self, graph: &mut Graph<F>, host: &HostValues) -> NodeKey {
        graph.add(self.0.build(host)).erase()
    }

    fn replace(
        &self,
        graph: &mut Graph<F>,
        host: &HostValues,
        node: NodeKey,
        fading: Fading,
    ) -> Result<(), ReplaceRefused> {
        let incoming = Replacement::new(self.0.build(host));
        let incoming = match fading {
            Fading::Declick => incoming,
            Fading::Over(fade) => incoming.fade(fade),
            Fading::Cut => incoming.fade(Fade::new(Samples::ZERO)),
        };
        graph.replace(node, incoming).map(drop)
    }
}

/// A declaration in a patch or kept from one.
struct Declaration<F: Float> {
    config: Box<dyn Declared<F>>,
    fading: Fading,
}

/// The nodes one `session.patch` call declares, in the order declared.
///
/// Built by [`collect`](Iterator::collect) or [`Extend`] over parts, or by
/// handing the parts to `session.patch` directly; it is itself a part.
#[must_use = "a patch does nothing until `Session::patch` takes it"]
pub struct Declarations<K, F: Float = f32> {
    nodes: Vec<(K, Declaration<F>)>,
}

impl<K, F: Float> Default for Declarations<K, F> {
    fn default() -> Self {
        Self { nodes: Vec::new() }
    }
}

impl<K: fmt::Debug, F: Float> fmt::Debug for Declarations<K, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.nodes.iter().map(|(key, _)| key))
            .finish()
    }
}

impl<K, F: Float, P: IntoPatch<K, F>> FromIterator<P> for Declarations<K, F> {
    fn from_iter<I: IntoIterator<Item = P>>(parts: I) -> Self {
        let mut patch = Self::default();
        patch.extend(parts);
        patch
    }
}

impl<K, F: Float, P: IntoPatch<K, F>> Extend<P> for Declarations<K, F> {
    fn extend<I: IntoIterator<Item = P>>(&mut self, parts: I) {
        for part in parts {
            part.add_to(self);
        }
    }
}

mod sealed {
    use super::Declarations;
    use tutti_buffers::Float;

    /// What adds a part to a patch.
    pub trait Sealed<K, F: Float> {
        fn add_to(self, patch: &mut Declarations<K, F>);
    }
}

/// A value that can be part of a patch: a [`NodeDecl`], a tuple of up to 16
/// parts, an array, `Vec` or `Option` of parts, or a [`Declarations`]. Sealed.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not part of a patch",
    label = "not a declaration or a container of declarations",
    note = "a patch is made of `NodeDecl::new(..)`, tuples, arrays, `Vec`s and `Option`s of parts, and `Declarations`",
    note = "collect an iterator of parts with `.collect::<Declarations<_>>()`",
    note = "in a tuple, every element must be a part"
)]
pub trait IntoPatch<K, F: Float = f32>: sealed::Sealed<K, F> {}

impl<K, F, C> sealed::Sealed<K, F> for NodeDecl<K, C>
where
    F: Float,
    C: NodeConfig<F>,
    C::Node: IntoNode<F, Controls: Send + 'static>,
{
    fn add_to(self, patch: &mut Declarations<K, F>) {
        patch.nodes.push((
            self.key,
            Declaration {
                config: Box::new(Held(self.config)),
                fading: self.fading,
            },
        ));
    }
}

impl<K, F, C> IntoPatch<K, F> for NodeDecl<K, C>
where
    F: Float,
    C: NodeConfig<F>,
    C::Node: IntoNode<F, Controls: Send + 'static>,
{
}

impl<K, F: Float> sealed::Sealed<K, F> for Declarations<K, F> {
    fn add_to(self, patch: &mut Declarations<K, F>) {
        patch.nodes.extend(self.nodes);
    }
}

impl<K, F: Float> IntoPatch<K, F> for Declarations<K, F> {}

/// The container impls: each adds its parts in order.
macro_rules! container {
    ($([$($g:tt)*] $ty:ty => |$this:ident, $patch:ident| $body:expr;)+) => {$(
        impl<K, F: Float, $($g)*> sealed::Sealed<K, F> for $ty {
            fn add_to($this, $patch: &mut Declarations<K, F>) {
                $body
            }
        }

        #[diagnostic::do_not_recommend]
        impl<K, F: Float, $($g)*> IntoPatch<K, F> for $ty {}
    )+};
}

/// A tuple of parts, each added in order.
macro_rules! tuple {
    ($(($($p:ident),+))+) => {$(
        #[allow(non_snake_case, reason = "each element is bound by its type's name")]
        impl<K, F: Float, $($p: IntoPatch<K, F>),+> sealed::Sealed<K, F> for ($($p,)+) {
            fn add_to(self, patch: &mut Declarations<K, F>) {
                let ($($p,)+) = self;
                $($p.add_to(patch);)+
            }
        }

        #[diagnostic::do_not_recommend]
        impl<K, F: Float, $($p: IntoPatch<K, F>),+> IntoPatch<K, F> for ($($p,)+) {}
    )+};
}

container! {
    [P: IntoPatch<K, F>, const N: usize] [P; N] => |self, patch| patch.extend(self);
    [P: IntoPatch<K, F>] Vec<P> => |self, patch| patch.extend(self);
    [P: IntoPatch<K, F>] Option<P> => |self, patch| patch.extend(self);
}

tuple! {
    (P1)
    (P1, P2)
    (P1, P2, P3)
    (P1, P2, P3, P4)
    (P1, P2, P3, P4, P5)
    (P1, P2, P3, P4, P5, P6)
    (P1, P2, P3, P4, P5, P6, P7)
    (P1, P2, P3, P4, P5, P6, P7, P8)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14, P15)
    (P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11, P12, P13, P14, P15, P16)
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
}

/// Why [`Session::key`](crate::Session::key) found no node.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum KeyError<K> {
    /// No patch declared the key.
    #[error("{0:?} is not declared")]
    Undeclared(K),
    /// The key is declared with a config of another type than the one asked.
    #[error("{0:?} is declared with a config of another type")]
    OtherConfig(K),
    /// The key is declared, but its node was removed by hand.
    #[error("{0:?} is declared, but its node was removed")]
    Gone(K),
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
struct Stored<F: Float> {
    node: NodeKey,
    declaration: Declaration<F>,
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

impl<K: AppKey, F: Float> Kept<K, F> {
    /// What the last patch declared under `key`.
    fn get(&self, key: &K) -> Option<&Stored<F>> {
        self.nodes.get(key)
    }

    /// Makes the session's graph match `patch`, in the order: nodes of the
    /// last patch it does not declare are removed, then each declared key is
    /// added, kept (its differing values written), swapped, or removed and
    /// added.
    pub(crate) fn reconcile<C: Clock<Float = F>>(
        &mut self,
        patch: Declarations<K, F>,
        session: &mut Session<K, C>,
        host: &HostValues,
    ) -> Result<Patched<K>, PatchError<K>> {
        let mut seen = BTreeSet::new();
        if let Some((key, _)) = patch.nodes.iter().find(|(key, _)| !seen.insert(key)) {
            return Err(PatchError::DuplicateKey(key.clone()));
        }
        let finite = |declaration: &Declaration<F>| {
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
        let mut patched = Patched {
            rebuilt: Vec::new(),
            unconverged: Vec::new(),
            removed: Vec::new(),
            readded: Vec::new(),
        };
        let gone: Vec<K> = self
            .nodes
            .keys()
            .filter(|key| !seen.contains(key))
            .cloned()
            .collect();
        drop(seen);
        for key in gone {
            let stored = self.nodes.remove(&key).expect("a key listed from the map");
            if take_out(session, &stored) {
                patched.removed.push(key);
            }
        }
        let mut writes = Vec::new();
        for (key, declaration) in patch.nodes {
            self.declare(key, declaration, session, host, &mut patched, &mut writes);
        }
        // One call, one gesture: every changed initial lands in one batch.
        session
            .values
            .set(writes)
            .expect("an initial of a config's own node is addressed, finite and not refused");
        Ok(patched)
    }

    /// Makes `key`'s node match `declaration`.
    fn declare<C: Clock<Float = F>>(
        &mut self,
        key: K,
        declaration: Declaration<F>,
        session: &mut Session<K, C>,
        host: &HostValues,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) {
        let Some(old) = self.nodes.get_mut(&key) else {
            let node = declaration.config.add(&mut session.graph, host);
            self.nodes.insert(key, Stored { node, declaration });
            return;
        };
        let new = &declaration.config;
        let same_type = old.declaration.config.config_type() == new.config_type();
        if !session.graph.contains(old.node) || !same_type {
            // The node is gone, or the config is another type: a new node
            // under the key. Taking out a node that is gone finds nothing.
            take_out(session, old);
            let node = new.add(&mut session.graph, host);
            patched.readded.push(key.clone());
            self.nodes.insert(key, Stored { node, declaration });
        } else if old.declaration.config.same_structure(&**new) {
            declared_writes(
                &session.graph,
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
            match new.replace(&mut session.graph, host, old.node, declaration.fading) {
                Ok(()) => {
                    patched.rebuilt.push(key);
                    old.declaration = declaration;
                }
                Err(refused) => patched.unconverged.push((key, refused)),
            }
        }
    }
}

/// Takes the node of `stored` out of the session over the fade its declaration
/// chose, and whether it was there.
fn take_out<K: AppKey, C: Clock>(session: &mut Session<K, C>, stored: &Stored<C::Float>) -> bool {
    let Ok(removal) = session.remove(stored.node) else {
        return false;
    };
    if stored.declaration.fading == Fading::Cut {
        let _ = removal.unfaded();
    }
    true
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
    /// the declaration says it must change ([`NodeDecl`] has the rules); a key
    /// no longer declared loses its node, over the fade its last declaration
    /// chose. [`Patched`] reports what happened to the keys that were
    /// declared before.
    ///
    /// A node that [`Graph::add`](crate::Graph::add) made is not declared: a
    /// patch never removes or changes it.
    ///
    /// # Errors
    ///
    /// [`PatchError::DuplicateKey`] when a key is declared twice in `patch`, and
    /// [`PatchError::NotFinite`] when a config gives an initial that is NaN or
    /// infinite; nothing changes.
    pub fn patch(
        &mut self,
        patch: impl IntoPatch<K, C::Float>,
    ) -> Result<Patched<K>, PatchError<K>> {
        let mut parts = Declarations::default();
        sealed::Sealed::add_to(patch, &mut parts);
        let host = self.host_values();
        let mut declared = std::mem::take(&mut self.declared);
        let patched = declared.reconcile(parts, self, &host);
        self.declared = declared;
        patched
    }

    /// The typed key of the node declared under `key` with a config of type
    /// `Cfg`, to set its values and read them.
    ///
    /// # Errors
    ///
    /// [`KeyError`]: `Undeclared`, `OtherConfig` (declared with a config of
    /// another type) or `Gone` (its node was removed by hand).
    pub fn key<Cfg: NodeConfig<C::Float>>(&self, key: &K) -> Result<Key<Cfg::Node>, KeyError<K>> {
        let Some(stored) = self.declared.get(key) else {
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
