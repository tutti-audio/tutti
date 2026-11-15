//! What a kind of pool entry declares, and how the app names a set of them.

use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tutti_rt::MainThreadMarker;

use crate::pool::open::OpenCx;
use crate::{JobFailed, PeerError, Presence};

/// What the session holds of a loaded entry, whatever its kind.
pub(crate) type Value = Box<dyn Any + Send>;

/// The declaration of one kind of loaded thing the session keeps for the app.
///
/// A kind says three things of itself. Its [`identity`](Self::identity) is what
/// loads it: the session compares it with the identity the key was declared
/// with last, and a key whose identity is unchanged keeps its entry, loaded
/// once. Its [`load`](Self::load) describes the job that makes the
/// [`Loaded`](Self::Loaded) value the session holds while the entry lives. Its
/// [`handle`](Self::handle) reads the loaded value for the app.
///
/// What the app only starts a load with (a plugin's initial state, say) is
/// not identity: it is applied by the load and never compared. The kind is
/// part of the identity, so one key declared as another kind is a changed
/// identity.
///
/// A kind with something live behind it (a plugin, a capture, a MIDI port)
/// makes its `Handle` a [`Peer`](crate::Peer), whose
/// [`Cause`](crate::Peer::Cause) is the kind's own reason it is not live; a
/// data kind (a wave, a SoundFont) makes it the loaded value or a view of it.
/// A peer handle keeps the entry's [`Life`] and reads
/// [`Presence::Closed`] through [`Life::presence`] once the entry is
/// released.
pub trait PoolEntry: Send + 'static {
    /// What loads the entry: equal identities under one key are one entry.
    type Identity: Eq + fmt::Debug + Send + 'static;
    /// What the load makes and the session holds.
    type Loaded: Send + 'static;
    /// What the app reads of a loaded entry.
    type Handle;

    /// The entry's identity.
    fn identity(&self) -> Self::Identity;

    /// The job that loads the entry, described and not started.
    fn load(&self) -> Load<Self::Loaded>;

    /// The handle for a loaded value. `life` is the entry's: it reads closed
    /// once the entry is released or swapped to another identity.
    fn handle(loaded: &Self::Loaded, life: &Life) -> Self::Handle;

    /// Whether the live thing behind a loaded value is gone: what a peer
    /// handle reads as [`Presence::Lost`] (a plugin that gave up, a capture
    /// whose device left). The kind decides presence; the pool only asks, and
    /// [`Session::retry`](crate::Session::retry) reopens such an entry by
    /// running [`load`](Self::load) again.
    ///
    /// This is the one definition: a handle's `presence()` must read `Lost`
    /// exactly when `is_lost` is true of the loaded value it was made from,
    /// by calling it or by reading the same state. The default is `false`: a
    /// data kind is never lost.
    fn is_lost(_loaded: &Self::Loaded) -> bool {
        false
    }
}

/// The description of a load: the stages that end in the loaded value or a
/// [`LoadError`], and the thread each runs on.
///
/// - [`on_worker`](Self::on_worker): one stage on a thread of the session's
///   spawner, which finishes the load there.
/// - [`on_worker_then_main`](Self::on_worker_then_main): a worker stage whose
///   output goes by value to a stage on the main thread, which finishes it.
/// - [`on_session`](Self::on_session): one stage on the session's own thread,
///   inside [`Session::update`](crate::Session::update), with no worker stage.
/// - [`ready`](Self::ready): no stage; the value lands when the pool declares
///   the entry.
#[must_use = "a load runs when the pool declares its entry"]
pub struct Load<T> {
    pub(crate) step: Step<T>,
    /// Whether a worker stage hands on to a main-thread stage.
    main: bool,
}

