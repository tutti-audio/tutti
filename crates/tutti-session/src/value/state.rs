//! The session's value state: the value lock and the table of what each node
//! can be written through, shared by the session, its graph and every
//! [`Knobs`].

use std::any::TypeId;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tutti_graph::{NodeKey, ParamSet};
use tutti_rt::LockExt;
use tutti_rt::ParamCell;
use tutti_units::UnitAddr;

use super::batch::{Batch, Fail, IntoAssign, IntoRead, Readable, Stageable};
use super::error::{ValueError, ValueErrorKind};
use super::slot::{self, NodeSlots, SlotCell, SlotId};
use super::target::AnyTarget;
use crate::AppKey;

/// What one node's params are written through, and what they were declared at.
#[derive(Debug)]
struct NodeParams {
    /// The node type the params were addressed as (`Graph::add::<N>`): the
    /// type a typed key names.
    owner: TypeId,
    set: ParamSet,
    /// Each param's value when the node was addressed: the declaration's seed.
    seeds: Vec<(UnitAddr, f64)>,
}

/// The key-to-cell table, behind the value lock.
#[derive(Default, Debug)]
pub struct Params {
    nodes: BTreeMap<NodeKey, NodeParams>,
    /// The session's own values, by what they are: each a
    /// [`SlotCell`] of its value type.
    pub(super) slots: BTreeMap<SlotId, slot::Erased>,
}

impl Params {
    /// `addr`'s cell on `node`, when the node is of the type `owner` names.
    pub fn cell(&self, node: NodeKey, addr: UnitAddr, owner: TypeId) -> Option<ParamCell> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        node.set.cell(addr)
    }

    /// `addr`'s value on `node` when the node was last addressed, when the
    /// node is of the type `owner` names.
    pub fn seed(&self, node: NodeKey, addr: UnitAddr, owner: TypeId) -> Option<f64> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        node.seeds.iter().find(|(a, _)| *a == addr).map(|(_, v)| *v)
    }
}

thread_local! {
    /// The value locks this thread holds, by address: what a call checks
    /// before it takes one, so a value call from inside a `modify` is refused
    /// where it would deadlock.
    static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The value lock and the table behind it. Control side: the audio thread
/// never touches either.
#[derive(Default)]
pub struct ValueTable {
    params: Mutex<Params>,
    /// What the output policy is checked against, which the graph publishes.
    /// A lock of its own: a check reads it under the value lock.
    output: crate::policy::FactsCell,
}

/// The value lock, held; released and forgotten by this thread when dropped.
pub struct Held<'a> {
    guard: MutexGuard<'a, Params>,
    token: usize,
}

impl Held<'_> {
    pub(crate) fn params(&self) -> &Params {
        &self.guard
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(at) = held.iter().rposition(|t| *t == self.token) {
                held.remove(at);
            }
        });
    }
}

impl ValueTable {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes the value lock, or `None` when this thread already holds it.
    pub(crate) fn enter(&self) -> Option<Held<'_>> {
        let token = std::ptr::from_ref(self) as usize;
        let reentrant = HELD.with(|held| held.borrow().contains(&token));
        if reentrant {
            return None;
        }
        let guard = self.params.lock_recover();
        HELD.with(|held| held.borrow_mut().push(token));
        Some(Held { guard, token })
    }

    /// The cell the graph publishes the output device and root to.
    pub(crate) fn output_facts(&self) -> crate::policy::FactsCell {
        Arc::clone(&self.output)
    }

    /// Addresses `node`'s params, replacing any it had. Control thread.
    pub(crate) fn address(&self, node: NodeKey, owner: TypeId, set: ParamSet) {
        let seeds = set
            .params()
            .filter_map(|addr| {
                Some((
                    addr,
                    set.cell(addr)?.load(std::sync::atomic::Ordering::Acquire),
                ))
            })
            .collect();
        let old = self
            .params
            .lock_recover()
            .nodes
            .insert(node, NodeParams { owner, set, seeds });
        drop(old);
    }

    /// Registers the session's own value `id`, held in `slot`. Once, as the
    /// session is built.
    pub(crate) fn provide<V: 'static>(&self, id: SlotId, slot: SlotCell<V>) {
        self.params
            .lock_recover()
            .slots
            .insert(id, slot::erase(slot));
    }

    /// Registers the values of a node, from its [`NodeSlots`]. Control
    /// thread.
    pub(crate) fn provide_node(&self, slots: NodeSlots) {
        self.params.lock_recover().slots.extend(slots.0);
    }

    /// Writes `assign` as [`Session::set`](crate::Session::set) does, for the
    /// session's own wiring (a clip `Midi` plays).
    #[cfg(feature = "midi")]
    pub(crate) fn write<A: Stageable>(&self, assign: A) -> Result<(), ValueErrorKind> {
        let held = self.enter().ok_or(ValueErrorKind::Reentrant)?;
        let mut batch = Batch::new(held.params());
        assign.stage(&mut batch).map_err(|fail| fail.kind)?;
        batch.land();
        Ok(())
    }

    /// Forgets `node`'s params and slots: its targets are gone.
    pub(crate) fn retire(&self, node: NodeKey) {
        let (gone, slots) = {
            let mut params = self.params.lock_recover();
            let gone = params.nodes.remove(&node);
            let slots: Vec<_> = params
                .slots
                .extract_if(.., |id, _| id.node() == Some(node))
                .collect();
            (gone, slots)
        };
        drop((gone, slots));
    }

    /// `addr`'s cell on `node`, whatever its type, for the graph's own use.
    pub(crate) fn cell(&self, node: NodeKey, addr: UnitAddr) -> Option<ParamCell> {
        self.params.lock_recover().nodes.get(&node)?.set.cell(addr)
    }
}

