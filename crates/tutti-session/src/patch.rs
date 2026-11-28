//! The declarations of a patch: the nodes an app declares by its own keys.
//!
//! - [`NodeDecl`] names one node: an app key and the config it is built from.
//! - [`InstrumentDecl`] names one played instrument: a node with its event queue
//!   and bus strip, whose keys [`Session::take_keys`](crate::Session::take_keys)
//!   lends.
//! - The wires between declared nodes are parts too, made by `connect` on a
//!   [`WireStart`](crate::WireStart); [`WireEnds`] names one.
//! - [`Section`] names a subtree of the patch by an app key and the value it is
//!   computed from; a patch whose value is equal to the last one's carries what
//!   the session made of it without evaluating it.
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
use tutti_units::{Amplitude, Samples};
#[cfg(feature = "plugin")]
use tutti_units::{Normalized, ParamId};

mod wire;

pub use wire::WireEnds;
pub(crate) use wire::{WireDecl, WireId};

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

/// One played instrument of a patch: the app's key for it, the config its node
/// is built from, and the event queue and bus strip the session makes beside
/// it.
///
/// The node is declared as a [`NodeDecl`] declares one, and has two parts of its
/// own that carry no app key: the queue its [keys](crate::Session::take_keys)
/// fill, wired to its event input, and a strip after its main output pair. A key
/// that changes between a `NodeDecl` and an `InstrumentDecl` removes the old node
/// and its parts and adds the new ones; another structure swaps the node and
/// keeps the parts, and other values are written. A patch without the key
/// removes all three.
///
/// A wire that names the key leaves from the strip: a [`Mention`](crate::Mention)
/// or `&InstrumentDecl` is the strip's main bus, and
/// [`event_input`](Self::event_input) is the instrument's own event input,
/// beside the queue. The instrument's audio is not an end: a wire into it is
/// refused ([`PatchError::Wire`](crate::PatchError::Wire)).
#[cfg(feature = "midi")]
#[must_use = "a declaration is part of a patch only once `Session::patch` takes it"]
pub struct InstrumentDecl<K, C> {
    key: K,
    config: C,
    fading: Fading,
    gain: Amplitude,
}

#[cfg(feature = "midi")]
impl<K, C> InstrumentDecl<K, C> {
    /// The instrument of `config` under `key`, its strip at unity, swapped and
    /// removed over the graph's declick.
    pub fn new(key: K, config: C) -> Self {
        Self {
            key,
            config,
            fading: Fading::default(),
            gain: Amplitude::UNITY,
        }
    }

    /// The key the instrument is declared under.
    #[must_use]
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The declaration this makes: the node's config held with the strip's
    /// volume and `build`, the target's way to build the node with its main
    /// pair.
    pub(crate) fn declaration<G>(self, build: fn(&C, &HostValues) -> Built) -> (K, Declaration<G>)
    where
        G: Place<C>,
        C: Send + 'static,
    {
        let held = Held {
            config: self.config,
            played: Some(Played {
                gain: self.gain,
                build,
            }),
        };
        let declaration = Declaration {
            body: Body::Node(Box::new(held)),
            fading: self.fading,
        };
        (self.key, declaration)
    }

    /// Swaps and removes the instrument at the next block boundary with no
    /// fade, as [`NodeDecl::unfaded`] does.
    pub fn unfaded(mut self) -> Self {
        self.fading = Fading::Cut;
        self
    }

    /// The strip's volume when the instrument is declared: a value, so a patch
    /// that declares another writes it to the running strip.
    pub fn initial_gain(mut self, gain: Amplitude) -> Self {
        self.gain = gain;
        self
    }
}

#[cfg(feature = "midi")]
impl<K: fmt::Debug, C> fmt::Debug for InstrumentDecl<K, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstrumentDecl")
            .field("key", &self.key)
            .field("fading", &self.fading)
            .field("gain", &self.gain)
            .finish_non_exhaustive()
    }
}