/// What a load's stages are, by the thread the first one runs on.
pub(crate) enum Step<T> {
    Ready(T),
    Worker(Box<dyn FnOnce() -> Result<Staged, LoadError> + Send>),
    Session(Box<dyn FnOnce(&mut OpenCx<'_>) -> Result<T, LoadError> + Send>),
}

/// What a worker stage hands on: the finished value, or the main-thread stage
/// that finishes it.
pub(crate) enum Staged {
    Done(Value),
    Main(MainStage),
}

/// Hands work to the main thread's next tick: the session's main-work
/// collector, when it has one.
pub(crate) type Defer = Arc<dyn Fn(Box<dyn FnOnce(MainThreadMarker) + Send>) + Send + Sync>;

/// Main-thread stages whose worker output was neither run nor dropped on the
/// main thread, and that nothing could defer to it: leaked.
static STRANDED: AtomicU64 = AtomicU64::new(0);

/// How many worker outputs the pool leaked, process-wide since the process
/// began: outputs of a load's worker stage that were dropped off the main
/// thread with no main-work collector to hand them to
/// (`tutti_plugin_types::MainWork::stranded` counts those a collector could not
/// run). A diagnostic only.
pub fn stranded_loads() -> u64 {
    STRANDED.load(Ordering::Relaxed)
}

/// A load's main-thread stage with the worker's output inside it. It runs or
/// is discarded only with a [`MainThreadMarker`], so the worker's output is
/// consumed or dropped on the main thread.
///
/// Dropped without either, the stage drops nowhere else: on the main thread it
/// discards the output; elsewhere it hands the discard to the session's
/// main-work collector, and with none the output is leaked and counted
/// ([`stranded_loads`]).
pub(crate) struct MainStage {
    stage:
        Option<Box<dyn FnOnce(MainThreadMarker, Turn) -> Option<Result<Value, LoadError>> + Send>>,
    defer: Option<Defer>,
}

/// What a [`MainStage`] is asked to do with the worker's output.
enum Turn {
    Run,
    Discard,
}

impl MainStage {
    pub(crate) fn new<T: Send + 'static, W: Send + 'static>(
        output: W,
        main: impl FnOnce(W, MainThreadMarker) -> Result<T, LoadError> + Send + 'static,
    ) -> Self {
        Self {
            stage: Some(Box::new(move |marker, turn| match turn {
                Turn::Run => Some(main(output, marker).map(|done| Box::new(done) as Value)),
                Turn::Discard => {
                    drop(output);
                    None
                }
            })),
            defer: None,
        }
    }

    /// This stage, with `defer` to hand its discard to when it is dropped off
    /// the main thread.
    pub(crate) fn deferring(mut self, defer: Option<Defer>) -> Self {
        self.defer = defer;
        self
    }

    /// Runs the stage on the main thread.
    pub(crate) fn run(mut self, main: MainThreadMarker) -> Result<Value, LoadError> {
        let stage = self.stage.take().expect("a stage is run once");
        stage(main, Turn::Run).expect("a stage asked to run answers")
    }

    /// Drops the worker's output on the main thread, running nothing.
    pub(crate) fn discard(mut self, main: MainThreadMarker) {
        if let Some(stage) = self.stage.take() {
            let _ = stage(main, Turn::Discard);
        }
    }
}

impl Drop for MainStage {
    fn drop(&mut self) {
        let Some(stage) = self.stage.take() else {
            return;
        };
        if let Some(main) = MainThreadMarker::new() {
            let _ = stage(main, Turn::Discard);
        } else if let Some(defer) = self.defer.take() {
            defer(Box::new(move |main| {
                let _ = stage(main, Turn::Discard);
            }));
        } else {
            STRANDED.fetch_add(1, Ordering::Relaxed);
            // Dropping it here would drop the worker's output off the main thread.
            std::mem::forget(stage);
        }
    }
}

impl Staged {
    /// This output, its main stage (if it has one) handing its discard to
    /// `defer` when dropped off the main thread.
    pub(crate) fn deferring(self, defer: Option<Defer>) -> Self {
        match self {
            Self::Done(done) => Self::Done(done),
            Self::Main(stage) => Self::Main(stage.deferring(defer)),
        }
    }
}

impl<T: Send + 'static> Load<T> {
    /// A load that finishes in one stage: `work`, on a thread of the session's
    /// spawner.
    pub fn on_worker(work: impl FnOnce() -> Result<T, LoadError> + Send + 'static) -> Self {
        Self {
            step: Step::Worker(Box::new(move || {
                work().map(|done| Staged::Done(Box::new(done) as Value))
            })),
            main: false,
        }
    }

    /// A load of a worker stage and then a main-thread stage: `work` runs on a
    /// thread of the session's spawner, and its output `W` goes by value to
    /// `main`, which runs on the main thread in an
    /// [`update`](crate::Session::update) on it and makes the loaded value.
    ///
    /// The entry reads [`PoolState::WaitingForMainThread`](crate::PoolState::WaitingForMainThread)
    /// between the stages, and [`Session::pool`](crate::Session::pool) refuses
    /// the entry with [`PoolError::NeedsMainThread`](crate::PoolError::NeedsMainThread)
    /// when no update has run on the main thread and the caller is not on it.
    ///
    /// `W` is never dropped off the main thread. A load cancelled or
    /// superseded between the stages never runs `main`: its `W` is dropped on
    /// the main thread, by the queue that would have run `main`. `W` dropped
    /// anywhere else (a session that ends off the main thread, the jobs'
    /// channel closing) is handed to the session's main-work collector to drop
    /// at its next tick on the main thread when the session has one (feature
    /// `plugin`); with none, or when the collector itself ends off the main
    /// thread, `W` is leaked and counted
    /// ([`stranded_loads`]).
    pub fn on_worker_then_main<W: Send + 'static>(
        work: impl FnOnce() -> Result<W, LoadError> + Send + 'static,
        main: impl FnOnce(W, MainThreadMarker) -> Result<T, LoadError> + Send + 'static,
    ) -> Self {
        Self {
            step: Step::Worker(Box::new(move || {
                work().map(|output| Staged::Main(MainStage::new(output, main)))
            })),
            main: true,
        }
    }

