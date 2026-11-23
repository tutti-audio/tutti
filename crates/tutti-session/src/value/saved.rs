//! [`SavedValues`]: every saved target's base at one instant, and the
//! `set` that puts it back.
//!
//! A map holds one [`AnyAssign`] per saved target, so putting it back is the
//! batch of a `set` over those assignments, landing as one restore. The map
//! is copy-on-write behind one `Arc`: a clone, and a capture a mark keeps, is
//! a pointer clone.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_graph::{DeclaredUnit, NodeKey};
use tutti_rt::ParamCell;
use tutti_units::UnitAddr;

use super::batch::{Batch, Failure, IntoAssign, Stageable};
use super::error::{SetRefusal, ValueError};
use super::slot::{SlotCell, SlotId};
use super::state::{Params, ValueState, Values};
use super::target::{
    AnyAssign, AnyTarget, Assign, Target, TargetId,
    sealed::{Sealed, Store},
};
use super::watcher::StoredChange;
use crate::AppKey;

/// The base of every saved target at one instant, read under the value lock, so
/// a batch is in it whole or not at all: what [`Session::saved_values`](crate::Session::saved_values) answers
/// and `set(&saved)` puts back.
///
/// It names its targets by what the session knows them as, not by an app key
/// yet, so it lives in memory as long as the session does; it has no serde
/// until the app-key index gives it a stable key. A target a plugin hosts, a
/// policy, a MIDI device's latency and a plugin's automation mode are not
/// saved. `Clone` is a pointer clone, and `Send + Sync`.
///
/// `session.set(&saved)` lands as one restore: every change a watcher sees is
/// [`ChangedBy::Restore`](crate::ChangedBy::Restore), and two or more targets
/// are one gesture. A target whose node is gone fails the call with
/// [`NoSuchTarget`](crate::ValueErrorKind::NoSuchTarget) at the item's position
/// in [`iter`](Self::iter) order, and nothing lands; [`retain_live`](Self::retain_live)
/// drops those first. A call that stages a target the map names too, with a
/// value of another type than the map's, is refused as
/// [`MixedValueTypes`](SetRefusal::MixedValueTypes).
///
/// ```
/// fn shared<T: Clone + Send + Sync>() {}
/// shared::<tutti_session::SavedValues<&'static str>>();
/// ```
pub struct SavedValues<K> {
    items: Arc<BTreeMap<TargetId, AnyAssign>>,
    key: PhantomData<fn() -> K>,
}

/// The saved targets of one instant, without the session's key type: what the
/// feed captures for an edit log.
pub(super) type Captured = Arc<BTreeMap<TargetId, AnyAssign>>;

impl<K> SavedValues<K> {
    /// The map of a capture, sharing it.
    pub(super) fn of_captured(items: Captured) -> Self {
        Self {
            items,
            key: PhantomData,
        }
    }

    fn new(items: BTreeMap<TargetId, AnyAssign>) -> Self {
        Self {
            items: Arc::new(items),
            key: PhantomData,
        }
    }

    /// How many targets are saved.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no target is saved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Each saved target with the assignment of its saved value, in the order
    /// `set(&saved)` stages them.
    pub fn iter(&self) -> impl Iterator<Item = (AnyTarget<K>, &AnyAssign)> + '_ {
        self.items
            .iter()
            .map(|(id, assign)| (AnyTarget::of(*id), assign))
    }

    /// Takes `target` out, answering its assignment when it was saved.
    pub fn remove(&mut self, target: &AnyTarget<K>) -> Option<AnyAssign> {
        let id = target.id();
        if !self.items.contains_key(&id) {
            return None;
        }
        Arc::make_mut(&mut self.items).remove(&id)
    }

    /// Keeps the targets that are there now in `values` and drops the rest: what a
    /// lenient restore calls first (`saved.retain_live(&session.values())`).
    ///
    /// # Errors
    ///
    /// [`Reentrant`](crate::ValueErrorKind::Reentrant) from inside a
    /// `modify`, and [`Closed`](crate::ValueErrorKind::Closed) once the session
    /// is gone.
    pub fn retain_live(&mut self, values: &Values<K>) -> Result<(), ValueError<K>>
    where
        K: AppKey,
    {
        let state = values.state()?;
        let held = state.enter()?;
        let gone: Vec<TargetId> = self
            .items
            .iter()
            .filter(|(_, assign)| !assign.live(held.params()))
            .map(|(id, _)| *id)
            .collect();
        if !gone.is_empty() {
            let items = Arc::make_mut(&mut self.items);
            for id in gone {
                items.remove(&id);
            }
        }
        Ok(())
    }
}

