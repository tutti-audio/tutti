//! The session's pool: the declared set diffed by key and identity, and each
//! entry's life of declared, held and released.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::SessionEvent;
use crate::pool::entry::{ErasedIdentity, Item, Life, Load, LoadError, PoolEntry, PoolSet, Value};
use crate::pool::key::PoolKey;

/// Where a pool entry stands.
///
/// [`Session::pool_state`](crate::Session::pool_state) reads it. `Reloading`
/// counts as loaded: the old value stays until the new one lands.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PoolState {
    /// Declared; its load has not landed.
    Loading,
    /// Declared and loaded.
    Loaded,
    /// Declared and loaded; its identity changed and the new load has not
    /// landed. The old value and its handles stay until the new load lands;
    /// if it fails, they end (the old handles read
    /// [`Presence::Closed`](crate::Presence::Closed)) and the entry is
    /// `Failed`.
    Reloading,
    /// Declared; its load failed. Declaring the key again with the same
    /// identity does not load again; a retry does. A failed reload leaves no
    /// value: the old one is dropped.
    Failed(LoadError),
    /// No longer declared, and kept by a node or a snapshot until their last
    /// hold is dropped.
    Held,
}

/// An entry's change, as [`SessionEvent::Pool`] reports it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PoolEvent {
    /// The entry's first load landed.
    Loaded,
    /// A reload landed: the entry's identity changed and the new value
    /// replaced the old one.
    Reloaded,
    /// The load failed.
    Failed(LoadError),
    /// The entry was released: undeclared, with no hold left. Its handles
    /// read [`Presence::Closed`](crate::Presence::Closed).
    Released,
}

/// Why [`Session::pool`](crate::Session::pool) refused a set. It refuses the
/// whole call: nothing of the set is applied.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PoolError<K: fmt::Debug> {
    /// The set names this key twice.
    #[error("the pool names key {0:?} twice")]
    DuplicateKey(K),
}

/// Why a key's reader (`Session::audio`, `Session::soundfont`) found nothing.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HandleError<K: fmt::Debug> {
    /// The pool does not declare the key: never declared, or released.
    #[error("key {0:?} is not in the pool")]
    NotPooled(K),
    /// The key is declared as another kind of entry than the reader's.
    #[error("key {0:?} is a pool entry of another kind")]
    WrongKind(K),
    /// The entry has not landed; the state says why (loading, or failed).
    #[error("the pool entry has not landed: {0:?}")]
    NotLoaded(PoolState),
}

/// A claim on a pool entry that keeps it held, not released, after the app's
/// pool stops naming it: a node over it, a snapshot. Dropping it is the hold's
/// end; the entry is released at the next update that finds no hold left.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "a node over an entry and a snapshot take holds; neither exists yet"
    )
)]
pub(crate) struct Hold {
    _claim: Arc<()>,
}

/// One key's entry.
struct Slot {
    kind: TypeId,
    identity: Box<dyn ErasedIdentity>,
    /// The app's pool names it.
    declared: bool,
    /// Its place in the set that last named it.
    order: usize,
    /// One strong count for the slot, one for each [`Hold`].
    holds: Arc<()>,
    life: Life,
    loaded: Option<Value>,
    state: PoolState,
    /// The outcome that landed while the entry was held: reported when the
    /// pool declares the entry again.
    unreported: Option<PoolEvent>,
    /// The load that may land: an older one is stale.
    generation: u64,
}

impl Slot {
    fn state(&self) -> PoolState {
        if self.declared {
            self.state.clone()
        } else {
            PoolState::Held
        }
    }

    /// No hold but the slot's own.
    fn unheld(&self) -> bool {
        Arc::strong_count(&self.holds) == 1
    }
}

/// A load the diff started: the session runs it on its spawner and lands the
/// result with the same key and generation.
pub(crate) struct Start {
    pub(crate) key: PoolKey,
    pub(crate) generation: u64,
    pub(crate) load: Load<Value>,
}

struct Queued {
    order: usize,
    key: PoolKey,
    event: PoolEvent,
}