    /// A load that is one stage on the session's own thread, run inside the
    /// [`update`](crate::Session::update) after the entry is declared, with no
    /// worker stage. `open` is given what the session thread owns that an open
    /// needs ([`OpenCx`]); the entry reads
    /// [`PoolState::Opening`](crate::PoolState::Opening) until it runs.
    pub fn on_session(
        open: impl FnOnce(&mut OpenCx<'_>) -> Result<T, LoadError> + Send + 'static,
    ) -> Self {
        Self {
            step: Step::Session(Box::new(open)),
            main: false,
        }
    }

    /// A load with nothing to do: `value` lands when the pool declares the
    /// entry, as a provided `Arc` does.
    pub const fn ready(value: T) -> Self {
        Self {
            step: Step::Ready(value),
            main: false,
        }
    }

    /// Whether the load has a main-thread stage.
    pub(crate) const fn needs_main(&self) -> bool {
        self.main
    }

    pub(crate) fn erase(self) -> Load<Value> {
        let step = match self.step {
            Step::Ready(value) => Step::Ready(Box::new(value) as Value),
            Step::Worker(work) => Step::Worker(work),
            Step::Session(open) => Step::Session(Box::new(move |cx| {
                open(cx).map(|done| Box::new(done) as Value)
            })),
        };
        Load {
            step,
            main: self.main,
        }
    }
}

impl<T> fmt::Debug for Load<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Load").finish_non_exhaustive()
    }
}

/// Whether a pool entry is still the one its handle was made for: the
/// handle's way to read [`Presence::Closed`] and to fail a request with
/// [`PeerError::Released`].
///
/// The session closes it when the entry is released and when the entry is
/// swapped to another identity. It cannot be reopened, and it is cheap to
/// clone into every handle.
#[derive(Clone, Debug)]
pub struct Life(Arc<AtomicBool>);

