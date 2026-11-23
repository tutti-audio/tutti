//! The session's value state: the value lock and the table of what each node
//! can be written through, shared by the session, its graph and every
//! [`Values`].

use std::any::TypeId;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, Weak};

use tutti_graph::{DeclaredUnit, EffectiveCell, NodeKey, ParamSet, PassError};
use tutti_rt::Mutex;
use tutti_rt::ParamCell;
use tutti_units::UnitAddr;

use super::batch::{Batch, Failure, IntoAssign, IntoRead, Readable, Stageable};
use super::controls::{GainSlot, MixSlot, ModeSlot, NodeControl, Queue, Write};
use super::error::{ValueError, ValueErrorKind};
use super::gesture::{Deferred, Ending, Gesture};
use super::saved::SavedValues;
use super::slot::{self, NodeSlots, SlotCell, SlotId};
use super::target::{AnyAssign, AnyTarget};
use super::watcher::{Feed, Grouping, IntoTargets, Watcher};
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
    /// What each param's lane last delivered, once the graph's editor has
    /// the cells (`bind_lanes`).
    lanes: Vec<(UnitAddr, EffectiveCell)>,
}

/// The params a node had when a replace took its key: what the incoming
/// node's params are seeded from ([`ValueTable::address`]).
#[derive(Default, Debug)]
pub(crate) struct Outgoing(Option<NodeParams>);

/// The key-to-cell table, behind the value lock.
#[derive(Default, Debug)]
pub struct Params {
    nodes: BTreeMap<NodeKey, NodeParams>,
    /// The session's own values, by what they are: each a
    /// [`SlotCell`] of its value type.
    pub(super) slots: BTreeMap<SlotId, slot::Erased>,
    /// Each node's mode and mix, which its two targets share.
    controls: BTreeMap<NodeKey, Arc<NodeControl>>,
    /// What each gain lane last delivered.
    gain_lanes: BTreeMap<crate::ChannelEdge, EffectiveCell>,
    /// Each hosted plugin, by its node.
    #[cfg(feature = "plugin")]
    plugins: BTreeMap<NodeKey, Arc<super::PluginEntry>>,
}

impl Params {
    /// `addr`'s cell on `node`, when the node is of the type `owner` names.
    pub fn cell(&self, node: NodeKey, addr: UnitAddr, owner: TypeId) -> Option<ParamCell> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        node.set.cell(addr)
    }

    /// `addr`'s cell on `node` with the unit the node declares it in, when
    /// the node is of the type `owner` names and declares one.
    pub(super) fn unit_cell(
        &self,
        node: NodeKey,
        addr: UnitAddr,
        owner: TypeId,
    ) -> Option<(ParamCell, DeclaredUnit)> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        Some((node.set.cell(addr)?, node.set.unit(addr)?))
    }

    /// Every native param a declaration gave a unit, with the value in its
    /// cell: the node, the address, the node type and the cell's raw.
    pub(super) fn native_params(
        &self,
    ) -> impl Iterator<Item = (NodeKey, UnitAddr, TypeId, f64)> + '_ {
        self.nodes.iter().flat_map(|(node, params)| {
            params.set.params().filter_map(move |addr| {
                params.set.unit(addr)?;
                let raw = params.set.get(addr)?;
                Some((*node, addr, params.owner, raw))
            })
        })
    }

    /// What `addr`'s lane on `node` last delivered, as the raw of its unit:
    /// `None` when no lane drives it, or the node is not of the type `owner`
    /// names.
    pub(super) fn lane(&self, node: NodeKey, addr: UnitAddr, owner: TypeId) -> Option<f64> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        node.lanes.iter().find(|(a, _)| *a == addr)?.1.current()
    }

    /// What `edge`'s gain lane last delivered, as the raw of its unit.
    pub(super) fn gain_lane(&self, edge: crate::ChannelEdge) -> Option<f64> {
        self.gain_lanes.get(&edge)?.current()
    }

    /// The plugin hosted at `node`.
    #[cfg(feature = "plugin")]
    pub(super) fn plugin(&self, node: NodeKey) -> Option<Arc<super::PluginEntry>> {
        self.plugins.get(&node).cloned()
    }

    /// `addr`'s value on `node` when the node was last addressed, when the
    /// node is of the type `owner` names.
    pub fn seed(&self, node: NodeKey, addr: UnitAddr, owner: TypeId) -> Option<f64> {
        let node = self.nodes.get(&node).filter(|n| n.owner == owner)?;
        node.seeds.iter().find(|(a, _)| *a == addr).map(|(_, v)| *v)
    }
}

