//! The session's pool: the declared set diffed by key and identity, and each
//! entry's life of declared, held and released.

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Weak};

use tutti_buffers::Float;
use tutti_platform::MainThreadMarker;

use crate::job::Ticket;
use crate::pool::config::PoolConfig;
use crate::pool::entry::{
    Cloner, ErasedIdentity, Item, Life, LoadError, MainStage, PoolEntry, PoolLoad, PoolSet, Reload,
    Staged, Step, Value,
};
use crate::pool::key::PoolKey;
use crate::pool::report::{PoolOutcome, PoolReport};
use crate::{Clock, PoolLoaded, SessionEvent};

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
    /// does. A failed reload leaves no value: the old one is dropped. A kind
    /// whose holder refuses a value that landed (a node over a plugin with
    /// other ports) reads failed too, with its own error, after the landing's
    /// [`PoolEvent::Loaded`] or [`PoolEvent::Reloaded`] in the same update, or a
    /// later one when the swap waits for the main thread or a re-prepare.
    Failed(LoadError),
    /// No longer declared, and kept by a node or a snapshot until their last
    /// hold is dropped.
    Held,
}

/// An entry's change, as [`SessionEvent::Pool`] reports it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PoolEvent {
    /// The entry's load landed with no value before it: the first load, or the
    /// first after a failed or refused one (a `retry`, a new identity).
    Loaded,
    /// A reload landed: the entry's identity changed and the new value
    /// replaced the old one. It does not promise that whatever holds the old
    /// value (a node over a plugin) took the new one: a kind whose holder can
    /// refuse the landed value fails the entry, as [`Failed`](Self::Failed),
    /// after this event in the same update, or in a later one when the
    /// holder's refusal waits for the main thread or a re-prepare.
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

/// Why a key's reader (`Session::midi_input`, `midi_output`, `midi_sink`) found
/// nothing.
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

/// What a [`Hold`] is for: whether the entry parks when the hold is old.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, feature = "plugin")),
    expect(
        dead_code,
        reason = "a node over an entry (feature `plugin`) and a snapshot take holds"
    )
)]
pub(crate) enum HoldKind {
    /// A live node reads the entry: it stays loaded while the hold lasts.
    Node,
    /// A snapshot recorded the entry: past the depth it parks.
    Snapshot,
}

/// A claim on a pool entry that keeps it held, not released, after the app's
/// pool stops naming it: a node over it, a snapshot. Dropping it is the hold's
/// end; the entry is released at the next update that finds no hold left.
#[derive(Clone, Debug)]
pub(crate) struct Hold {
    _claim: Arc<()>,
    _pin: Option<Arc<()>>,
    _rank: Option<Arc<()>>,
}

/// A snapshot hold's place among the newest: held weakly, so the hold's drop
/// ends it.
struct Rank {
    seq: u64,
    key: PoolKey,
    tag: Weak<()>,
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
struct Slot<S: Float> {
    kind: TypeId,
    identity: Box<dyn ErasedIdentity>,
    /// The app's pool names it.
    declared: bool,
    /// Its place in the set that last named it.
    order: usize,
    /// One strong count for the slot, one for each [`Hold`].
    holds: Arc<()>,
    /// One strong count for the slot, one for each node [`Hold`].
    pins: Arc<()>,
    /// The value was released by the kind's [`retain`](PoolEntry::retain); the
    /// next declaration loads it again.
    parked: bool,
    /// What the kind does with the value when the entry parks.
    retain: fn(Value) -> Option<Value>,
    life: Life,
    loaded: Option<Value>,
    state: PoolState,
    /// The pending load stands at its main-thread stage (read while
    /// [`pending`](Self::pending)).
    at_main: bool,
    /// The outcome that landed while the entry was held: reported when the
    /// pool declares the entry again.
    unreported: Option<PoolEvent>,
    /// The load that may land: an older one is stale.
    generation: u64,
    /// The kind's load, again, for a retry.
    reload: Reload<S>,
    /// Whether the kind reads the loaded value as lost.
    lost: fn(&Value) -> bool,
    /// How another key's entry of the same identity gets this one's value.
    share: Option<Cloner>,
}

impl<S: Float> Slot<S> {
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

