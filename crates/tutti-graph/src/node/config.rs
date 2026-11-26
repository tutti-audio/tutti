//! What a node is made from: [`NodeConfig`], and [`Asset`], the handle a config
//! holds a shared resource by.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use tutti_buffers::Float;
use tutti_units::{SettingKind, UnitAddr};

use crate::exec::published::PublishedKind;

use crate::node::IntoNode;

/// A node's config: plain data from which the node is built, and by whose
/// difference a session decides whether a running node is kept.
///
/// [`declare_params!`](crate::declare_params)'s cells form writes it for its
/// config; a hand-written config states the same three items.
///
/// A config is compared by its structure, not its values. A field that decides
/// what the node is (the `required` and `options` of the cells form, each
/// `PartialEq`) is structure: a change builds a new node. A setting with no
/// [`SettingKind`] has no address, so nothing can write a changed initial to a
/// running node: it is structure too. A declared param, a kinded setting and a
/// published value are values: a change is a write to the running node, which
/// keeps its state.
pub trait NodeConfig<F: Float = f32>: Clone + Send + 'static {
    /// What the config makes.
    type Node: IntoNode<F> + Send + 'static;

    /// The node, its params at this config's initial values. Takes `&self`: the
    /// caller keeps the config as its record of what it asked for. `host` holds
    /// the values the host keeps once for many nodes; a node that reads one
    /// registers its [`Reader`](tutti_rt::Reader) here (see [`HostValues`]).
    /// The cells form's `build` has no use for it.
    fn build(&self, host: &HostValues) -> Self::Node;

    /// Whether `other` makes the same node but for its values (params and kinded settings):
    /// `false` means a new node, `true` means the running node stays and the
    /// differing [`initials`](Self::initials) are written to it.
    fn same_structure(&self, other: &Self) -> bool;

    /// The initial value of each declared param, kinded setting and published
    /// value the config gives. Read from the config's fields, never by building the node.
    fn initials(&self) -> Initials;
}

/// The initial values a config gives, by address: each declared param's raw in its
/// unit, each setting that has a [`SettingKind`] as its encoded word, and each
/// published value by its [`PublishedKind`].
///
/// A param left out has no value (an `Option` param given none). A setting with
/// no kind has no address, so it is not listed: it is structure, compared by
/// [`NodeConfig::same_structure`].
///
/// Listed in declaration order, and two lists of one config type compare in that
/// order: a hand-written [`NodeConfig::initials`] lists its params and settings in
/// one fixed order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Initials {
    params: Vec<(UnitAddr, f64)>,
    settings: Vec<(SettingKind, u64)>,
    published: Vec<PublishedInitial>,
}

/// One published value a config gives, erased: its kind and the value, which
/// a session writes to the running node's published target of that kind.
///
/// Two are equal when they are of one kind and their values are equal by the
/// value's own `PartialEq`, which is what a published cell compares by
/// ([`Published::replace`](crate::Published::replace)); a value that is an
/// `Arc` of a large resource is cheap to compare when its `PartialEq` starts
/// at identity.
#[derive(Clone)]
pub struct PublishedInitial {
    kind: TypeId,
    kind_name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
    same: fn(&dyn Any, &dyn Any) -> bool,
}

impl PublishedInitial {
    fn new<K: PublishedKind>(value: K::Value) -> Self {
        Self {
            kind: TypeId::of::<K>(),
            kind_name: type_name::<K>(),
            value: Arc::new(value),
            same: |a, b| match (a.downcast_ref::<K::Value>(), b.downcast_ref::<K::Value>()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }

    /// The value, when this is of the kind `K`.
    #[must_use]
    pub fn value<K: PublishedKind>(&self) -> Option<K::Value> {
        if self.kind != TypeId::of::<K>() {
            return None;
        }
        self.value.downcast_ref::<K::Value>().cloned()
    }
}

impl PartialEq for PublishedInitial {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && (self.same)(&*self.value, &*other.value)
    }
}

impl fmt::Debug for PublishedInitial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishedInitial")
            .field("kind", &self.kind_name)
            .finish_non_exhaustive()
    }
}

impl Initials {
    /// No initial values: a config adds its own with [`param`](Self::param) and
    /// [`setting`](Self::setting).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also `raw`, in the unit's raw, as the initial value of the param at `addr`.
    #[must_use]
    pub fn param(mut self, addr: UnitAddr, raw: f64) -> Self {
        self.params.push((addr, raw));
        self
    }