/// Copies `from`'s value at `addr` into `set`'s cell, when both declare the
/// param in one unit at one declared value and the value is finite, as a
/// write requires ([`ParamTarget`](crate::ParamTarget)'s check). `declared`
/// is the incoming declaration's value: both seeds are the declaration's
/// initial in the cell's own `f64`, so they are compared as that.
fn seed_from(from: &NodeParams, set: &ParamSet, addr: UnitAddr, declared: f64) {
    let (Some(old), Some(new)) = (from.set.cell(addr), set.cell(addr)) else {
        return;
    };
    let same_unit = matches!(
        (from.set.unit(addr), set.unit(addr)),
        (Some(a), Some(b)) if a == b
    );
    let same_declaration = from
        .seeds
        .iter()
        .any(|&(a, seed)| a == addr && seed.to_bits() == declared.to_bits());
    let live = old.load(Ordering::Acquire);
    if same_unit && same_declaration && live.is_finite() {
        new.store(live, Ordering::Release);
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
    /// The controls written since the last update, for the graph's editor.
    queue: Arc<Queue>,
    /// Every change a write made, for the watchers.
    feed: Feed,
    /// The ends of gestures dropped while this thread held the lock.
    deferred: Deferred,
}

impl fmt::Debug for ValueTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValueTable").finish_non_exhaustive()
    }
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
        let guard = self.params.lock();
        HELD.with(|held| held.borrow_mut().push(token));
        let held = Held { guard, token };
        self.deferred.land(held.params(), &self.feed);
        Some(held)
    }

    /// Lands what `ending` records, now under the value lock, or with the
    /// call this thread is inside when it holds the lock already.
    pub(crate) fn finish(&self, ending: Ending) {
        match self.enter() {
            Some(held) => ending(held.params(), &self.feed),
            None => self.deferred.push(ending),
        }
    }

    /// The feed of changes a write made.
    pub(crate) fn feed(&self) -> &Feed {
        &self.feed
    }

    /// The cell the graph publishes the output device and main output to.
    pub(crate) fn output_facts(&self) -> crate::policy::FactsCell {
        Arc::clone(&self.output)
    }

    /// Addresses `node`'s params, replacing any it had, and seeds each from
    /// the `outgoing` node's value at the same address, under the value lock
    /// and through the cells alone: no write reaches the feed, so no watcher
    /// and no edit-log mark sees it. A carried value equals the one the last
    /// mark's saved values hold for that address. Control thread.
    ///
    /// An address takes the outgoing node's value when both nodes declare it
    /// in one unit, both declared it at the same value, and the value is one
    /// a write accepts (finite). A declaration that changed keeps its new
    /// initial, and an address the incoming node lacks is dropped with the
    /// outgoing params.
    pub(crate) fn address(&self, node: NodeKey, owner: TypeId, set: ParamSet, outgoing: &Outgoing) {
        let seeds: Vec<(UnitAddr, f64)> = set
            .params()
            .filter_map(|addr| Some((addr, set.cell(addr)?.load(Ordering::Acquire))))
            .collect();
        let old = {
            let mut params = self.params.lock();
            if let Some(from) = &outgoing.0 {
                for &(addr, declared) in &seeds {
                    seed_from(from, &set, addr, declared);
                }
            }
            params.nodes.insert(
                node,
                NodeParams {
                    owner,
                    set,
                    seeds,
                    lanes: Vec::new(),
                },
            )
        };
        drop(old);
    }

    /// Puts `outgoing` back under `node` whole, as it was: a replace that
    /// failed leaves the node it did not replace its params, seeds and lanes.
    /// Control thread.
    pub(crate) fn reinstate(&self, node: NodeKey, outgoing: Outgoing) {
        if let Some(params) = outgoing.0 {
            let old = self.params.lock().nodes.insert(node, params);
            drop(old);
        }
    }

    /// Binds the delivered-value cells `cell` names to `node`'s params, as
    /// the graph's editor has them now (a replace gives a key fresh cells).
    /// Control thread.
    pub(crate) fn bind_lanes(
        &self,
        node: NodeKey,
        cell: impl Fn(UnitAddr) -> Option<EffectiveCell>,
    ) {
        let mut params = self.params.lock();
        if let Some(entry) = params.nodes.get_mut(&node) {
            entry.lanes = entry
                .set
                .params()
                .filter_map(|addr| Some((addr, cell(addr)?)))
                .collect();
        }
    }

    /// Binds `edge`'s gain target to the cell its lane delivers to. Control
    /// thread.
    pub(crate) fn bind_gain_lane(&self, edge: crate::ChannelEdge, cell: EffectiveCell) {
        self.params.lock().gain_lanes.insert(edge, cell);
    }

    /// Registers the session's own value `id`, held in `slot`. Once, as the
    /// session is built.
    pub(crate) fn provide<V: Clone + PartialEq + Send + Sync + 'static>(
        &self,
        id: SlotId,
        slot: SlotCell<V>,
    ) {
        self.params.lock().slots.insert(id, slot::erase(slot));
    }

    /// Registers the session's own staging value `id`, held in `slot`. Once,
    /// as the session is built.
    pub(crate) fn provide_staged<C: slot::Held>(
        &self,
        id: SlotId,
        slot: Arc<slot::PublishedSlot<C>>,
    ) {
        self.params
            .lock()
            .slots
            .insert(id, slot::erase_staged(slot));
    }

    /// Forgets the slots `ids` name, with what was written to them and not
    /// yet published. Control thread.
    pub(crate) fn unprovide(&self, ids: &[SlotId]) {
        let gone: Vec<_> = {
            let mut params = self.params.lock();
            ids.iter()
                .filter_map(|id| params.slots.remove(id))
                .collect()
        };
        drop(gone);
    }

    /// Drops what was written to the slots `ids` name and not yet published.
    /// Control thread.
    #[cfg(feature = "midi")]
    pub(crate) fn discard_pending(&self, ids: &[SlotId]) {
        if let Some(held) = self.enter() {
            held.params().discard_pending(ids);
        }
    }

    /// Publishes every value written since the last call and not yet
    /// published ([`Slot::store`](slot::Slot::store) on a staging slot), all
    /// together under the value lock. Control thread.
    pub(crate) fn publish_pending(&self) {
        if let Some(held) = self.enter() {
            held.params().publish_pending();
        }
    }

    /// Registers the values of a node, from its [`NodeSlots`]. Control
    /// thread.
    pub(crate) fn provide_node(&self, slots: NodeSlots) {
        self.params.lock().slots.extend(slots.0);
    }

    /// Writes `assign` as [`Session::set`](crate::Session::set) does, for the
    /// session's own wiring (a clip `Midi` plays).
    #[cfg(feature = "midi")]
    pub(crate) fn write<A: Stageable>(&self, assign: A) -> Result<(), ValueErrorKind> {
        let held = self.enter().ok_or(ValueErrorKind::Reentrant)?;
        let mut batch = Batch::new(held.params());
        assign.stage(&mut batch).map_err(|fail| fail.kind)?;
        batch.store();
        Ok(())
    }

    /// Gives `node` its mode and mix targets, or refreshes what they check
    /// against: why the node has no pass (`None` when it has one), and whether
    /// it bypasses itself. Control thread.
    pub(crate) fn provide_node_controls(
        &self,
        node: NodeKey,
        no_pass: Option<PassError>,
        own_bypass: bool,
    ) {
        let mut params = self.params.lock();
        if let Some(control) = params.controls.get(&node) {
            control.note_pass(no_pass, own_bypass);
            return;
        }
        let control = NodeControl::new(node, no_pass, own_bypass, Arc::clone(&self.queue));
        params.controls.insert(node, Arc::clone(&control));
        params.slots.insert(
            SlotId::Mode(node),
            slot::erase(Arc::new(ModeSlot(Arc::clone(&control))) as SlotCell<_>),
        );
        params.slots.insert(
            SlotId::Mix(node),
            slot::erase(Arc::new(MixSlot(control)) as SlotCell<_>),
        );
    }

    /// Forgets the targets of what is declared on `node`'s params and mix and
    /// on the edges that touch it (a range, a route's depth), the moment the
    /// node leaves the graph: a call that names them fails at the call, not
    /// at the next update. Control thread.
    pub(crate) fn retire_declared_on(&self, node: NodeKey) {
        let touches = |target: &crate::ModTarget| match target {
            crate::ModTarget::Param(p) => p.node == node,
            crate::ModTarget::Mix(m) => m.node == node,
            crate::ModTarget::Edge(e) => e.touches(node),
        };
        let gone: Vec<_> = self
            .params
            .lock()
            .slots
            .extract_if(.., |id, _| match id {
                SlotId::Range(target) | SlotId::RouteDepth((_, target)) => touches(target),
                _ => false,
            })
            .collect();
        drop(gone);
    }

    /// Forgets `node`'s mode and mix targets, and what was written to them
    /// and not yet handed to the editor. Control thread.
    pub(crate) fn retire_node_controls(&self, node: NodeKey) {
        let gone = {
            let mut params = self.params.lock();
            (
                params.controls.remove(&node),
                params.slots.remove(&SlotId::Mode(node)),
                params.slots.remove(&SlotId::Mix(node)),
            )
        };
        self.queue.forget_node(node);
        drop(gone);
    }

    /// Gives `edge` a gain target holding `seed`, unless it has one. Control
    /// thread.
    pub(crate) fn provide_gain(&self, edge: crate::ChannelEdge, seed: tutti_units::Gain) {
        let mut params = self.params.lock();
        params.slots.entry(SlotId::Gain(edge)).or_insert_with(|| {
            slot::erase(GainSlot::new(edge, seed, Arc::clone(&self.queue)) as SlotCell<_>)
        });
    }

    /// The edges that have a gain target.
    pub(crate) fn gain_edges(&self) -> Vec<crate::ChannelEdge> {
        self.params
            .lock()
            .slots
            .keys()
            .filter_map(|id| match id {
                SlotId::Gain(edge) => Some(*edge),
                _ => None,
            })
            .collect()
    }

    /// Forgets the gain target of `edge`, and what was written to it and not
    /// yet handed to the editor. Control thread.
    pub(crate) fn retire_gain(&self, edge: crate::ChannelEdge) {
        let gone = {
            let mut params = self.params.lock();
            (
                params.slots.remove(&SlotId::Gain(edge)),
                params.gain_lanes.remove(&edge),
            )
        };
        self.queue.forget_edge(edge);
        drop(gone);
    }

    /// The controls written since the last call, for the graph's editor.
    pub(crate) fn take_writes(&self) -> Vec<Write> {
        self.queue.take()
    }

    /// Registers the plugin hosted at `node`, replacing the entry it had: its
    /// parameter targets, its automation mode where the plugin takes one
    /// (`mode`) and its track info (`track`). Control thread.
    #[cfg(feature = "plugin")]
    pub(crate) fn provide_plugin(
        &self,
        node: NodeKey,
        entry: super::PluginEntry,
        mode: Option<SlotCell<tutti_plugin_types::AutomationMode>>,
        track: SlotCell<tutti_plugin_types::TrackInfo>,
    ) {
        let mut params = self.params.lock();
        let slot = SlotId::automation_mode(node);
        let old = (
            params.plugins.insert(node, Arc::new(entry)),
            match mode {
                Some(mode) => params.slots.insert(slot, slot::erase(mode)),
                None => params.slots.remove(&slot),
            },
            params
                .slots
                .insert(SlotId::track_info(node), slot::erase(track)),
        );
        drop(params);
        drop(old);
    }

    /// Records the values the plugin at `node` reported or the host set, in
    /// order, as the host mirror holds them. Control thread.
    #[cfg(feature = "plugin")]
    pub(crate) fn note_plugin_values(
        &self,
        node: NodeKey,
        values: &[(tutti_units::ParamId, tutti_units::Normalized)],
    ) {
        if values.is_empty() {
            return;
        }
        if let Some(held) = self.enter()
            && let Some(entry) = held.params().plugin(node)
        {
            for &(id, value) in values {
                entry.note(id, value);
            }
        }
    }

    /// Forgets `node`'s params and slots: its targets are gone.
    pub(crate) fn retire(&self, node: NodeKey) {
        drop(self.retire_keeping(node));
    }

    /// [`retire`](Self::retire), handing back the params the node had, for
    /// the node that replaces it to be seeded from
    /// ([`address`](Self::address)). Control thread.
    pub(crate) fn retire_keeping(&self, node: NodeKey) -> Outgoing {
        let (gone, slots, plugin) = {
            let mut params = self.params.lock();
            #[cfg(feature = "plugin")]
            let plugin = params.plugins.remove(&node);
            #[cfg(not(feature = "plugin"))]
            let plugin = ();
            let gone = params.nodes.remove(&node);
            let slots: Vec<_> = params
                .slots
                .extract_if(.., |id, _| id.node() == Some(node))
                .collect();
            (gone, slots, plugin)
        };
        drop((slots, plugin));
        Outgoing(gone)
    }

    /// `addr`'s cell on `node`, whatever its type, for the graph's own use.
    pub(crate) fn cell(&self, node: NodeKey, addr: UnitAddr) -> Option<ParamCell> {
        self.params.lock().nodes.get(&node)?.set.cell(addr)
    }
}