impl<K> Clone for SavedValues<K> {
    fn clone(&self) -> Self {
        Self {
            items: Arc::clone(&self.items),
            key: PhantomData,
        }
    }
}

/// Shows how many targets are saved, never their values.
impl<K> fmt::Debug for SavedValues<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedValues")
            .field("len", &self.items.len())
            .finish()
    }
}

impl<K> Stageable for &SavedValues<K> {
    fn stage(self, batch: &mut Batch<'_>) -> Result<(), Failure> {
        batch.restoring();
        self.items
            .values()
            .try_for_each(|assign| assign.0.stage(batch))
    }
}
impl<K> IntoAssign<K> for &SavedValues<K> {}

impl<K: AppKey> ValueState<K> {
    /// The base of every saved target, read under the value lock.
    pub(super) fn saved_values(&self) -> Result<SavedValues<K>, ValueError<K>> {
        let held = self.enter()?;
        Ok(SavedValues::new(held.params().capture()))
    }

    /// The assignments among `items` whose target is saved, as a map.
    pub(super) fn values_of(items: impl IntoIterator<Item = AnyAssign>) -> SavedValues<K> {
        SavedValues::new(
            items
                .into_iter()
                .map(|assign| (assign.id(), assign))
                .filter(|(id, _)| id.saved())
                .collect(),
        )
    }
}

impl Params {
    /// The value now of every saved target, shareable.
    pub(super) fn captured(&self) -> Captured {
        Arc::new(self.capture())
    }

    /// An assignment of the value now of every saved target.
    fn capture(&self) -> BTreeMap<TargetId, AnyAssign> {
        let params = self.native_params().map(|(node, addr, owner, cell)| {
            let target = CellParam { node, addr, owner };
            let assign = Assign {
                target,
                value: CellValue(cell),
            };
            (Sealed::id(target), assign.erase())
        });
        let slots = self
            .slots
            .iter()
            .filter(|(id, _)| id.saved())
            .map(|(id, erased)| (TargetId::Slot(*id), erased.capture(*id)));
        params.chain(slots).collect()
    }
}

/// A native param by its cell, whatever its unit: the target a captured param
/// is assigned through. The value is the cell's raw `f64`, which a restore
/// stores as it is; a watcher is told it in the node's own unit
/// ([`DeclaredUnit`]).
#[derive(Clone, Copy)]
struct CellParam {
    node: NodeKey,
    addr: UnitAddr,
    owner: TypeId,
}

/// A param's cell value: the raw of its unit.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellValue(f64);

impl Sealed for CellParam {
    fn id(self) -> TargetId {
        TargetId::Param(self.node, self.addr, self.owner)
    }
}
impl Target for CellParam {
    type Value = CellValue;
}
impl Store for CellParam {
    type Cell = (ParamCell, DeclaredUnit);

    fn resolve(self, params: &Params) -> Option<(Self::Cell, CellValue)> {
        let (cell, unit) = params.unit_cell(self.node, self.addr, self.owner)?;
        let base = CellValue(cell.load(Ordering::Acquire));
        Some(((cell, unit), base))
    }

    fn check((_, _): &Self::Cell, value: &CellValue) -> Result<(), SetRefusal> {
        if !value.0.is_finite() {
            return Err(SetRefusal::NotFinite);
        }
        Ok(())
    }