    /// No node holds it.
    fn unpinned(&self) -> bool {
        Arc::strong_count(&self.pins) == 1
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

    /// A load of this entry waits for the main thread.
    const fn waits_on_main(&self) -> bool {
        self.at_main && self.pending()
    }

    /// The state while a load that stands at `stage` is pending: the old
    /// value, if there is one, stays the entry's.
    fn awaiting(&mut self, stage: Stage) {
        self.at_main = matches!(stage, Stage::Main);
        self.state = if self.loaded.is_some() {
            PoolState::Reloading
        } else {
            stage.state()
        };
    }
}

/// A load the diff started: the session runs it and lands the result under
/// the same flight.
pub(crate) struct Start<S: Float> {
    pub(crate) flight: u64,
    pub(crate) load: PoolLoad<Value, S>,
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

/// A worker stage the pool let start: its flight and the stage.
pub(crate) struct Worker {
    pub(crate) flight: u64,
    pub(crate) work: Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>,
}

/// A worker stage waiting for a free worker.
struct Queued {
    flight: u64,
    work: Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>,
}

/// A stage on the session's thread, waiting for the next update.
pub(crate) struct Opening<S: Float> {
    pub(crate) flight: u64,
    pub(crate) open: Box<dyn FnOnce(&mut crate::OpenCx<'_, S>) -> Result<Value, LoadError> + Send>,
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
struct WhenLoaded<A: crate::AppKey, C: Clock> {
    seq: u64,
    key: PoolKey,
    then: Box<dyn FnOnce(&mut crate::Session<A, C>) + Send>,
}

struct Pending {
    order: usize,
    key: PoolKey,
    event: PoolEvent,
}

/// The pool's entries by key, which a wait reads.
type Slots<S> = BTreeMap<PoolKey, Slot<S>>;

/// The pool: every key's entry, the loads in flight, and the events not yet
/// reported.
pub(crate) struct Pool<A: crate::AppKey, C: Clock> {
    config: PoolConfig,
    slots: Slots<C::Float>,
    last_generation: u64,
    flights: BTreeMap<u64, Flight>,
    /// Worker stages not started, in declaration order.
    queue: VecDeque<Queued>,
    /// Worker stages started and not landed.
    running: usize,
    openings: Vec<Opening<C::Float>>,
    main: Vec<MainItem>,
    /// An update has run on the main thread: the stages that wait for it run.
    main_seen: bool,
    queued: Vec<Pending>,
    /// Released since the last report.
    released: Vec<PoolKey>,
    /// The snapshot holds, with their order.
    ranks: Vec<Rank>,
    last_rank: u64,
    waiters: Vec<Box<dyn FnMut(&Slots<C::Float>) -> bool + Send>>,
    /// Runs waiting for an entry to load, in the order they were asked.
    when_loaded: Vec<WhenLoaded<A, C>>,
    last_when: u64,
}

impl<A: crate::AppKey, C: Clock> fmt::Debug for Pool<A, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("entries", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl<A: crate::AppKey, C: Clock> Pool<A, C> {
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
            ranks: Vec::new(),
            last_rank: 0,
            waiters: Vec::new(),
            when_loaded: Vec::new(),
            last_when: 0,
        }
    }

    /// The same pool for a session on clock `N`: every entry, load in flight,
    /// stage and wait carries over. The runs waiting for an entry to load
    /// ([`when_loaded`](Self::when_loaded)) are typed to the session they were
    /// asked of and do not: the caller has run or refused them first
    /// ([`waiting_runs`](Self::waiting_runs)).
    #[cfg(feature = "device")]
    pub(crate) fn rebind<N: Clock<Float = C::Float>>(self) -> Pool<A, N> {
        debug_assert!(
            self.when_loaded.is_empty(),
            "no run waits when the session changes clock"
        );
        Pool {
            config: self.config,
            slots: self.slots,
            last_generation: self.last_generation,
            flights: self.flights,
            queue: self.queue,
            running: self.running,
            openings: self.openings,
            main: self.main,
            main_seen: self.main_seen,
            queued: self.queued,
            released: self.released,
            ranks: self.ranks,
            last_rank: self.last_rank,
            waiters: self.waiters,
            when_loaded: Vec::new(),
            last_when: self.last_when,
        }
    }

    /// Whether a run is due: its entry is declared and loaded.
    #[cfg(feature = "device")]
    pub(crate) fn has_due_run(&self) -> bool {
        self.when_loaded.iter().any(|run| {
            self.slots
                .get(&run.key)
                .is_some_and(|slot| slot.declared && matches!(slot.state, PoolState::Loaded))
        })
    }

    /// The entries the runs not yet run wait for, in the order asked: a run
    /// whose entry is loading, failed or held. (One whose entry is loaded runs
    /// at the next update; one whose entry was released is dropped there.)
    #[cfg(feature = "device")]
    pub(crate) fn waiting_runs(&self) -> Vec<PoolKey> {
        self.when_loaded
            .iter()
            .filter(|run| {
                self.slots
                    .get(&run.key)
                    .is_some_and(|slot| !(slot.declared && matches!(slot.state, PoolState::Loaded)))
            })
            .map(|run| run.key.clone())
            .collect()
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
        set: PoolSet<K, C::Float>,
    ) -> Result<Vec<Start<C::Float>>, PoolError<K>> {
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

    fn declare_one<K>(
        &mut self,
        key: &PoolKey,
        order: usize,
        item: Item<K, C::Float>,
    ) -> Option<Start<C::Float>> {
        let Item {
            kind,
            identity,
            load,
            reload,
            lost,
            retain,
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
                    pins: Arc::new(()),
                    parked: false,
                    retain,
                    life: Life::new(),
                    loaded: None,
                    state: PoolState::Loading,
                    at_main: false,
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
        slot.retain = retain;
        slot.share = share;
        if slot.parked {
            // Its value was released: the recall loads it again.
            slot.parked = false;
            slot.unreported = None;
            slot.kind = kind;
            slot.identity = identity;
            return self.begin(key, load);
        }
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
    fn begin(&mut self, key: &PoolKey, load: PoolLoad<Value, C::Float>) -> Option<Start<C::Float>> {
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
                && slot.declared
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
        self.park_overflow();
    }

    /// Parks the loaded, undeclared entries that no node holds and that only
    /// snapshot holds outside the newest [`PoolConfig::live_holds`] keep. A
    /// kind that keeps its value keeps it.
    fn park_overflow(&mut self) {
        self.ranks.retain(|rank| rank.tag.strong_count() > 0);
        let mut newest: Vec<&Rank> = self.ranks.iter().collect();
        newest.sort_by_key(|rank| std::cmp::Reverse(rank.seq));
        let live: BTreeSet<&PoolKey> = newest
            .iter()
            .take(self.config.live_holds)
            .map(|rank| &rank.key)
            .collect();
        for (key, slot) in &mut self.slots {
            let parkable = !slot.declared
                && !live.contains(key)
                && slot.unpinned()
                && matches!(slot.state, PoolState::Loaded);
            if !parkable {
                continue;
            }
            let Some(value) = slot.loaded.take() else {
                continue;
            };
            slot.loaded = (slot.retain)(value);
            if slot.loaded.is_none() {
                slot.life.close();
                slot.life = Life::new();
                slot.parked = true;
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
    pub(crate) fn next_worker(&mut self) -> Option<Worker> {
        while self.running < self.config.parallel_loads.get() {
            let Queued { flight, work } = self.queue.pop_front()?;
            if self.live(flight) {
                self.running += 1;
                return Some(Worker { flight, work });
            }
            self.flights.remove(&flight);
        }
        None
    }

    /// A started worker stage ended.
    pub(crate) fn worker_ended(&mut self) {
        self.running -= 1;
    }

    pub(crate) fn open(&mut self, opening: Opening<C::Float>) {
        self.openings.push(opening);
    }

    pub(crate) fn take_openings(&mut self) -> Vec<Opening<C::Float>> {
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

    /// Refuses a landed value the graph could not take: its value is dropped,
    /// its handles read closed, and the entry reads failed with `error`
    /// ([`PoolEvent::Failed`], after the landing's event). `generation` is the
    /// landing that was refused ([`landing_of`](Self::landing_of)): a refusal of
    /// a landing that a newer declaration or a pending load has since
    /// superseded is stale and records nothing, so it cannot clobber the load
    /// in flight. Returns whether the entry failed.
    #[cfg(feature = "plugin")]
    pub(crate) fn refuse_landed(
        &mut self,
        key: &PoolKey,
        generation: u64,
        error: LoadError,
    ) -> bool {
        let Some(slot) = self
            .slots
            .get_mut(key)
            .filter(|slot| slot.generation == generation && slot.loaded.is_some())
        else {
            return false;
        };
        slot.loaded = None;
        slot.life.close();
        slot.life = Life::new();
        slot.state = PoolState::Failed(error.clone());
        let event = PoolEvent::Failed(error);
        if slot.declared {
            self.queued.push(Pending {
                order: slot.order,
                key: key.clone(),
                event,
            });
        } else {
            slot.unreported = Some(event);
        }
        true
    }

    /// The keys whose load landed since the last report, in landing order.
    #[cfg(feature = "plugin")]
    pub(crate) fn landed_keys(&self) -> Vec<PoolKey> {
        self.queued
            .iter()
            .filter(|queued| matches!(queued.event, PoolEvent::Loaded | PoolEvent::Reloaded))
            .map(|queued| queued.key.clone())
            .collect()
    }

    /// Loads the declared entry under `key` again: one that failed, or one the
    /// kind reads as lost.
    ///
    /// # Errors
    ///
    /// [`RetryError::NotPooled`] when the key is not declared;
    /// [`RetryError::NotFailed`] when its entry is neither failed nor lost.
    pub(crate) fn retry<K>(&mut self, key: &K) -> Result<Option<Start<C::Float>>, RetryError<K>>
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
    fn report<K: Clone + 'static>(slots: &Slots<C::Float>) -> Option<PoolReport<K>> {
        let mut entries: Vec<(usize, K, PoolOutcome)> = Vec::new();
        for (key, slot) in slots.iter().filter(|(_, slot)| slot.declared) {
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
    /// The wait ends when its [`Job`](crate::Job) is dropped.
    pub(crate) fn wait_loaded<K: Clone + Send + 'static>(&mut self, ticket: Ticket<PoolLoaded<K>>) {
        let mut ticket = Some(ticket);
        let mut waiter = move |slots: &Slots<C::Float>| {
            let Some(ticket_ref) = ticket.as_ref().filter(|t| !t.cancelled()) else {
                return true;
            };
            let Some(report) = Self::report::<K>(slots) else {
                ticket_ref.set_main_wait(
                    slots
                        .values()
                        .any(|slot| slot.declared && slot.waits_on_main()),
                );
                return false;
            };
            if let Some(ticket) = ticket.take() {
                ticket.land(Ok(report));
            }
            true
        };
        if !waiter(&self.slots) {
            self.waiters.push(Box::new(waiter));
        }
    }

    /// Lands the waits whose entries all have an outcome, and ends those whose
    /// handle was dropped.
    pub(crate) fn poll_waiters(&mut self) {
        let mut waiters = std::mem::take(&mut self.waiters);
        waiters.retain_mut(|waiter| !waiter(&self.slots));
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
        then: Box<dyn FnOnce(&mut crate::Session<A, C>) + Send>,
    ) -> Result<(), WhenLoadedError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let erased = PoolKey::new(key);
        if !self.slots.get(&erased).is_some_and(|slot| slot.declared) {
            return Err(WhenLoadedError::NotPooled(key.clone()));
        }
        self.last_when += 1;
        self.when_loaded.push(WhenLoaded {
            seq: self.last_when,
            key: erased,
            then,
        });
        Ok(())
    }

    /// The next run, of those asked up to `through`, whose entry is declared
    /// and loaded now, taken out. A run whose entry was released is dropped;
    /// one whose entry is loading, failed or held keeps waiting. The caller
    /// asks again after each run, so a run sees what the one before it did.
    pub(crate) fn next_loaded(
        &mut self,
        through: u64,
    ) -> Option<Box<dyn FnOnce(&mut crate::Session<A, C>) + Send>> {
        let slots = &self.slots;
        self.when_loaded.retain(|run| slots.contains_key(&run.key));
        let at = self.when_loaded.iter().position(|run| {
            run.seq <= through
                && slots
                    .get(&run.key)
                    .is_some_and(|slot| slot.declared && matches!(slot.state, PoolState::Loaded))
        })?;
        Some(self.when_loaded.remove(at).then)
    }

    /// The newest run asked: the bound of one update's runs.
    pub(crate) const fn last_when(&self) -> u64 {
        self.last_when
    }

    /// How many waits are registered.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.waiters.len()
    }

    /// A hold of `kind` on `key`'s entry, `None` when the pool has none.
    #[cfg_attr(
        not(any(test, feature = "plugin")),
        expect(
            dead_code,
            reason = "a node over an entry (feature `plugin`) takes holds"
        )
    )]
    pub(crate) fn hold(&mut self, key: &PoolKey, kind: HoldKind) -> Option<Hold> {
        let slot = self.slots.get(key)?;
        let claim = Arc::clone(&slot.holds);
        let pin = (kind == HoldKind::Node).then(|| Arc::clone(&slot.pins));
        let rank = (kind == HoldKind::Snapshot).then(|| {
            let tag = Arc::new(());
            self.last_rank += 1;
            self.ranks.push(Rank {
                seq: self.last_rank,
                key: key.clone(),
                tag: Arc::downgrade(&tag),
            });
            tag
        });
        Some(Hold {
            _claim: claim,
            _pin: pin,
            _rank: rank,
        })
    }

    /// `E`'s handle for the loaded entry under `key`.
    #[cfg_attr(
        not(any(test, feature = "plugin")),
        expect(
            dead_code,
            reason = "the readers of the kinds (feature `plugin`) are its callers"
        )
    )]
    pub(crate) fn handle<K, E>(&self, key: &K) -> Result<E::Handle, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
        E: PoolEntry<C::Float>,
    {
        let (loaded, life) = self.loaded_with_life::<K, E>(key)?;
        Ok(E::handle(loaded, life))
    }

    /// `E`'s loaded value under `key`, for a reader that makes its own handle.
    #[cfg(feature = "plugin")]
    pub(crate) fn loaded<K, E>(&self, key: &K) -> Result<&E::Loaded, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
        E: PoolEntry<C::Float>,
    {
        self.loaded_with_life::<K, E>(key).map(|(loaded, _)| loaded)
    }

    /// `E`'s loaded value under `key` and the entry's life: what a handle is
    /// made of.
    fn loaded_with_life<K, E>(&self, key: &K) -> Result<(&E::Loaded, &Life), HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
        E: PoolEntry<C::Float>,
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
        Ok((loaded, &slot.life))
    }

    /// `E`'s loaded value under `key` and the generation of the landing that
    /// made it, wherever the entry stands in the app's set (declared or held).
    #[cfg(feature = "plugin")]
    pub(crate) fn landing_of<E: PoolEntry<C::Float>>(
        &self,
        key: &PoolKey,
    ) -> Option<(u64, &E::Loaded)> {
        let slot = self
            .slots
            .get(key)
            .filter(|slot| slot.kind == TypeId::of::<E>())?;
        let loaded = slot.loaded.as_ref()?.downcast_ref::<E::Loaded>()?;
        Some((slot.generation, loaded))
    }
}