/// The session's value state, one `Arc` independent of the clock: a clock
/// switch moves it and never rebuilds it. [`Values`] hold it weakly, so the
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

pub(super) fn error<K>(fail: Failure) -> ValueError<K> {
    ValueError {
        at: fail.at,
        target: fail.target.map(AnyTarget::of),
        kind: fail.kind,
    }
}

pub(super) fn whole_call<K>(kind: ValueErrorKind) -> ValueError<K> {
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
        self.table.feed().land(batch.store(), Grouping::Batch);
        self.table.deferred.land(held.params(), self.table.feed());
        Ok(())
    }

    /// Takes the value lock, or answers the call's `Reentrant` error.
    pub(super) fn enter(&self) -> Result<Held<'_>, ValueError<K>> {
        self.table
            .enter()
            .ok_or_else(|| whole_call(ValueErrorKind::Reentrant))
    }

    /// The feed of changes the writes make.
    pub(super) fn feed(&self) -> &Feed {
        self.table.feed()
    }

    /// Lands `ending`, as [`ValueTable::finish`] does.
    pub(super) fn finish(&self, ending: Ending) {
        self.table.finish(ending);
    }

    /// Publishes every value a write staged since the last call. Control
    /// thread.
    pub(crate) fn publish_pending(&self) {
        self.table.publish_pending();
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

/// The session's clone-able write handle for values, beside `ValueState` and
/// `ValueTable`: `Clone + Send`, it borrows nothing, so a UI thread writes
/// and reads while the session thread runs. Never for an audio or MIDI callback: a call takes the value lock.
///
/// Once the session is gone, every call answers
/// [`ValueErrorKind::Closed`].
pub struct Values<K: AppKey = &'static str> {
    state: Weak<ValueState<K>>,
}

