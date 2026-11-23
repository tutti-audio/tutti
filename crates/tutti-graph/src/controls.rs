//! A node's parameters, addressed by [`UnitAddr`]: [`ParamSet`], and the
//! fork of a node whose controls are one ([`ParamNode`], [`param_parts`]).
//!
//! # Why an address, beside the typed handles
//!
//! A node's own API hands out typed `Param<U>` handles (`cutoff: Param<Hz>`),
//! and a caller that knows the node's type uses those. A host that does not
//! (it names a param by its packed [`UnitAddr`] and reaches the node through its
//! graph key) needs the same cells by address.
//! A graph node's controls **are** its cells, shared with the running unit, so a write lands
//! on the next block without a ring and a fork reads the values it needs from
//! the controls rather than from a shadow copy of the unit.
//!
//! # Addresses: a kind and an index
//!
//! A param's address is a [`UnitAddr`]: a [`UnitParam`](tutti_units::UnitParam) kind and an index among
//! the node's params of that kind. A node with one param of a kind adds it at
//! index 0 (a bare [`UnitParam`](tutti_units::UnitParam) is that address); a
//! node with several (a threshold per band) adds each at its own index, with
//! the same [`ParamSetBuilder::param`]. Every method that takes an address
//! takes `impl Into<UnitAddr>`, so `set.set(UnitParam::Cutoff, v)` stands for
//! index 0 and `set.set(UnitAddr::new(UnitParam::Threshold, 1), v)` names
//! another cell. The duplicate check and every lookup are on the whole
//! address: the same kind at another index is another param.
//!
//! # One value per param
//!
//! Each param is one cell: the knob. The node reads it once per block, and
//! the host's [`set`](ParamSet::set) is its one writer. A modulation lane
//! does not write it (it sends `knob + Σ offsets` into the param's event
//! port instead), so the cell holds what the user set, and a fork of the
//! node starts from it ([`ParamFork`]) without a second, "authored" copy.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_rt::ParamCell;
use tutti_units::UnitAddr;
#[cfg(test)]
use tutti_units::UnitParam;

use crate::RenderMode;
use crate::fork::{ForkBuild, ForkCause, ForkSource, Forked};
use tutti_buffers::Float;

use crate::declare::ParamValue;
use crate::node::{IntoNode, Node, NodeParts};

/// One param: its address, the cell the node reads, and the unit the node
/// declares it in.
struct Slot {
    param: UnitAddr,
    live: ParamCell,
    unit: DeclaredUnit,
}

/// The unit a param's value is in, erased: how a host that reaches a param by
/// address names a cell's `f64` as the value of the unit the node declares it
/// in.
#[derive(Clone, Copy)]
pub struct DeclaredUnit {
    value: fn(f64) -> Arc<dyn Any + Send + Sync>,
}

impl DeclaredUnit {
    /// The unit `U`.
    #[must_use]
    pub const fn of<U: ParamValue>() -> Self {
        Self {
            value: |cell| Arc::new(U::from_cell(cell)),
        }
    }

    /// The unit's value for a cell's `f64`: a `U` behind the pointer.
    #[must_use]
    pub fn value(self, cell: f64) -> Arc<dyn Any + Send + Sync> {
        (self.value)(cell)
    }
}

impl std::fmt::Debug for DeclaredUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredUnit").finish_non_exhaustive()
    }
}

/// A node's params by [`UnitAddr`]: the cells the running node reads, once
/// per block, and a fork of it starts from.
///
/// `Clone` shares: every clone addresses the same cells. Control-thread type;
/// the node itself reads its own `Param<U>` fields, never this.
#[derive(Clone)]
pub struct ParamSet {
    slots: Arc<[Slot]>,
}

impl ParamSet {
    /// A set with no params (a node with nothing to control).
    pub fn empty() -> Self {
        Self {
            slots: Arc::from(Vec::new()),
        }
    }

    /// Starts a set; add each param with [`ParamSetBuilder::param`].
    pub fn builder() -> ParamSetBuilder {
        ParamSetBuilder { slots: Vec::new() }
    }

    fn slot(&self, param: UnitAddr) -> Option<&Slot> {
        self.slots.iter().find(|s| s.param == param)
    }

    /// Sets `param` to `value`: the cell the node reads on its next block,
    /// and a fork taken after this starts from. `false` if the node has no
    /// such param; nothing is written then.
    pub fn set(&self, param: impl Into<UnitAddr>, value: f64) -> bool {
        let Some(slot) = self.slot(param.into()) else {
            return false;
        };
        slot.live.store(value, Ordering::Release);
        true
    }

    /// `param`'s live value: what the node reads on its next block.
    pub fn get(&self, param: impl Into<UnitAddr>) -> Option<f64> {
        self.slot(param.into())
            .map(|s| s.live.load(Ordering::Acquire))
    }