/// The session's value state, one `Arc` independent of the clock: a clock
/// switch moves it and never rebuilds it. [`Knobs`] hold it weakly, so the
/// session's owners are the state's owners.
pub struct ValueState<K> {
    table: Arc<ValueTable>,
    key: PhantomData<fn() -> K>,
}

impl<K> fmt::Debug for ValueState<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValueState").finish_non_exhaustive()
    }
}

fn error<K>(fail: Fail) -> ValueError<K> {
    ValueError {
        at: fail.at,
        target: fail.target.map(AnyTarget::of),
        kind: fail.kind,
    }
}

fn whole_call<K>(kind: ValueErrorKind) -> ValueError<K> {
    ValueError {
        at: 0,
        target: None,
        kind,
    }
}

impl<K: AppKey> ValueState<K> {
    pub(crate) fn new(table: Arc<ValueTable>) -> Arc<Self> {
        Arc::new(Self {
            table,
            key: PhantomData,
        })
    }

    /// Writes `assign`: every item staged under the value lock, then every
    /// target's final value stored. An item that fails lands nothing.
    pub(crate) fn set<A: Stageable>(&self, assign: A) -> Result<(), ValueError<K>> {
        let held = self
            .table
            .enter()
            .ok_or_else(|| whole_call(ValueErrorKind::Reentrant))?;
        let mut batch = Batch::new(held.params());
        assign.stage(&mut batch).map_err(error)?;
        batch.land();
        Ok(())
    }

    /// Reads `read`, in its own shape, under the value lock.
    pub(crate) fn get<R: Readable>(&self, read: R) -> Result<R::Output, ValueError<K>> {
        let held = self
            .table
            .enter()
            .ok_or_else(|| whole_call(ValueErrorKind::Reentrant))?;
        let mut batch = Batch::new(held.params());
        read.read(&mut batch).map_err(error)
    }
}

/// The session's values for any control-side thread: `Clone + Send`, it
/// borrows nothing, so a UI thread writes and reads while the session thread
/// runs. Never for an audio or MIDI callback: a call takes the value lock.
///
/// Once the session is gone, every call answers
/// [`ValueErrorKind::Closed`].
pub struct Knobs<K: AppKey = &'static str> {
    state: Weak<ValueState<K>>,
}

impl<K: AppKey> Knobs<K> {
    pub(crate) fn of(state: &Arc<ValueState<K>>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }

    fn state(&self) -> Result<Arc<ValueState<K>>, ValueError<K>> {
        self.state
            .upgrade()
            .ok_or_else(|| whole_call(ValueErrorKind::Closed))
    }

    /// [`Session::set`](crate::Session::set), from this handle.
    ///
    /// # Errors
    ///
    /// As [`Session::set`](crate::Session::set), and
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn set(&self, assign: impl IntoAssign<K>) -> Result<(), ValueError<K>> {
        self.state()?.set(assign)
    }

    /// [`Session::get`](crate::Session::get), from this handle.
    ///
    /// # Errors
    ///
    /// As [`Session::get`](crate::Session::get), and
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn get<R: IntoRead>(&self, read: R) -> Result<R::Output, ValueError<K>> {
        self.state()?.get(read)
    }
}

impl<K: AppKey> Clone for Knobs<K> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<K: AppKey> fmt::Debug for Knobs<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Knobs").finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "midi"))]
mod tests {
    use super::*;

    /// **A write from a thread that holds the value lock is `Reentrant`**,
    /// the whole call's error, and changes nothing: what `Midi::play_clip`
    /// meets if it is ever reached from inside a `modify`.
    #[test]
    fn a_write_under_the_value_lock_is_reentrant() {
        let table = ValueTable::new();
        let target = crate::Key::<tutti_midi_runtime::MidiClipNode>::new(NodeKey(1)).events();
        let held = table.enter().expect("the lock is free");
        let refused = table.write(target.to(tutti_midi_runtime::ClipEvents::default()));
        assert!(matches!(refused, Err(ValueErrorKind::Reentrant)));
        drop(held);
        let absent = table.write(target.to(tutti_midi_runtime::ClipEvents::default()));
        assert!(matches!(absent, Err(ValueErrorKind::NoSuchTarget)));
    }
}