/// A subtree of a patch, named by an app key and computed from `deps`: while
/// the value is equal to the last patch's, the body is not run and its parts
/// are not compared, and the session keeps what it made of them.
///
/// The body is handed the deps, and is the only thing that makes the parts: what
/// it reads beside them has to be moved in, where it is seen. A handle the body
/// names (a [`Key`](crate::Key), a plugin) belongs in the deps: a re-applied
/// handle is another value, so deps that hold it are unequal.
///
/// A section is carried, its body not run, only when the last successful patch
/// ran or carried it under the same key with deps of the same type that compare
/// equal; its keys converged (no swap refused, no wire dormant) and its nodes and
/// wires are still in the graph; and every key its wires mention outside the
/// section is declared by the rest of the patch with the node it had (not
/// readded, not swapped). Otherwise the body runs, and what it declares joins
/// the patch like any other part. A body runs before the patch is checked, so
/// it may run for a patch that is then refused: it makes parts and does nothing
/// else.
///
/// Deps are compared by [`PartialEq`]: `Arc<T>` compares contents (short-cut by
/// pointer when `T: Eq`), so a revision number is the cheap form. Deps of
/// another type than the last patch's are unequal.
///
/// Section keys are a namespace apart from node keys; a key names at most one
/// section per patch ([`PatchError::DuplicateSection`](crate::PatchError)).
/// Sections nest: a carried section carries the ones inside it, and an
/// evaluated one judges each inner section by the same rule.
#[must_use = "a section is part of a patch only once `Session::patch` takes it"]
pub struct Section<K, D, B> {
    key: K,
    deps: D,
    body: B,
}

impl<K, D, B> Section<K, D, B> {
    /// The section named `key`, whose parts are `body` of `deps`.
    pub fn new<P>(key: K, deps: D, body: B) -> Self
    where
        D: PartialEq + Send + 'static,
        B: FnOnce(&D) -> P + Send + 'static,
    {
        Self { key, deps, body }
    }
}

impl<K: fmt::Debug, D, B> fmt::Debug for Section<K, D, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Section")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// The value a section is computed from, held erased: equal to another of its
/// own type when the values are.
pub(crate) trait Deps: Send {
    fn as_any(&self) -> &dyn Any;
    /// Whether `other` is a value of this one's type that compares equal.
    fn same(&self, other: &dyn Deps) -> bool;
}

impl<D: PartialEq + Send + 'static> Deps for D {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn same(&self, other: &dyn Deps) -> bool {
        other
            .as_any()
            .downcast_ref::<D>()
            .is_some_and(|other| self == other)
    }
}

/// A section of a patch not yet evaluated: its key, its deps, and the body that
/// makes its parts from them.
pub(crate) struct PendingSection<K, G: PatchTarget> {
    pub(crate) key: K,
    pub(crate) deps: Box<dyn Deps>,
    pub(crate) body: Box<dyn FnOnce(&dyn Deps) -> Declarations<K, G> + Send>,
}

/// Where declarations are placed: the session's [`Graph`](crate::Graph). What
/// a patch asks of it, so nothing here names the graph itself.
pub trait PatchTarget {
    /// Why a swap was refused.
    type Refused;

    /// What a plugin node is declared as on this target: the held plugin where
    /// the graph hosts plugins, and an empty type where it cannot.
    #[doc(hidden)]
    #[cfg(feature = "plugin")]
    type Plugin: HoldsPlugin + Send;
}

/// The float a declared graph runs at, and what a plugin node is declared as
/// at it: the held plugin for `f32`, whose formats are `f32` at their FFI, and
/// an empty type for `f64`, so an `f64` patch cannot hold a plugin.
#[doc(hidden)]
pub trait PatchFloat: tutti_buffers::Float {
    /// What a plugin node is declared as at this float.
    #[cfg(feature = "plugin")]
    type Plugin: HoldsPlugin + Send;
}

impl PatchFloat for f32 {
    #[cfg(feature = "plugin")]
    type Plugin = PluginHeld;
}

impl PatchFloat for f64 {
    #[cfg(feature = "plugin")]
    type Plugin = std::convert::Infallible;
}

/// A plugin as a declaration holds it. An empty type implements it for a graph
/// that hosts no plugin, so a plugin arm over that graph is never entered.
#[doc(hidden)]
#[cfg(feature = "plugin")]
pub trait HoldsPlugin {
    /// The held plugin.
    fn held(&self) -> &PluginHeld;
}