impl<K: AppKey> Values<K> {
    pub(crate) fn of(state: &Arc<ValueState<K>>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }

    pub(super) fn state(&self) -> Result<Arc<ValueState<K>>, ValueError<K>> {
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

    /// [`Session::saved_values`](crate::Session::saved_values), from this handle.
    ///
    /// # Errors
    ///
    /// As [`Session::saved_values`](crate::Session::saved_values), and
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn saved_values(&self) -> Result<SavedValues<K>, ValueError<K>> {
        self.state()?.saved_values()
    }

    /// [`Session::values_of`](crate::Session::values_of), from this handle.
    ///
    /// # Errors
    ///
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn values_of(
        &self,
        items: impl IntoIterator<Item = AnyAssign>,
    ) -> Result<SavedValues<K>, ValueError<K>> {
        self.state()?;
        Ok(ValueState::values_of(items))
    }

    /// [`Session::watch`](crate::Session::watch), from this handle.
    ///
    /// # Errors
    ///
    /// As [`Session::watch`](crate::Session::watch), and
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn watch<W: IntoTargets>(&self, targets: W) -> Result<Watcher<K, W>, ValueError<K>> {
        self.state()?.watch(targets)
    }

    /// [`Session::gesture`](crate::Session::gesture), from this handle.
    ///
    /// # Errors
    ///
    /// As [`Session::gesture`](crate::Session::gesture), and
    /// [`ValueErrorKind::Closed`] once the session is gone.
    pub fn gesture<W: IntoTargets>(&self, targets: W) -> Result<Gesture<K, W>, ValueError<K>> {
        self.state()?.gesture(targets)
    }
}

impl<K: AppKey> Clone for Values<K> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<K: AppKey> fmt::Debug for Values<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Values").finish_non_exhaustive()
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