    /// Also `word` ([`AtomicWord::to_word`](tutti_rt::AtomicWord::to_word) of the
    /// value) as the initial value of the setting of `kind`.
    #[must_use]
    pub fn setting(mut self, kind: SettingKind, word: u64) -> Self {
        self.settings.push((kind, word));
        self
    }

    /// Also `value` as the initial value of the published value of the kind `K`.
    #[must_use]
    pub fn published<K: PublishedKind>(mut self, value: K::Value) -> Self {
        self.published.push(PublishedInitial::new::<K>(value));
        self
    }

    /// The published values' initials, in declaration order.
    #[must_use]
    pub fn published_values(&self) -> &[PublishedInitial] {
        &self.published
    }

    /// The params' initial raws, in declaration order.
    #[must_use]
    pub fn params(&self) -> &[(UnitAddr, f64)] {
        &self.params
    }

    /// The settings' initial words ([`AtomicWord::to_word`](tutti_rt::AtomicWord::to_word)),
    /// in declaration order.
    #[must_use]
    pub fn settings(&self) -> &[(SettingKind, u64)] {
        &self.settings
    }
}

/// The values a host holds once for many nodes, as a set of its publish cells
/// keyed by the value's type.
///
/// A value of this kind reaches a node as a [`Reader`](tutti_rt::Reader)
/// registered at insertion: [`NodeConfig::build`] takes the reader from here
/// where the node is built, stores it in the node, and lends it once per
/// block. The node has no setter for the value; the host publishes it.
///
/// It is keyed by type, so the graph names no host's domain. A type the host
/// did not [`provide`](Self::provide) has no reader: the node then uses one
/// constant default, which its own doc states. A graph with no host (a test, a
/// bench) builds with [`HostValues::new`].
#[derive(Clone, Default)]
pub struct HostValues {
    cells: HashMap<TypeId, (&'static str, Arc<dyn Any + Send + Sync>)>,
}

impl HostValues {
    /// Nothing provided.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also `cell` as the host's value of type `T`, replacing one provided
    /// before for `T`. The cell is shared, not copied: what the host publishes
    /// on it is what every reader registered from this set lends.
    #[must_use]
    pub fn provide<T: Send + Sync + 'static>(mut self, cell: &tutti_rt::Publisher<T>) -> Self {
        let cell: Arc<dyn Any + Send + Sync> = Arc::new(cell.clone());
        self.cells
            .insert(TypeId::of::<T>(), (type_name::<T>(), cell));
        self
    }

    /// A new reader of the host's value of type `T`, or `None` when the host
    /// provides none.
    ///
    /// Control side: registered where the node is built, stored in the node,
    /// lent once per block.
    #[must_use]
    pub fn reader<T: Send + Sync + 'static>(&self) -> Option<tutti_rt::Reader<T>> {
        let (_, cell) = self.cells.get(&TypeId::of::<T>())?;
        cell.downcast_ref::<tutti_rt::Publisher<T>>()
            .map(tutti_rt::Publisher::reader)
    }
}

impl fmt::Debug for HostValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.cells.values().map(|(name, _)| *name).collect();
        names.sort_unstable();
        f.debug_tuple("HostValues").field(&names).finish()
    }
}

/// A shared resource a config holds (a wavetable, a wave, an impulse response),
/// equal to another only when it is the same one.
///
/// Two assets are equal when they share one allocation (`Arc::ptr_eq`), and a
/// clone is the same asset. Content equality would cost a compare of the whole
/// resource on every patch, and a resource loaded again is a new asset whose
/// equal content is no reason to keep the node built from the old one.
///
/// It dereferences to the resource. It is made from an `Arc` (`From`), which keeps
/// that allocation's identity, or by [`Asset::new`], which is a new identity.
pub struct Asset<T>(Arc<T>);

impl<T> Asset<T> {
    /// `value` as a new asset, equal to no other: two assets made from equal values
    /// are two identities, so a config given each makes a new node.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// The shared resource, for an algorithm that takes an `Arc`.
    #[must_use]
    pub fn arc(&self) -> &Arc<T> {
        &self.0
    }
}

impl<T> Clone for Asset<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> PartialEq for Asset<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Eq for Asset<T> {}

impl<T: fmt::Debug> fmt::Debug for Asset<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Asset").field(&self.0).finish()
    }
}

impl<T> Deref for Asset<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> From<Arc<T>> for Asset<T> {
    fn from(arc: Arc<T>) -> Self {
        Self(arc)
    }
}
