//! The pool: what the app declares loaded, and the session keeps while it is
//! declared.
//!
//! The app declares a set with [`Session::pool`], whole each time. The
//! session diffs it against the set before, by key and by identity
//! ([`PoolEntry::identity`]): a new key starts a load, a key whose identity is
//! unchanged keeps its entry, and a key whose identity changed reloads. A key
//! the set no longer names is **held** while a node over it or a snapshot
//! holds it, and **released** at the last hold's drop; declaring a held key
//! again with the same identity adopts it with no reload.
//!
//! A key declared again with the same identity keeps what it has, a failed
//! load included: [`Session::retry`] loads it again, and also reopens a loaded
//! entry its kind reads as lost. When an identity changes, the old value
//! stays until the new load lands; if that load fails the old value is dropped,
//! its handles read closed, and the entry reads failed.
//!
//! Each kind of entry is a [`PoolEntry`]; `audio_file` (feature `sampler`) and
//! `soundfont` (feature `soundfont`) are the data kinds. A load's outcome lands
//! in [`Session::update`] as a [`SessionEvent::Pool`](crate::SessionEvent::Pool), and
//! [`Session::pool_state`] reads where an entry stands.
//!
//! # Stages
//!
//! A [`Load`] has one to two stages, each on a named thread: a worker of the
//! session's spawner ([`Load::on_worker`]), a worker and then the main thread
//! ([`Load::on_worker_then_main`]), the session's own thread
//! ([`Load::on_session`]), or none ([`Load::ready`]). At most
//! [`PoolConfig::parallel_loads`] worker stages run at once; the rest wait in
//! declaration order. A stage on the main thread or the session's thread runs
//! in an [`update`](Session::update) on it.
//!
//! Two keys of a kind that shares ([`Declared::shared`]) with the same
//! identity share one load and the one loaded value; any other kind is one
//! instance per key.
//!
//! [`Session::pool_loaded`] reports every declared entry's outcome once none
//! is pending, and a bounce refuses while one is
//! (`BounceError::PoolPending`, feature `export`).

use std::fmt;
#[cfg(any(feature = "sampler", feature = "soundfont"))]
use std::sync::Arc;

use crate::Session;

mod config;
#[cfg(any(feature = "sampler", feature = "soundfont"))]
mod data;
mod entry;
mod key;
mod open;
mod report;
mod run;
mod table;

pub use config::PoolConfig;
#[cfg(feature = "sampler")]
pub use data::{AudioFile, audio_file};
#[cfg(feature = "soundfont")]
pub use data::{SoundFontFile, soundfont};
pub use entry::{Declared, IntoPool, Life, Load, LoadError, PoolEntry, PoolSet, stranded_loads};
pub use key::PoolKey;
pub use open::OpenCx;
pub use report::{PoolLoaded, PoolOutcome, PoolReport};
pub(crate) use table::Pool;
pub use table::{HandleError, PoolError, PoolEvent, PoolState, RetryError};

#[cfg(test)]
mod tests;

impl<C: crate::Clock> Session<C> {
    /// Declares what the session keeps loaded: the whole set, each time.
    ///
    /// The set is diffed against the one declared before, by key and by
    /// identity. A new key starts a load on the session's spawner; a key whose
    /// identity is unchanged keeps its entry, loaded once; a key whose
    /// identity changed reloads, its old value staying until the new one
    /// lands; a key the set no longer names is held while a node or a snapshot
    /// holds it and released otherwise. Declaring a held key again with its
    /// identity adopts it, with no reload.
    ///
    /// The declaration is whole across key types too: a call with keys of
    /// another type undeclares every entry declared under the first type.
    /// Declaring a key again with the same identity does not retry a failed
    /// load; [`retry`](Self::retry) does.
    ///
    /// Returns at once. Each load's outcome lands as a
    /// [`SessionEvent::Pool`](crate::SessionEvent::Pool) in a later [`update`](Self::update), those of one
    /// update in the order the set names them, then the releases, by key within a key type.
    /// An outcome that landed while its entry was held is reported when the
    /// pool declares the entry again. At most
    /// [`PoolConfig::parallel_loads`] worker stages run at once.
    ///
    /// # Errors
    ///
    /// [`PoolError::DuplicateKey`] for a key the set names twice;
    /// [`PoolError::NeedsMainThread`] for a key whose own load has a stage on
    /// the main thread ([`Load::on_worker_then_main`]) when no update has run
    /// on the main thread and this call is not on it. A key of the same kind
    /// whose load is [`Load::on_worker`] is not refused. The call refuses the
    /// set whole: nothing of it is applied.
    pub fn pool<K>(&mut self, parts: impl IntoPool<K>) -> Result<(), PoolError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let mut set = PoolSet::new();
        parts.into_pool(&mut set);
        let starts = self.pool.declare(set)?;
        self.pool_start(starts);
        Ok(())
    }

    /// Loads an entry again, with the identity it has: one that
    /// [failed](PoolState::Failed), or a loaded one its kind reads as lost.
    ///
    /// A failed entry loads as it did when declared (the plugin installed
    /// since, the file written). A loaded entry whose handle reads lost
    /// (a plugin that gave up, a device unplugged mid-run) reopens: its load
    /// runs again, and the entry reads [`PoolState::Reloading`] until it lands
    /// as a [`PoolEvent::Reloaded`]. It is the kind that decides whether an
    /// entry is lost ([`PoolEntry::is_lost`]); the data kinds never are.
    ///
    /// # Errors
    ///
    /// [`RetryError::NotFailed`] for a declared key whose entry is neither;
    /// [`RetryError::NotPooled`] for a key the pool does not declare;
    /// [`RetryError::NeedsMainThread`] for an entry whose load has a stage on
    /// the main thread when no update has run on it and this call is not on it.
    pub fn retry<K>(&mut self, key: &K) -> Result<(), RetryError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let start = self.pool.retry(key)?;
        self.pool_start(start.into_iter().collect());
        Ok(())
    }

    /// The wait for every declared entry's outcome: it lands once no declared
    /// entry is pending, as a [`PoolReport`] of each key's outcome. Entries
    /// that failed are in it, as [`PoolOutcome::Failed`]. An offline session
    /// calls [`update`](Self::update) until [`PoolLoaded::has_landed`] before
    /// it renders. Dropping the [`PoolLoaded`] ends the wait.
    ///
    /// `K` is the type of the keys the pool was declared with; keys of another
    /// type are not in the report.
    pub fn pool_loaded<K>(&mut self) -> PoolLoaded<K>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let (loaded, landing) = PoolLoaded::new();
        self.pool.wait_loaded(landing);
        loaded
    }

    /// Where the entry under `key` stands, `None` when the pool has none: never
    /// declared, or released.
    #[must_use]
    pub fn pool_state<K>(&self, key: &K) -> Option<PoolState>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        self.pool.state(&PoolKey::new(key))
    }
}

#[cfg(feature = "sampler")]
impl<C: crate::Clock> Session<C> {
    /// The wave of the `audio_file` entry under `key`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed.
    pub fn audio<K>(&self, key: &K) -> Result<Arc<tutti_codec::Wave>, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, AudioFile>(key)
    }
}

#[cfg(feature = "soundfont")]
impl<C: crate::Clock> Session<C> {
    /// The SoundFont of the `soundfont` entry under `key`, shared by its `Arc`.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed.
    pub fn soundfont<K>(&self, key: &K) -> Result<Arc<tutti_sf2::SoundFont>, HandleError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        self.pool.handle::<K, SoundFontFile>(key)
    }
}