impl Life {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn close(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the entry was released or swapped.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// `live`'s reading while the entry is the handle's, [`Presence::Closed`]
    /// after.
    #[must_use]
    pub fn presence<C>(&self, live: impl FnOnce() -> Presence<C>) -> Presence<C> {
        if self.is_closed() {
            Presence::Closed
        } else {
            live()
        }
    }

    /// Whether a request may go to the peer.
    ///
    /// # Errors
    ///
    /// [`PeerError::Released`] once the entry is released or swapped.
    pub fn check<E>(&self) -> Result<(), PeerError<E>> {
        if self.is_closed() {
            Err(PeerError::Released)
        } else {
            Ok(())
        }
    }
}

/// Why an entry did not load.
///
/// The kinds add their own failures (a plugin's, a capture's, a MIDI port's)
/// to the common ones here. Clone, so the entry's [`PoolState::Failed`](crate::PoolState::Failed) and
/// [`PoolEvent::Failed`](crate::PoolEvent::Failed) both carry it.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// A file could not be decoded.
    #[error(transparent)]
    Decode(Arc<tutti_codec::DecodeError>),
    /// A SoundFont could not be opened.
    #[cfg(feature = "soundfont")]
    #[error(transparent)]
    SoundFont(Arc<tutti_sf2::Error>),
    /// A plugin did not open.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Plugin(Arc<tutti_plugin_types::BridgeError>),
    /// The plugin's id names no plugin of the catalog, or several.
    #[cfg(feature = "plugin")]
    #[error(transparent)]
    Unresolved(tutti_plugin::catalog::BindError),
    /// The landed plugin has other ports than the node over the entry, which
    /// keeps the plugin it has.
    #[cfg(feature = "plugin")]
    #[error("the plugin has {got} where its node has {was}")]
    PluginPorts {
        /// The landed plugin's ports.
        got: crate::PluginPorts,
        /// The node's.
        was: crate::PluginPorts,
    },
    /// The graph refused the node's swap to the landed plugin.
    #[cfg(feature = "plugin")]
    #[error("the node could not swap to the landed plugin: {0}")]
    PluginSwap(Arc<crate::CrossfadeRefused>),
    /// The load's job did not finish: it panicked, or the spawner dropped it.
    #[error(transparent)]
    Job(#[from] JobFailed),
}

/// An erased identity: compared with another of the same kind.
pub(crate) trait ErasedIdentity: Send + fmt::Debug {
    fn as_any(&self) -> &dyn Any;
    fn same(&self, other: &dyn ErasedIdentity) -> bool;
}

impl<T: Eq + fmt::Debug + Send + 'static> ErasedIdentity for T {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn same(&self, other: &dyn ErasedIdentity) -> bool {
        other.as_any().downcast_ref::<T>() == Some(self)
    }
}

/// What loads an entry again: the kind's `load`, kept for a retry.
pub(crate) type Reload = Box<dyn Fn() -> Load<Value> + Send>;

/// Another owner of a loaded value, for an entry declared with
/// [`Declared::shared`].
pub(crate) type Cloner = fn(&Value) -> Option<Value>;

/// One declared entry, its kind and its load erased.
pub(crate) struct Item<K> {
    pub(crate) key: K,
    pub(crate) kind: TypeId,
    pub(crate) identity: Box<dyn ErasedIdentity>,
    /// The load, described when the set was made.
    pub(crate) load: Load<Value>,
    pub(crate) reload: Reload,
    pub(crate) lost: fn(&Value) -> bool,
    pub(crate) share: Option<Cloner>,
}

/// The entries of one [`Session::pool`](crate::Session::pool) call, in
/// declaration order.
pub struct PoolSet<K> {
    pub(crate) items: Vec<Item<K>>,
}

impl<K> PoolSet<K> {
    pub(crate) const fn new() -> Self {
        Self { items: Vec::new() }
    }
}

impl<K> fmt::Debug for PoolSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolSet")
            .field("entries", &self.items.len())
            .finish()
    }
}

