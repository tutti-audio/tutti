//! The declarations of a patch: the nodes an app declares by its own keys.
//!
//! - [`NodeDecl`] names one node: an app key and the config it is built from.
//! - The wires between declared nodes are parts too, made by `connect` on a
//!   [`WireStart`](crate::WireStart); [`WireEnds`] names one.
//! - [`Declarations`] is the declarations of one
//!   [`Session::patch`](crate::Session::patch) call, and [`IntoPatch`] is
//!   everything that can be part of one: a declaration, a tuple, array, `Vec`
//!   or `Option` of parts, or a `Declarations`.
//!
//! Nothing here knows the session: a declaration is placed in whatever
//! [`PatchTarget`] it is given, and the session reconciles the last patch's
//! declarations against the new ones.

use std::any::{Any, TypeId};
use std::fmt;

use thiserror::Error;
use tutti_graph::{Fade, HostValues, Initials, NodeKey, Shape};
use tutti_units::Samples;

mod wire;

pub(crate) use wire::WireDecl;
pub use wire::WireEnds;

/// How a declaration is swapped or removed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Fading {
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

    /// The key the node is declared under.
    #[must_use]
    pub fn key(&self) -> &K {
        &self.key
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

/// Where declarations are placed: the session's [`Graph`](crate::Graph). What
/// a patch asks of it, so nothing here names the graph itself.
pub trait PatchTarget {
    /// Why a swap was refused.
    type Refused;
}

/// A node built from a declaration and not yet placed: the target's own node
/// value, erased, and the shape its wires are checked against.
pub(crate) struct Built {
    pub(crate) node: Box<dyn Any + Send>,
    pub(crate) shape: Shape,
}

/// That a [`PatchTarget`] places nodes built from the config `C`, at the float the
/// target runs at, and reads `C` at that float.
pub(crate) trait Place<C>: PatchTarget {
    /// The type of the node `config` builds.
    fn node_type() -> TypeId;
    /// `config`'s initial values.
    fn initials(config: &C) -> Initials;
    /// Whether `a` and `b` make the same node but for their values.
    fn same_structure(a: &C, b: &C) -> bool;
    /// The node `config` builds, and its shape.
    fn build(config: &C, host: &HostValues) -> Built;
    /// `built`, a node of `build`, added unwired under `id`, a key no node
    /// holds.
    fn add(&mut self, id: NodeKey, built: Built);
    /// The node at `key`, swapped for `built` over `fade` (`None` is the
    /// target's default declick).
    fn swap(&mut self, key: NodeKey, built: Built, fade: Option<Fade>)
    -> Result<(), Self::Refused>;
}

/// A config held erased: what a reconcile asks of it, over its target.
pub(crate) trait Declared<G: PatchTarget>: Send {
    /// The config's own type.
    fn config_type(&self) -> TypeId;
    /// The type of the node it builds: the owner its params are addressed under.
    fn node_type(&self) -> TypeId;
    fn as_any(&self) -> &dyn Any;
    /// Whether `other`, a config of this one's type, makes the same node but
    /// for its values. `false` for a config of another type.
    fn same_structure(&self, other: &dyn Declared<G>) -> bool;
    fn initials(&self) -> Initials;
    /// The node this config makes, built once.
    fn build(&self, host: &HostValues) -> Built;
    /// `built`, this config's node, placed in `target` under `id`.
    fn add(&self, target: &mut G, id: NodeKey, built: Built);
    /// The node at `node`, swapped for `built`, this config's node.
    fn replace(
        &self,
        target: &mut G,
        built: Built,
        node: NodeKey,
        fading: Fading,
    ) -> Result<(), G::Refused>;
}

struct Held<C>(C);

impl<G, C> Declared<G> for Held<C>
where
    G: Place<C>,
    C: Send + 'static,
{
    fn config_type(&self) -> TypeId {
        TypeId::of::<C>()
    }

    fn node_type(&self) -> TypeId {
        G::node_type()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn same_structure(&self, other: &dyn Declared<G>) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| G::same_structure(&self.0, &other.0))
    }

    fn initials(&self) -> Initials {
        G::initials(&self.0)
    }

    fn build(&self, host: &HostValues) -> Built {
        G::build(&self.0, host)
    }

    fn add(&self, target: &mut G, id: NodeKey, built: Built) {
        target.add(id, built);
    }

    fn replace(
        &self,
        target: &mut G,
        built: Built,
        node: NodeKey,
        fading: Fading,
    ) -> Result<(), G::Refused> {
        let fade = match fading {
            Fading::Declick => None,
            Fading::Over(fade) => Some(fade),
            Fading::Cut => Some(Fade::new(Samples::ZERO)),
        };
        target.swap(node, built, fade)
    }
}