#[cfg(feature = "plugin")]
impl HoldsPlugin for PluginHeld {
    fn held(&self) -> &PluginHeld {
        self
    }
}

#[cfg(feature = "plugin")]
impl HoldsPlugin for std::convert::Infallible {
    fn held(&self) -> &PluginHeld {
        match *self {}
    }
}

/// A node built from a declaration and not yet placed: the target's own node
/// value, erased, and the shape its wires are checked against.
pub(crate) struct Built {
    pub(crate) node: Box<dyn Any + Send>,
    pub(crate) shape: Shape,
    /// The pair of outputs an instrument plays by; `None` for a plain node.
    pub(crate) main: Option<[u16; 2]>,
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
    /// The strip volume of an instrument declaration; `None` for a plain node.
    fn played(&self) -> Option<Amplitude>;
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

/// A config held as declared: a plain node's, or an instrument's with what
/// makes it one.
struct Held<C> {
    config: C,
    played: Option<Played<C>>,
}

/// An instrument declaration's part of a held config: its strip's volume, and
/// the target's way to build the node with its main pair.
#[cfg_attr(
    not(feature = "midi"),
    allow(
        dead_code,
        reason = "only an instrument declaration, behind `midi`, makes one"
    )
)]
struct Played<C> {
    gain: Amplitude,
    build: fn(&C, &HostValues) -> Built,
}

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
            .is_some_and(|other| G::same_structure(&self.config, &other.config))
    }

    fn initials(&self) -> Initials {
        G::initials(&self.config)
    }

    fn played(&self) -> Option<Amplitude> {
        self.played.as_ref().map(|played| played.gain)
    }

    fn build(&self, host: &HostValues) -> Built {
        match &self.played {
            Some(played) => (played.build)(&self.config, host),
            None => G::build(&self.config, host),
        }
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
    pub(crate) body: Body<G>,
    pub(crate) fading: Fading,
}

/// What a declaration is a node of. A `match` over it sits in the plan and the
/// commit of `Session::patch` alone.
pub(crate) enum Body<G: PatchTarget> {
    /// A node built from a value: built at the plan, placed at the commit, and
    /// built again by any number of plans.
    Node(Box<dyn Declared<G>>),
    /// A node bound over a loaded plugin: checked at the plan, installed at the
    /// commit, once.
    #[cfg(feature = "plugin")]
    Plugin(G::Plugin),
}

impl<G: PatchTarget> Body<G> {
    /// The config of a value-built node, `None` for any other body.
    pub(crate) fn node(&self) -> Option<&dyn Declared<G>> {
        match self {
            Self::Node(config) => Some(&**config),
            #[cfg(feature = "plugin")]
            Self::Plugin(_) => None,
        }
    }

    /// The loaded plugin of a plugin node, `None` for any other body.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin(&self) -> Option<&PluginHeld> {
        match self {
            Self::Node(_) => None,
            Self::Plugin(held) => Some(held.held()),
        }
    }
}

/// A loaded plugin as declared: the plugin, which is identified by its
/// instance, and the values its parameters are declared at.
#[cfg(feature = "plugin")]
#[doc(hidden)]
#[derive(Debug)]
pub struct PluginHeld {
    pub(crate) loaded: crate::LoadedPlugin,
    pub(crate) initials: Vec<(ParamId, Normalized)>,
}

/// One plugin node of a patch: the app's key for it and the loaded plugin it is
/// over.
///
/// The plugin is the node's identity. A patch that declares the key again with
/// the same [`LoadedPlugin`](crate::LoadedPlugin) (or a clone of it) keeps the node, its editor and
/// its record, and writes the [`initial`](Self::initial) values that changed;
/// with another plugin it swaps the node over, the old plugin closing, over the
/// graph's declick; with a config of another type it removes the node and adds
/// the new one. A patch without the key removes the node, and the plugin with
/// it: a plugin whose node was removed is spent, and is loaded again to be
/// declared again.
///
/// A plugin is declared under one key, and is taken by the first patch that adds
/// it. The ports of the node are the plugin's own: a change of them is the
/// plugin's, and the node at the key follows it.
#[cfg(feature = "plugin")]
#[must_use = "a declaration is part of a patch only once `Session::patch` takes it"]
pub struct PluginDecl<K> {
    key: K,
    loaded: crate::LoadedPlugin,
    initials: Vec<(ParamId, Normalized)>,
}

