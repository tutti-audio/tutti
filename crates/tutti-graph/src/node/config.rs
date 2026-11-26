//! What a node is made from: [`NodeConfig`], and [`Asset`], the handle a config
//! holds a shared resource by.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use tutti_buffers::Float;
use tutti_units::{SettingKind, UnitAddr};

use crate::node::IntoNode;

/// A node's config: plain data from which the node is built, and by whose
/// difference a session decides whether a running node is kept.
///
/// [`declare_params!`](crate::declare_params)'s cells form writes it for its
/// config; a hand-written config states the same three items.
///
/// A config is compared by its structure, not its values. A field that decides
/// what the node is (the `required` and `options` of the cells form, each
/// `PartialEq`) is structure: a change builds a new node. A declared param is a
/// value: a change is a write to the running node, which keeps its state.
pub trait NodeConfig<F: Float = f32>: Clone + Send + 'static {
    /// What the config makes.
    type Node: IntoNode<F> + Send + 'static;

    /// The node, its params at this config's initial values. Takes `&self`: the
    /// caller keeps the config as its record of what it asked for.
    fn build(&self) -> Self::Node;

    /// Whether `other` makes the same node but for its declared param values:
    /// `false` means a new node, `true` means the running node stays and the
    /// differing [`initials`](Self::initials) are written to it.
    fn same_structure(&self, other: &Self) -> bool;

    /// The initial value of each declared param and kinded setting the config
    /// gives. Read from the config's fields, never by building the node.
    fn initials(&self) -> Initials;
}

/// The initial values a config gives, by address: each declared param's raw in its
/// unit, and each setting that has a [`SettingKind`] as its encoded word.
///
/// A param left out has no value (an `Option` param given none). A setting with
/// no kind has no address, so it is not listed.
///
/// Listed in declaration order, and two lists of one config type compare in that
/// order: a hand-written [`NodeConfig::initials`] lists its params and settings in
/// one fixed order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Initials {
    params: Vec<(UnitAddr, f64)>,
    settings: Vec<(SettingKind, u64)>,
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