    fn store((cell, _): &Self::Cell, value: &CellValue) {
        cell.store(value.0, Ordering::Release);
    }

    fn landing(
        id: TargetId,
        (_, unit): &Self::Cell,
        base: CellValue,
        value: CellValue,
    ) -> StoredChange {
        StoredChange::erased(id, unit.value(base.0), unit.value(value.0))
    }
}

/// A session value by its slot, whatever its target: the target a captured
/// slot is assigned through, holding the value type the slot was registered
/// with.
struct SlotValue<V> {
    id: SlotId,
    value: PhantomData<fn() -> V>,
}

impl<V> Clone for SlotValue<V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for SlotValue<V> {}

impl<V: Clone + PartialEq + Send + Sync + 'static> Sealed for SlotValue<V> {
    fn id(self) -> TargetId {
        TargetId::Slot(self.id)
    }
}
impl<V: Clone + PartialEq + Send + Sync + 'static> Target for SlotValue<V> {
    type Value = V;
}
impl<V: Clone + PartialEq + Send + Sync + 'static> Store for SlotValue<V> {
    type Cell = SlotCell<V>;

    fn resolve(self, params: &Params) -> Option<(Self::Cell, V)> {
        let cell = params.slot::<V>(self.id)?;
        let base = cell.load();
        Some((cell, base))
    }

    fn check(cell: &Self::Cell, value: &V) -> Result<(), SetRefusal> {
        cell.check(value)
    }

    fn store(cell: &Self::Cell, value: &V) {
        cell.store(value);
    }
}

/// The assignment of the value `cell` holds, a slot registered as `V`.
pub(super) fn capture<V: Clone + PartialEq + Send + Sync + 'static>(
    id: SlotId,
    cell: &(dyn Any + Send + Sync),
) -> AnyAssign {
    let cell = cell
        .downcast_ref::<SlotCell<V>>()
        .expect("a slot is registered under its value type");
    Assign {
        target: SlotValue::<V> {
            id,
            value: PhantomData,
        },
        value: cell.load(),
    }
    .erase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map of one saved target, built from an assignment.
    fn one() -> SavedValues<&'static str> {
        let assign = Assign {
            target: SlotValue::<u8> {
                id: SlotId::ClickVolume,
                value: PhantomData,
            },
            value: 7,
        }
        .erase();
        ValueState::<&'static str>::values_of([assign])
    }

    /// **A clone of the map shares its root**: two owners of one `Arc`, until one
    /// of them removes a target and copies it.
    #[test]
    fn a_clone_shares_the_root_until_it_removes_a_target() {
        let saved = one();
        let mut clone = saved.clone();
        assert_eq!(Arc::strong_count(&saved.items), 2);
        assert!(Arc::ptr_eq(&saved.items, &clone.items));

        let (target, _) = saved.iter().next().expect("one target");
        let absent = AnyTarget::of(TargetId::Slot(SlotId::ClickMode));
        assert!(clone.remove(&absent).is_none());
        assert_eq!(
            Arc::strong_count(&saved.items),
            2,
            "removing what is not there copies nothing"
        );

        assert!(clone.remove(&target).is_some());
        assert_eq!(Arc::strong_count(&saved.items), 1);
        assert_eq!((saved.len(), clone.len()), (1, 0));
    }

    /// **The saved flag is the registry key's**: the output policy is not saved, the
    /// tempo and a param are.
    #[test]
    fn the_output_policy_slot_is_not_saved_and_the_tempo_slot_is() {
        assert!(!TargetId::Slot(SlotId::OutputPolicy).saved());
        assert!(TargetId::Slot(SlotId::Tempo).saved());
        let param = TargetId::Param(
            NodeKey(1),
            UnitAddr::first(tutti_units::UnitParam::Cutoff),
            TypeId::of::<u8>(),
        );
        assert!(param.saved());
    }

    /// **A saved map shows its size and none of its values**.
    #[test]
    fn debug_shows_the_count_only() {
        assert_eq!(format!("{:?}", one()), "SavedValues { len: 1 }");
    }
}