#[cfg(feature = "plugin")]
impl<K> PluginDecl<K> {
    /// The plugin `loaded` as the node under `key`, with no parameter declared.
    pub fn new(key: K, loaded: crate::LoadedPlugin) -> Self {
        Self {
            key,
            loaded,
            initials: Vec::new(),
        }
    }

    /// Declares the parameter `id` at `value`: written to the plugin when the
    /// node is added or swapped in, and again each time a patch declares another
    /// value for it. A patch that declares the same value writes nothing, so an
    /// edit made in the plugin's editor stands until the app declares a new
    /// value. Declaring `id` twice keeps the last value.
    ///
    /// An id the plugin does not list, or lists read-only, refuses the patch
    /// ([`PluginPatchError::NoSuchParam`](crate::PluginPatchError::NoSuchParam),
    /// [`ParamReadOnly`](crate::PluginPatchError::ParamReadOnly)). A plugin that
    /// stops listing a declared parameter after it loaded is not written to for
    /// it: that is counted
    /// ([`Diagnostics::unlisted_initials`](crate::Diagnostics::unlisted_initials)).
    pub fn initial(mut self, id: ParamId, value: Normalized) -> Self {
        self.initials.retain(|&(known, _)| known != id);
        self.initials.push((id, value));
        self
    }

    /// The declaration this makes.
    pub(crate) fn declaration<G: PatchTarget<Plugin = PluginHeld>>(self) -> (K, Declaration<G>) {
        let held = PluginHeld {
            loaded: self.loaded,
            initials: self.initials,
        };
        let declaration = Declaration {
            body: Body::Plugin(held),
            fading: Fading::Declick,
        };
        (self.key, declaration)
    }
}

#[cfg(feature = "plugin")]
impl<K: fmt::Debug> fmt::Debug for PluginDecl<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginDecl")
            .field("key", &self.key)
            .field("initials", &self.initials)
            .finish_non_exhaustive()
    }
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
    pub(crate) sections: Vec<PendingSection<K, G>>,
}

impl<K, G: PatchTarget> Default for Declarations<K, G> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            wires: Vec::new(),
            sections: Vec::new(),
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

/// A value that can be part of a patch: a [`NodeDecl`], a [`Section`], a tuple
/// of up to 16 parts, an array, `Vec` or `Option` of parts, or a
/// [`Declarations`]. Sealed.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not part of a patch",
    label = "not a declaration or a container of declarations",
    note = "a patch is made of `NodeDecl::new(..)`, `Section::new(..)`, tuples, arrays, `Vec`s and `Option`s of parts, and `Declarations`",
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
                body: Body::Node(Box::new(Held {
                    config: self.config,
                    played: None,
                })),
                fading: self.fading,
            },
        ));
    }
}

impl<K, G: PatchTarget> IntoPatch<K, G> for Declarations<K, G> {
    fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
        patch.nodes.extend(self.nodes);
        patch.wires.extend(self.wires);
        patch.sections.extend(self.sections);
    }
}

impl<K, G, D, B, P> IntoPatch<K, G> for Section<K, D, B>
where
    K: Send + 'static,
    G: PatchTarget + 'static,
    D: PartialEq + Send + 'static,
    B: FnOnce(&D) -> P + Send + 'static,
    P: IntoPatch<K, G>,
{
    fn add_to(self, patch: &mut Declarations<K, G>, _: Sealed) {
        let body = self.body;
        patch.sections.push(PendingSection {
            key: self.key,
            deps: Box::new(self.deps),
            body: Box::new(move |deps| {
                let deps = deps
                    .as_any()
                    .downcast_ref::<D>()
                    .expect("a section's body is handed the deps it was made with");
                Declarations::of(body(deps))
            }),
        });
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