/// A declaration in a patch or kept from one.
pub(crate) struct Declaration<G: PatchTarget> {
    pub(crate) config: Box<dyn Declared<G>>,
    pub(crate) fading: Fading,
}

/// The nodes one `session.patch` call declares, in the order declared.
///
/// Built by [`collect`](Iterator::collect) or [`Extend`] over parts, or by
/// handing the parts to `session.patch` directly; it is itself a part. `G` is
/// the graph the nodes go to ([`Graph`](crate::Graph)).
#[must_use = "declarations do nothing until `Session::patch` takes them"]
pub struct Declarations<K, G: PatchTarget> {
    pub(crate) nodes: Vec<(K, Declaration<G>)>,
    pub(crate) wires: Vec<WireDecl<K>>,
}

impl<K, G: PatchTarget> Default for Declarations<K, G> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            wires: Vec::new(),
        }
    }
}

impl<K: fmt::Debug, G: PatchTarget> fmt::Debug for Declarations<K, G> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.nodes.iter().map(|(key, _)| key))
            .finish()
    }
}

impl<K, G: PatchTarget> Declarations<K, G> {
    /// The declarations `part` makes.
    pub(crate) fn of(part: impl IntoPatch<K, G>) -> Self {
        let mut declarations = Self::default();
        declarations.extend([part]);
        declarations
    }
}

impl<K, G: PatchTarget, P: IntoPatch<K, G>> FromIterator<P> for Declarations<K, G> {
    fn from_iter<I: IntoIterator<Item = P>>(parts: I) -> Self {
        let mut declarations = Self::default();
        declarations.extend(parts);
        declarations
    }
}

impl<K, G: PatchTarget, P: IntoPatch<K, G>> Extend<P> for Declarations<K, G> {
    fn extend<I: IntoIterator<Item = P>>(&mut self, parts: I) {
        for part in parts {
            part.add_to(self, Sealed(()));
        }
    }
}

/// Seals [`IntoPatch`]: only this crate makes one, so only it adds parts.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Sealed(());

/// A value that can be part of a patch: a [`NodeDecl`], a tuple of up to 16
/// parts, an array, `Vec` or `Option` of parts, or a [`Declarations`]. Sealed.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not part of a patch",
    label = "not a declaration or a container of declarations",
    note = "a patch is made of `NodeDecl::new(..)`, tuples, arrays, `Vec`s and `Option`s of parts, and `Declarations`",
    note = "collect an iterator of parts with `.collect::<Declarations<_, _>>()`",
    note = "in a tuple, every element must be a part"
)]
pub trait IntoPatch<K, G: PatchTarget> {
    /// Adds this part's declarations to `patch`.
    #[doc(hidden)]
    fn add_to(self, patch: &mut Declarations<K, G>, sealed: Sealed);
}

impl<K, G, C> IntoPatch<K, G> for NodeDecl<K, C>
where
    G: Place<C>,
    C: Send + 'static,
{
    fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
        patch.nodes.push((
            self.key,
            Declaration {
                config: Box::new(Held(self.config)),
                fading: self.fading,
            },
        ));
    }
}

impl<K, G: PatchTarget> IntoPatch<K, G> for Declarations<K, G> {
    fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
        patch.nodes.extend(self.nodes);
        patch.wires.extend(self.wires);
    }
}

/// The container impls: each adds its parts in order.
macro_rules! container {
    ($([$($g:tt)*] $ty:ty;)+) => {$(
        #[diagnostic::do_not_recommend]
        impl<K, G: PatchTarget, $($g)*> IntoPatch<K, G> for $ty {
            fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
                patch.extend(self);
            }
        }
    )+};
}

/// A tuple of parts, each added in order.
macro_rules! tuple {
    ($(($($p:ident),+))+) => {$(
        #[diagnostic::do_not_recommend]
        #[allow(non_snake_case, reason = "each element is bound by its type's name")]
        impl<K, G: PatchTarget, $($p: IntoPatch<K, G>),+> IntoPatch<K, G> for ($($p,)+) {
            fn add_to(self, patch: &mut Declarations<K, G>, sealed: Sealed) {
                let ($($p,)+) = self;
                $($p.add_to(patch, sealed);)+
            }
        }
    )+};
}

container! {
    [P: IntoPatch<K, G>, const N: usize] [P; N];
    [P: IntoPatch<K, G>] Vec<P>;
    [P: IntoPatch<K, G>] Option<P>;
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