/// One entry, declared under the app's key `K`: what a kind's builder
/// (`audio_file(key, path)`) returns.
#[must_use = "an entry is declared by passing it to Session::pool"]
#[derive(Clone, Debug)]
pub struct Declared<K, E> {
    key: K,
    entry: E,
    share: Option<Cloner>,
}

impl<K, E: PoolEntry> Declared<K, E> {
    /// `entry`, declared under `key`: one instance per key, even when another
    /// key is declared with the same identity.
    pub const fn new(key: K, entry: E) -> Self {
        Self {
            key,
            entry,
            share: None,
        }
    }

    /// `entry`, declared under `key` as a resource that shares: another key
    /// declared with the same kind and identity, and shared too, shares this
    /// one's load, and every handle reads the same loaded value (a
    /// [`Clone`] of it, an `Arc`). The data kinds (`audio_file` and
    /// `soundfont`) are declared this way; a kind with
    /// something live behind it (a plugin client, a capture) uses
    /// [`new`](Self::new).
    ///
    /// Every key's handle reads a clone of the loaded value, and the clones drop
    /// on whatever thread releases their key: a kind whose loaded value must
    /// drop on the main thread must not be shared.
    pub const fn shared(key: K, entry: E) -> Self
    where
        E::Loaded: Clone,
    {
        Self {
            key,
            entry,
            share: Some(clone_loaded::<E>),
        }
    }
}

fn clone_loaded<E: PoolEntry>(value: &Value) -> Option<Value>
where
    E::Loaded: Clone,
{
    value
        .downcast_ref::<E::Loaded>()
        .map(|loaded| Box::new(loaded.clone()) as Value)
}

fn lost<E: PoolEntry>(value: &Value) -> bool {
    value.downcast_ref::<E::Loaded>().is_some_and(E::is_lost)
}

/// What [`Session::pool`](crate::Session::pool) takes: an entry, a tuple of
/// them, or a collection of them.
pub trait IntoPool<K> {
    /// Adds the entries to `set`, in order.
    fn into_pool(self, set: &mut PoolSet<K>);
}

impl<K, E: PoolEntry> IntoPool<K> for Declared<K, E> {
    fn into_pool(self, set: &mut PoolSet<K>) {
        let Self { key, entry, share } = self;
        set.items.push(Item {
            key,
            kind: TypeId::of::<E>(),
            identity: Box::new(entry.identity()),
            load: entry.load().erase(),
            lost: lost::<E>,
            share,
            reload: Box::new(move || entry.load().erase()),
        });
    }
}

impl<K> IntoPool<K> for () {
    fn into_pool(self, _: &mut PoolSet<K>) {}
}

impl<K, T: IntoPool<K>> IntoPool<K> for Vec<T> {
    fn into_pool(self, set: &mut PoolSet<K>) {
        for part in self {
            part.into_pool(set);
        }
    }
}

impl<K, T: IntoPool<K>, const N: usize> IntoPool<K> for [T; N] {
    fn into_pool(self, set: &mut PoolSet<K>) {
        for part in self {
            part.into_pool(set);
        }
    }
}

macro_rules! tuples {
    ($(($($part:ident),+))+) => {$(
        impl<K, $($part: IntoPool<K>),+> IntoPool<K> for ($($part,)+) {
            #[expect(non_snake_case, reason = "the parts are named by their type parameters")]
            fn into_pool(self, set: &mut PoolSet<K>) {
                let ($($part,)+) = self;
                $($part.into_pool(set);)+
            }
        }
    )+};
}

tuples! {
    (A)
    (A, B)
    (A, B, C)
    (A, B, C, D)
    (A, B, C, D, E)
    (A, B, C, D, E, F)
    (A, B, C, D, E, F, G)
    (A, B, C, D, E, F, G, H)
    (A, B, C, D, E, F, G, H, I)
    (A, B, C, D, E, F, G, H, I, J)
    (A, B, C, D, E, F, G, H, I, J, L)
    (A, B, C, D, E, F, G, H, I, J, L, M)
}