    /// `param`'s live cell, for a reader or writer that holds it directly (a
    /// modulation lane, whose knob it is). Clones the `Arc`: control
    /// thread only.
    ///
    /// A value through the set is an `f64`, the raw of every unit ([`ParamCell`]).
    pub fn cell(&self, param: impl Into<UnitAddr>) -> Option<ParamCell> {
        self.slot(param.into()).map(|s| s.live.clone())
    }

    /// The unit `param` is declared in, or `None` for an address not in the set.
    pub fn unit(&self, param: impl Into<UnitAddr>) -> Option<DeclaredUnit> {
        self.slot(param.into()).map(|s| s.unit)
    }

    /// The params, in the order they were added.
    pub fn params(&self) -> impl Iterator<Item = UnitAddr> + '_ {
        self.slots.iter().map(|s| s.param)
    }
}

impl std::fmt::Debug for ParamSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.slots
                    .iter()
                    .map(|s| (s.param, s.live.load(Ordering::Relaxed))),
            )
            .finish()
    }
}

/// Builds a [`ParamSet`] over a node's cells. See [`ParamSet::builder`].
pub struct ParamSetBuilder {
    slots: Vec<Slot>,
}

impl std::fmt::Debug for ParamSetBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParamSetBuilder").finish_non_exhaustive()
    }
}

impl ParamSetBuilder {
    /// Address `cell` (a `Param<U>`'s, `Param::as_atomic`), whose value is in
    /// `unit`, as `param`: a [`UnitAddr`], or a bare `UnitParam` for index 0. A node with several
    /// params of a kind adds each at its own index.
    ///
    /// # Panics
    ///
    /// If that address (kind **and** index) was already added: one address,
    /// one cell. The same kind at another index is a different address.
    #[must_use]
    pub fn param(
        mut self,
        unit: DeclaredUnit,
        param: impl Into<UnitAddr>,
        cell: impl Into<ParamCell>,
    ) -> Self {
        let param = param.into();
        let cell = cell.into();
        assert!(
            self.slots.iter().all(|s| s.param != param),
            "{param:?} added to a ParamSet twice"
        );
        self.slots.push(Slot {
            param,
            live: cell,
            unit,
        });
        self
    }

    /// The set.
    #[must_use]
    pub fn build(self) -> ParamSet {
        ParamSet {
            slots: Arc::from(self.slots),
        }
    }
}

/// A node whose controls are its `Param<U>` cells, addressed by a
/// [`ParamSet`]: what [`param_parts`] inserts, with a fork that shares
/// nothing and starts from the values its cells hold.
///
/// A graph node reads time from its block's [`Env`](crate::Env), so
/// [`fork_fresh`](Self::fork_fresh) is the whole copy: the fork reads its
/// render's `Env`.
pub trait ParamNode<F: Float = f32>: Node<F> + Sized {
    /// A [`ParamSet`] over this node's cells (every param a host may set by
    /// address). Control thread; may allocate.
    fn param_set(&self) -> ParamSet;

    /// A copy of this node that shares **nothing** with it — its own
    /// `Param` cells (at the values this node's hold now), its own state —
    /// returned to the state of a freshly prepared node. What a fork runs.
    #[must_use]
    fn fork_fresh(&self) -> Self;
}

/// The fork source [`param_parts`] hands the editor: a template of the node
/// that shares its cells (so a fork reads them as they are when it is
/// taken), and its [`ParamSet`].
#[derive(Debug)]
pub struct ParamFork<N> {
    template: N,
    params: ParamSet,
}

impl<N> ParamFork<N> {
    /// The source for `node`: a template clone that shares its cells.
    pub fn new<F: Float>(node: &N) -> Self
    where
        N: ParamNode<F> + Clone,
    {
        Self {
            template: node.clone(),
            params: node.param_set(),
        }
    }

    /// The set over the cells a fork starts from: the controls
    /// [`param_parts`] hands the caller.
    pub fn params(&self) -> &ParamSet {
        &self.params
    }

    /// The fork of the node as it stands, as a value: what
    /// [`ForkSource::fork`] boxes. Its params are at the values the shared
    /// cells hold now ([`ParamNode::fork_fresh`] copies them).
    pub fn fork_node<F: Float>(&self) -> N
    where
        N: ParamNode<F>,
    {
        self.template.fork_fresh()
    }
}

impl<F: Float, N: ParamNode<F>> ForkSource<F> for ParamFork<N> {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        Ok(Forked::new(Box::new(self.fork_node())))
    }

    /// The cells are read here, at the gather: a param set after
    /// it does not reach the fork. The fork's cells are its own, so it can
    /// be prepared on any thread.
    fn gather(&self, mode: RenderMode) -> Result<ForkBuild<F>, ForkCause> {
        self.fork(mode).map(ForkBuild::ready)
    }
}