/// The pool: every key's entry, and the events not yet reported.
#[derive(Default)]
pub(crate) struct Pool {
    slots: BTreeMap<PoolKey, Slot>,
    last_generation: u64,
    queued: Vec<Queued>,
    /// Released since the last report.
    released: Vec<PoolKey>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("entries", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Diffs `set` against the current set by key and identity.
    ///
    /// A new key starts a load. A key whose kind and identity are unchanged
    /// keeps its entry, whether it was declared or held (adopted): no reload.
    /// A key whose identity changed starts a load that supersedes any running
    /// one, and its loaded value stays until the new one lands. A key the set
    /// no longer names is held while a [`Hold`] is outstanding, and released
    /// otherwise.
    ///
    /// # Errors
    ///
    /// [`PoolError::DuplicateKey`] for a key named twice. Nothing is applied.
    pub(crate) fn declare<K: Ord + Clone + fmt::Debug + Send + 'static>(
        &mut self,
        set: PoolSet<K>,
    ) -> Result<Vec<Start>, PoolError<K>> {
        let mut named = BTreeSet::new();
        for item in &set.items {
            if !named.insert(PoolKey::new(&item.key)) {
                return Err(PoolError::DuplicateKey(item.key.clone()));
            }
        }
        let mut starts = Vec::new();
        for (order, item) in set.items.into_iter().enumerate() {
            let key = PoolKey::new(&item.key);
            if let Some(start) = self.declare_one(&key, order, item) {
                starts.push(start);
            }
        }
        let dropped: Vec<PoolKey> = self
            .slots
            .iter()
            .filter(|(key, slot)| slot.declared && !named.contains(key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in dropped {
            if let Some(slot) = self.slots.get_mut(&key) {
                slot.declared = false;
            }
        }
        self.sweep();
        Ok(starts)
    }

    fn declare_one<K>(&mut self, key: &PoolKey, order: usize, item: Item<K>) -> Option<Start> {
        let generation = self.next_generation();
        let Some(slot) = self.slots.get_mut(key) else {
            // Released and declared again before the report: the key lives,
            // and only the new slot's outcome is reported.
            self.released.retain(|released| released != key);
            self.queued.retain(|queued| queued.key != *key);
            self.slots.insert(
                key.clone(),
                Slot {
                    kind: item.kind,
                    identity: item.identity,
                    declared: true,
                    order,
                    holds: Arc::new(()),
                    life: Life::new(),
                    loaded: None,
                    state: PoolState::Loading,
                    unreported: None,
                    generation,
                },
            );
            return Some(Start {
                key: key.clone(),
                generation,
                load: (item.load)(),
            });
        };
        slot.declared = true;
        slot.order = order;
        if slot.kind == item.kind && slot.identity.same(&*item.identity) {
            if let Some(event) = slot.unreported.take() {
                self.queued.push(Queued {
                    order,
                    key: key.clone(),
                    event,
                });
            }
            return None;
        }
        slot.unreported = None;
        slot.kind = item.kind;
        slot.identity = item.identity;
        slot.generation = generation;
        slot.state = if slot.loaded.is_some() {
            PoolState::Reloading
        } else {
            PoolState::Loading
        };
        Some(Start {
            key: key.clone(),
            generation,
            load: (item.load)(),
        })
    }

    fn next_generation(&mut self) -> u64 {
        self.last_generation += 1;
        self.last_generation
    }

    /// Releases every undeclared entry that no hold keeps.
    pub(crate) fn sweep(&mut self) {
        let released: Vec<PoolKey> = self
            .slots
            .iter()
            .filter(|(_, slot)| !slot.declared && slot.unheld())
            .map(|(key, _)| key.clone())
            .collect();
        for key in released {
            if let Some(slot) = self.slots.remove(&key) {
                slot.life.close();
                self.released.push(key);
            }
        }
    }

    /// Takes in a load's result. A result whose key was released, or whose
    /// identity changed since the load started, is dropped.
    pub(crate) fn land(&mut self, key: &PoolKey, generation: u64, done: Result<Value, LoadError>) {
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
        if slot.generation != generation {
            return;
        }
        let reloaded = slot.loaded.take().is_some();
        if reloaded {
            slot.life.close();
            slot.life = Life::new();
        }
        let event = match done {
            Ok(value) => {
                slot.loaded = Some(value);
                slot.state = PoolState::Loaded;
                if reloaded {
                    PoolEvent::Reloaded
                } else {
                    PoolEvent::Loaded
                }
            }
            Err(error) => {
                slot.state = PoolState::Failed(error.clone());
                PoolEvent::Failed(error)
            }
        };
        if slot.declared {
            self.queued.push(Queued {
                order: slot.order,
                key: key.clone(),
                event,
            });
        } else {
            slot.unreported = Some(event);
        }
    }

    /// The events since the last call: landings in the order the current set
    /// names them, then releases by key within a key type.
    pub(crate) fn drain_events(&mut self, out: &mut Vec<SessionEvent>) {
        self.queued.sort_by_key(|queued| queued.order);
        out.extend(
            self.queued
                .drain(..)
                .map(|Queued { key, event, .. }| SessionEvent::Pool { key, event }),
        );
        self.released.sort();
        out.extend(self.released.drain(..).map(|key| SessionEvent::Pool {
            key,
            event: PoolEvent::Released,
        }));
    }

    pub(crate) fn state(&self, key: &PoolKey) -> Option<PoolState> {
        self.slots.get(key).map(Slot::state)
    }

    /// A hold on `key`'s entry, `None` when the pool has none.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a node over an entry and a snapshot take holds")
    )]
    pub(crate) fn hold(&self, key: &PoolKey) -> Option<Hold> {
        self.slots.get(key).map(|slot| Hold {
            _claim: Arc::clone(&slot.holds),
        })
    }

    /// `E`'s handle for the loaded entry under `key`.
    #[cfg_attr(
        not(any(test, feature = "sampler", feature = "soundfont")),
        expect(
            dead_code,
            reason = "the readers of the data kinds (features `sampler`, `soundfont`) are its callers"
        )
    )]
    pub(crate) fn handle<K, E>(&self, key: &K) -> Result<E::Handle, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + 'static,
        E: PoolEntry,
    {
        let slot = self
            .slots
            .get(&PoolKey::new(key))
            .filter(|slot| slot.declared)
            .ok_or_else(|| HandleError::NotPooled(key.clone()))?;
        if slot.kind != TypeId::of::<E>() {
            return Err(HandleError::WrongKind(key.clone()));
        }
        let loaded = slot
            .loaded
            .as_ref()
            .ok_or_else(|| HandleError::NotLoaded(slot.state()))?;
        let loaded = loaded
            .downcast_ref::<E::Loaded>()
            .ok_or_else(|| HandleError::WrongKind(key.clone()))?;
        Ok(E::handle(loaded, &slot.life))
    }
}
