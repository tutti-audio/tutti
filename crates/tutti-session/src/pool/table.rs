//! The session's pool: the declared set diffed by key and identity, and each
//! entry's life of declared, held and released.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use tutti_rt::MainThreadMarker;

use crate::SessionEvent;
use crate::pool::config::PoolConfig;
use crate::pool::entry::{
    Cloner, ErasedIdentity, Item, Life, Load, LoadError, MainStage, PoolEntry, PoolSet, Reload,
    Staged, Step, Value,
};
use crate::pool::key::PoolKey;
use crate::pool::report::{Landing, PoolOutcome, PoolReport};

/// Where a pool entry stands.
///
/// [`Session::pool_state`](crate::Session::pool_state) reads it. `Reloading`
/// counts as loaded: the old value stays until the new one lands.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PoolState {
    /// Declared; its load has not landed: it waits for a worker (at most
    /// [`PoolConfig::parallel_loads`](crate::PoolConfig::parallel_loads) run at
    /// once) or its worker stage runs.
    Loading,
    /// Declared; its worker stage is done and its stage on the main thread
    /// waits for an [`update`](crate::Session::update) on it.
    WaitingForMainThread,
    /// Declared; its stage on the session's thread waits for the next
    /// [`update`](crate::Session::update).
    Opening,
    /// Declared and loaded.
    Loaded,
    /// Declared and loaded; its identity changed and the new load has not
    /// landed. The old value and its handles stay until the new load lands;
    /// if it fails, they end (the old handles read
    /// [`Presence::Closed`](crate::Presence::Closed)) and the entry is
    /// `Failed`.
    Reloading,
    /// Declared; its load failed. Declaring the key again with the same
    /// identity does not load again; [`Session::retry`](crate::Session::retry)
    /// does. A failed reload leaves no value: the old one is dropped.
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
    /// The set declares this key with a load that has a stage on the main
    /// thread, and no update has run on the main thread: the stage would wait
    /// for one that may never come. A call on the main thread is not refused.
    #[error("key {0:?} needs a main-thread stage, and no update has run on the main thread")]
    NeedsMainThread(K),
}

/// Why [`Session::retry`](crate::Session::retry) did nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RetryError<K: fmt::Debug> {
    /// The key is declared, and its entry neither failed nor reads lost.
    #[error("pool entry {0:?} did not fail and is not lost")]
    NotFailed(K),
    /// The pool does not declare the key: never declared, or released.
    #[error("key {0:?} is not in the pool")]
    NotPooled(K),
    /// The entry's load has a stage on the main thread, and no update has run on
    /// the main thread (and this call is not on it): the load would wait for one
    /// that may never come.
    #[error("key {0:?} needs a main-thread stage, and no update has run on the main thread")]
    NeedsMainThread(K),
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

/// Why [`Session::when_loaded`](crate::Session::when_loaded) took nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WhenLoadedError<K: fmt::Debug> {
    /// The pool does not declare the key: never declared, or released.
    #[error("key {0:?} is not in the pool")]
    NotPooled(K),
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

/// Where a load that has not landed stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Loading,
    Main,
    Opening,
}

impl Stage {
    const fn state(self) -> PoolState {
        match self {
            Self::Loading => PoolState::Loading,
            Self::Main => PoolState::WaitingForMainThread,
            Self::Opening => PoolState::Opening,
        }
    }
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
    /// The kind's load, again, for a retry.
    reload: Reload,
    /// Whether the kind reads the loaded value as lost.
    lost: fn(&Value) -> bool,
    /// How another key's entry of the same identity gets this one's value.
    share: Option<Cloner>,
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

    /// A load of this entry has not landed.
    const fn pending(&self) -> bool {
        matches!(
            self.state,
            PoolState::Loading
                | PoolState::Reloading
                | PoolState::WaitingForMainThread
                | PoolState::Opening
        )
    }

    /// The state while a load that stands at `stage` is pending: the old
    /// value, if there is one, stays the entry's.
    fn awaiting(&mut self, stage: Stage) {
        self.state = if self.loaded.is_some() {
            PoolState::Reloading
        } else {
            stage.state()
        };
    }
}

/// A load the diff started: the session runs it and lands the result under
/// the same flight.
pub(crate) struct Start {
    pub(crate) flight: u64,
    pub(crate) load: Load<Value>,
}