/// `node` split for insertion as a [`ParamNode`]: the node, its
/// [`ParamSet`] as its controls, and a [`ParamFork`] as its fork source.
/// What a `ParamNode`'s [`IntoNode::into_parts`] returns:
///
/// ```ignore
/// impl IntoNode for MyFilter {
///     type Controls = ParamSet;
///     fn into_parts(self) -> NodeParts<ParamSet> {
///         tutti_graph::param_parts(self)
///     }
/// }
/// ```
///
/// `N: Clone` is the template: a clone that **shares** the node's cells (as
/// `Param::clone` does), so a fork taken later reads the cells as they are
/// then. A `Clone` that detached them would fork the values at insert.
pub fn param_parts<F: Float, N: ParamNode<F> + Clone>(node: N) -> NodeParts<ParamSet, F> {
    let fork = ParamFork::new(&node);
    NodeParts::new(Box::new(node), fork.params.clone(), Some(Box::new(fork)))
}

/// A [`ParamNode`] inserted as one: the wrapper that makes `Params(node)` an
/// [`IntoNode`] whose controls are the node's [`ParamSet`] and
/// whose fork starts from the values set through it ([`param_parts`]), beside
/// [`ForkByClone`](crate::ForkByClone) and [`Unforkable`](crate::Unforkable)
/// for a node with no params. A host's own param'd node needs no hand-written
/// `IntoNode`:
///
/// ```ignore
/// let key = session.graph().add(Params(MyFilter::new()));
/// ```
///
/// The crate's guide ("Writing a node", "A node with params") has a whole
/// param'd node as a running example.
///
/// Its params are declared once, next to its cells, with
/// [`declare_params!`](crate::declare_params): the macro writes the
/// [`ParamSet`], the shape's ports and the typed [`HasParam`](crate::HasParam)
/// impls a session key's methods come from.
///
/// **A node with params is inserted this way and no other.** `Clone` means
/// two things here: [`ForkByClone`](crate::ForkByClone) promises the clone
/// shares nothing, and `param_parts` takes a clone that *shares* the cells.
/// `ForkByClone(param_node)` compiles (Rust has no way to refuse a type that
/// implements a trait) and forks onto the live cell, so a param set after
/// the fork reaches it; `Params` is the one that forks from the values.
/// No bound on `ForkByClone` can refuse it: that needs a negative one (`N`
/// declares no params), which stable Rust has no way to write, and a marker
/// the declaration implements cannot be tested for its absence. The rule is
/// documented, not checked.
#[derive(Clone, Debug)]
pub struct Params<N>(pub N);

impl<F: Float, N: ParamNode<F> + Clone> IntoNode<F> for Params<N> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        param_parts(self.0)
    }

    /// The node's own name: the wrapper only says how it is inserted.
    fn kind() -> &'static str {
        std::any::type_name::<N>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_rt::Param;
    use tutti_units::{Hz, Q};

    fn set() -> (Param<Hz>, Param<Q>, ParamSet) {
        let cutoff = Param::new(Hz::new(1000.0));
        let q = Param::new(Q::new(0.7));
        let set = ParamSet::builder()
            .param(
                DeclaredUnit::of::<Hz>(),
                UnitParam::Cutoff,
                cutoff.as_atomic(),
            )
            .param(DeclaredUnit::of::<Q>(), UnitParam::Q, q.as_atomic())
            .build();
        (cutoff, q, set)
    }

    /// `set` reaches the node's own cell, and `get` reads it back.
    #[test]
    fn set_writes_the_cell_the_node_reads() {
        let (cutoff, _q, set) = set();
        assert!(set.set(UnitParam::Cutoff, 250.0));
        assert_eq!(cutoff.load(), Hz::new(250.0));
        assert_eq!(set.get(UnitParam::Cutoff), Some(250.0));
    }

    /// An address the node does not have is refused, and writes nothing.
    #[test]
    fn an_unknown_param_is_refused() {
        let (cutoff, q, set) = set();
        assert!(!set.set(UnitParam::GainDb, 3.0));
        assert_eq!(set.get(UnitParam::GainDb), None);
        assert_eq!((cutoff.load(), q.load()), (Hz::new(1000.0), Q::new(0.7)));
    }

    /// Adding one address twice is a bug in the node, caught at build.
    #[test]
    #[should_panic(expected = "twice")]
    fn one_address_one_cell() {
        let a = Param::new(Hz::new(1.0));
        let _ = ParamSet::builder()
            .param(DeclaredUnit::of::<Hz>(), UnitParam::Cutoff, a.as_atomic())
            .param(DeclaredUnit::of::<Hz>(), UnitParam::Cutoff, a.as_atomic());
    }
}
