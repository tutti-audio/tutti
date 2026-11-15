//! What a kind of pool entry declares, and how the app names a set of them.

use std::any::{Any, TypeId};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
}

/// The description of a load: work for the session's spawner that ends in the
/// loaded value or a [`LoadError`].
#[must_use = "a load runs when the pool declares its entry"]
pub struct Load<T> {
    work: Box<dyn FnOnce() -> Result<T, LoadError> + Send>,
}

impl<T: Send + 'static> Load<T> {
    /// A load that runs `work` on a thread of the session's spawner.
    pub fn on_worker(work: impl FnOnce() -> Result<T, LoadError> + Send + 'static) -> Self {
        Self {
            work: Box::new(work),
        }
    }

    pub(crate) fn erase(self) -> Load<Value> {
        Load::on_worker(move || (self.work)().map(|loaded| Box::new(loaded) as Value))
    }
}

impl<T> Load<T> {
    pub(crate) fn run(self) -> Result<T, LoadError> {
        (self.work)()
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
    /// The session's clock cannot serve the entry (a capture on a system
    /// clock).
    #[error("the session's clock cannot serve this entry")]
    ClockCannotServe,
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

/// One declared entry, its kind and its load erased.
pub(crate) struct Item<K> {
    pub(crate) key: K,
    pub(crate) kind: TypeId,
    pub(crate) identity: Box<dyn ErasedIdentity>,
    pub(crate) load: Box<dyn FnOnce() -> Load<Value> + Send>,
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
}

impl<K, E: PoolEntry> Declared<K, E> {
    /// `entry`, declared under `key`.
    pub const fn new(key: K, entry: E) -> Self {
        Self { key, entry }
    }
}

/// What [`Session::pool`](crate::Session::pool) takes: an entry, a tuple of
/// them, or a collection of them.
pub trait IntoPool<K> {
    /// Adds the entries to `set`, in order.
    fn into_pool(self, set: &mut PoolSet<K>);
}

impl<K, E: PoolEntry> IntoPool<K> for Declared<K, E> {
    fn into_pool(self, set: &mut PoolSet<K>) {
        let Self { key, entry } = self;
        set.items.push(Item {
            key,
            kind: TypeId::of::<E>(),
            identity: Box::new(entry.identity()),
            load: Box::new(move || entry.load().erase()),
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
            #[allow(non_snake_case)]
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