/// One load, in flight, and the entries waiting on it. An entry waits on the
/// flight whose id is its generation; a flight no entry waits on is dead, and
/// whatever it lands is dropped.
struct Flight {
    targets: Vec<PoolKey>,
    stage: Stage,
    /// Set when the flight's entries share: every entry on a flight of several
    /// shares, since an entry whose sharing changes while its load is pending
    /// starts a flight of its own.
    share: Option<Cloner>,
    /// The load has a stage on the main thread.
    needs_main: bool,
}

/// A worker stage waiting for a free worker.
struct Queued {
    flight: u64,
    work: Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>,
}

/// A stage on the session's thread, waiting for the next update.
pub(crate) struct Opening {
    pub(crate) flight: u64,
    pub(crate) open: Box<dyn FnOnce(&mut crate::OpenCx<'_>) -> Result<Value, LoadError> + Send>,
}

/// A stage on the main thread, with its worker's output, waiting for an update
/// there. Whether it runs or is discarded is decided when it comes up: a flight
/// cancelled or superseded meanwhile drops the output on the main thread, never
/// where the cancel was.
pub(crate) struct MainItem {
    pub(crate) flight: u64,
    pub(crate) stage: MainStage,
}

/// What the app asked to run once an entry is loaded.
struct WhenLoaded {
    key: PoolKey,
    then: Box<dyn FnOnce(&mut crate::Session) + Send>,
}

struct Pending {
    order: usize,
    key: PoolKey,
    event: PoolEvent,
}

/// The pool: every key's entry, the loads in flight, and the events not yet
/// reported.
pub(crate) struct Pool {
    config: PoolConfig,
    slots: BTreeMap<PoolKey, Slot>,
    last_generation: u64,
    flights: BTreeMap<u64, Flight>,
    /// Worker stages not started, in declaration order.
    queue: VecDeque<Queued>,
    /// Worker stages started and not landed.
    running: usize,
    openings: Vec<Opening>,
    main: Vec<MainItem>,
    /// An update has run on the main thread: the stages that wait for it run.
    main_seen: bool,
    queued: Vec<Pending>,
    /// Released since the last report.
    released: Vec<PoolKey>,
    waiters: Vec<Box<dyn FnMut(&Pool) -> bool + Send>>,
    /// Runs waiting for an entry to load, in the order they were asked.
    when_loaded: Vec<WhenLoaded>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("entries", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl Pool {
    pub(crate) fn new(config: PoolConfig) -> Self {
        Self {
            config,
            slots: BTreeMap::new(),
            last_generation: 0,
            flights: BTreeMap::new(),
            queue: VecDeque::new(),
            running: 0,
            openings: Vec::new(),
            main: Vec::new(),
            main_seen: false,
            queued: Vec::new(),
            released: Vec::new(),
            waiters: Vec::new(),
            when_loaded: Vec::new(),
        }
    }

    /// Whether a stage on the main thread will be run: the caller is on the main
    /// thread, or an update has run on it.
    fn has_main_thread(&self) -> bool {
        self.main_seen || MainThreadMarker::new().is_some()
    }

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
    /// [`PoolError::DuplicateKey`] for a key named twice;
    /// [`PoolError::NeedsMainThread`] for a key with a main-thread stage when
    /// there is no main thread. Nothing is applied.
    pub(crate) fn declare<K: Ord + Clone + fmt::Debug + Send + Sync + 'static>(
        &mut self,
        set: PoolSet<K>,
    ) -> Result<Vec<Start>, PoolError<K>> {
        let mut named = BTreeSet::new();
        for item in &set.items {
            if !named.insert(PoolKey::new(&item.key)) {
                return Err(PoolError::DuplicateKey(item.key.clone()));
            }
        }
        if !self.has_main_thread()
            && let Some(item) = set.items.iter().find(|item| item.load.needs_main())
        {
            return Err(PoolError::NeedsMainThread(item.key.clone()));
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
        let Item {
            kind,
            identity,
            load,
            reload,
            lost,
            share,
            ..
        } = item;
        let Some(slot) = self.slots.get_mut(key) else {
            // Released and declared again before the report: the key lives,
            // and only the new slot's outcome is reported.
            self.released.retain(|released| released != key);
            self.queued.retain(|queued| queued.key != *key);
            self.slots.insert(
                key.clone(),
                Slot {
                    kind,
                    identity,
                    declared: true,
                    order,
                    holds: Arc::new(()),
                    life: Life::new(),
                    loaded: None,
                    state: PoolState::Loading,
                    unreported: None,
                    generation: 0,
                    reload,
                    lost,
                    share,
                },
            );
            return self.begin(key, load);
        };
        slot.declared = true;
        slot.order = order;
        let reshared = slot.share.is_some() != share.is_some();
        // What a retry loads, and how the kind reads and shares it, is the
        // latest declaration's.
        slot.reload = reload;
        slot.lost = lost;
        slot.share = share;
        if slot.kind == kind && slot.identity.same(&*identity) {
            if reshared && slot.pending() {
                // A load pending for an entry that shared is not one for an entry
                // that does not, nor the other way: it starts its own.
                slot.unreported = None;
                return self.begin(key, load);
            }
            if let Some(event) = slot.unreported.take() {
                self.queued.push(Pending {
                    order,
                    key: key.clone(),
                    event,
                });
            }
            return None;
        }
        slot.unreported = None;
        slot.kind = kind;
        slot.identity = identity;
        self.begin(key, load)
    }

    /// Starts `key`'s load: it shares a load in flight or a loaded value of the
    /// same identity under another key when the entry shares, and is a flight
    /// of its own otherwise. The slot's old flight, if any, is superseded.
    fn begin(&mut self, key: &PoolKey, load: Load<Value>) -> Option<Start> {
        let generation = self.next_generation();
        let needs_main = load.needs_main();
        let stage = if matches!(load.step, Step::Session(_)) {
            Stage::Opening
        } else {
            Stage::Loading
        };
        if self.slots[key].share.is_some() {
            if let Some(copy) = self.sibling_value(key) {
                let slot = self.slots.get_mut(key).expect("the slot is declared");
                slot.generation = generation;
                self.land_slot(key, Ok(copy));
                return None;
            }
            if let Some(flight) = self.sibling_flight(key) {
                let flight_stage = self.flights[&flight].stage;
                self.flights
                    .get_mut(&flight)
                    .expect("the flight is in flight")
                    .targets
                    .push(key.clone());
                let slot = self.slots.get_mut(key).expect("the slot is declared");
                slot.generation = flight;
                slot.awaiting(flight_stage);
                return None;
            }
        }
        let slot = self.slots.get_mut(key).expect("the slot is declared");
        slot.generation = generation;
        slot.awaiting(stage);
        self.flights.insert(
            generation,
            Flight {
                targets: vec![key.clone()],
                stage,
                share: slot.share,
                needs_main,
            },
        );
        Some(Start {
            flight: generation,
            load,
        })
    }

    /// A copy of the value another key's entry of `key`'s kind and identity
    /// has loaded.
    fn sibling_value(&self, key: &PoolKey) -> Option<Value> {
        let own = &self.slots[key];
        self.slots.iter().find_map(|(other, slot)| {
            let alike = other != key
                && slot.kind == own.kind
                && slot.share.is_some()
                && matches!(slot.state, PoolState::Loaded)
                && slot.identity.same(&*own.identity);
            if alike {
                // A value its kind reads as lost is not one to copy.
                slot.loaded
                    .as_ref()
                    .filter(|value| !(slot.lost)(value))
                    .and_then(|value| own.share?(value))
            } else {
                None
            }
        })
    }

    /// The flight another key's entry of `key`'s kind and identity waits on. A
    /// flight with a stage on the main thread is not one to join with no main
    /// thread: the entry's own load may not need it.
    fn sibling_flight(&self, key: &PoolKey) -> Option<u64> {
        let own = &self.slots[key];
        let main = self.has_main_thread();
        self.slots.iter().find_map(|(other, slot)| {
            let alike = other != key
                && slot.kind == own.kind
                && slot.share.is_some()
                && slot.pending()
                && self
                    .flights
                    .get(&slot.generation)
                    .is_some_and(|flight| main || !flight.needs_main)
                && slot.identity.same(&*own.identity);
            alike.then_some(slot.generation)
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

    /// Whether any entry waits on the flight.
    pub(crate) fn live(&self, flight: u64) -> bool {
        self.flights.get(&flight).is_some_and(|flight_| {
            flight_.targets.iter().any(|key| {
                self.slots
                    .get(key)
                    .is_some_and(|slot| slot.generation == flight)
            })
        })
    }

    /// Queues a worker stage behind the ones waiting.
    pub(crate) fn enqueue(
        &mut self,
        flight: u64,
        work: Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>,
    ) {
        self.queue.push_back(Queued { flight, work });
    }

    /// The next worker stage to start, if fewer than the configured number run:
    /// it counts as running from here. A stage no entry waits on any more is
    /// never started.
    pub(crate) fn next_worker(
        &mut self,
    ) -> Option<(u64, Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>)> {
        while self.running < self.config.parallel_loads.get() {
            let Queued { flight, work } = self.queue.pop_front()?;
            if self.live(flight) {
                self.running += 1;
                return Some((flight, work));
            }
            self.flights.remove(&flight);
        }
        None
    }

    /// A started worker stage ended.
    pub(crate) fn worker_ended(&mut self) {
        self.running -= 1;
    }

    pub(crate) fn open(&mut self, opening: Opening) {
        self.openings.push(opening);
    }

    pub(crate) fn take_openings(&mut self) -> Vec<Opening> {
        std::mem::take(&mut self.openings)
    }

    /// A flight's worker stage ended with a stage for the main thread: it
    /// waits there, whether or not an entry still wants it.
    pub(crate) fn stage_main(&mut self, flight: u64, stage: MainStage) {
        if let Some(flight_) = self.flights.get_mut(&flight) {
            flight_.stage = Stage::Main;
            let targets = flight_.targets.clone();
            for key in targets {
                if let Some(slot) = self.slots.get_mut(&key)
                    && slot.generation == flight
                {
                    slot.awaiting(Stage::Main);
                }
            }
        }
        self.main.push(MainItem { flight, stage });
    }

    /// Takes the main-thread stages, which the main thread runs or discards.
    pub(crate) fn take_main(&mut self, _main: MainThreadMarker) -> Vec<MainItem> {
        self.main_seen = true;
        std::mem::take(&mut self.main)
    }

    /// Forgets a flight no entry waits on, after its stage was dropped.
    pub(crate) fn end_flight(&mut self, flight: u64) {
        self.flights.remove(&flight);
    }

    /// Takes in a flight's result for every entry that still waits on it. A
    /// result nothing waits on (the key was released, or its identity changed
    /// since the load started) is dropped.
    pub(crate) fn land(&mut self, flight: u64, done: Result<Value, LoadError>) {
        let Some(Flight { targets, share, .. }) = self.flights.remove(&flight) else {
            return;
        };
        let waiting: Vec<PoolKey> = targets
            .into_iter()
            .filter(|key| {
                self.slots
                    .get(key)
                    .is_some_and(|slot| slot.generation == flight)
            })
            .collect();
        let Some((last, rest)) = waiting.split_last() else {
            return;
        };
        for key in rest {
            let copy = match &done {
                // Only a flight of entries that share has more than one entry,
                // and its value is its kind's own.
                Ok(value) => Ok(share
                    .and_then(|share| share(value))
                    .expect("a flight of several entries shares its kind's value")),
                Err(error) => Err(error.clone()),
            };
            self.land_slot(key, copy);
        }
        self.land_slot(last, done);
    }

    fn land_slot(&mut self, key: &PoolKey, done: Result<Value, LoadError>) {
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
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
            self.queued.push(Pending {
                order: slot.order,
                key: key.clone(),
                event,
            });
        } else {
            slot.unreported = Some(event);
        }
    }

    /// Loads the declared entry under `key` again: one that failed, or one the
    /// kind reads as lost.
    ///
    /// # Errors
    ///
    /// [`RetryError::NotPooled`] when the key is not declared;
    /// [`RetryError::NotFailed`] when its entry is neither failed nor lost.
    pub(crate) fn retry<K>(&mut self, key: &K) -> Result<Option<Start>, RetryError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let erased = PoolKey::new(key);
        let slot = self
            .slots
            .get_mut(&erased)
            .filter(|slot| slot.declared)
            .ok_or_else(|| RetryError::NotPooled(key.clone()))?;
        let lost =
            matches!(slot.state, PoolState::Loaded) && slot.loaded.as_ref().is_some_and(slot.lost);
        if !lost && !matches!(slot.state, PoolState::Failed(_)) {
            return Err(RetryError::NotFailed(key.clone()));
        }
        let load = (slot.reload)();
        if load.needs_main() && !self.has_main_thread() {
            return Err(RetryError::NeedsMainThread(key.clone()));
        }
        let slot = self.slots.get_mut(&erased).expect("the slot is declared");
        slot.unreported = None;
        Ok(self.begin(&erased, load))
    }

    /// The events since the last call: landings in the order the current set
    /// names them, then releases by key within a key type.
    pub(crate) fn drain_events(&mut self, out: &mut Vec<SessionEvent>) {
        self.queued.sort_by_key(|queued| queued.order);
        out.extend(
            self.queued
                .drain(..)
                .map(|Pending { key, event, .. }| SessionEvent::Pool { key, event }),
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

    /// The declared entries whose load has not landed, in declaration order.
    #[cfg(feature = "export")]
    pub(crate) fn pending(&self) -> Vec<PoolKey> {
        let mut pending: Vec<(usize, &PoolKey)> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.declared && slot.pending())
            .map(|(key, slot)| (slot.order, key))
            .collect();
        pending.sort_by_key(|&(order, _)| order);
        pending.into_iter().map(|(_, key)| key.clone()).collect()
    }

    /// Every declared entry's outcome as a report of `K` keys, `None` while any
    /// declared entry is pending.
    fn report<K: Clone + 'static>(&self) -> Option<PoolReport<K>> {
        let mut entries: Vec<(usize, K, PoolOutcome)> = Vec::new();
        for (key, slot) in self.slots.iter().filter(|(_, slot)| slot.declared) {
            if slot.pending() {
                return None;
            }
            let outcome = match &slot.state {
                PoolState::Failed(error) => PoolOutcome::Failed(error.clone()),
                _ => PoolOutcome::Loaded,
            };
            if let Some(key) = key.get::<K>() {
                entries.push((slot.order, key.clone(), outcome));
            }
        }
        entries.sort_by_key(|&(order, ..)| order);
        Some(PoolReport::new(
            entries
                .into_iter()
                .map(|(_, key, outcome)| (key, outcome))
                .collect(),
        ))
    }

    /// A report of `K` keys that lands, once, when no declared entry is pending.
    /// The wait ends when its [`PoolLoaded`](crate::PoolLoaded) is dropped.
    pub(crate) fn wait_loaded<K: Clone + Send + 'static>(&mut self, landing: Landing<K>) {
        let waiter = move |pool: &Pool| {
            if landing.orphaned() {
                return true;
            }
            let Some(report) = pool.report::<K>() else {
                return false;
            };
            landing.land(report);
            true
        };
        let waiter = Box::new(waiter);
        if !waiter(self) {
            self.waiters.push(waiter);
        }
    }

    /// Lands the waits whose entries all have an outcome, and ends those whose
    /// handle was dropped.
    pub(crate) fn poll_waiters(&mut self) {
        let mut waiters = std::mem::take(&mut self.waiters);
        waiters.retain_mut(|waiter| !waiter(self));
        waiters.append(&mut self.waiters);
        self.waiters = waiters;
    }

    /// Runs `then` once the declared entry under `key` is loaded.
    ///
    /// # Errors
    ///
    /// [`WhenLoadedError::NotPooled`] when the key is not declared.
    pub(crate) fn when_loaded<K>(
        &mut self,
        key: &K,
        then: Box<dyn FnOnce(&mut crate::Session) + Send>,
    ) -> Result<(), WhenLoadedError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let erased = PoolKey::new(key);
        if !self.slots.get(&erased).is_some_and(|slot| slot.declared) {
            return Err(WhenLoadedError::NotPooled(key.clone()));
        }
        self.when_loaded.push(WhenLoaded { key: erased, then });
        Ok(())
    }

    /// The runs whose entry is declared and loaded, in the order they were
    /// asked, taken out. A run whose entry was released is dropped; one whose
    /// entry is loading, failed or held keeps waiting.
    pub(crate) fn take_loaded(&mut self) -> Vec<Box<dyn FnOnce(&mut crate::Session) + Send>> {
        let slots = &self.slots;
        self.when_loaded.retain(|run| slots.contains_key(&run.key));
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.when_loaded)
            .into_iter()
            .partition(|run| {
                slots
                    .get(&run.key)
                    .is_some_and(|slot| slot.declared && matches!(slot.state, PoolState::Loaded))
            });
        self.when_loaded = waiting;
        ready.into_iter().map(|run| run.then).collect()
    }

    /// How many waits are registered.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.waiters.len()
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
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
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
