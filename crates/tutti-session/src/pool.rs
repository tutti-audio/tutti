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
//! load included: it does not retry. When an identity changes, the old value
//! stays until the new load lands; if that load fails the old value is dropped,
//! its handles read closed, and the entry reads failed.
//!
//! Each kind of entry is a [`PoolEntry`]; `audio_file` (feature `sampler`) and
//! `soundfont` (feature `soundfont`) are the data kinds. A load's outcome lands
//! in [`Session::update`] as a [`SessionEvent::Pool`], and
//! [`Session::pool_state`] reads where an entry stands.

use std::fmt;
#[cfg(any(feature = "sampler", feature = "soundfont"))]
use std::sync::Arc;

use crate::{Session, SessionEvent};

#[cfg(any(feature = "sampler", feature = "soundfont"))]
mod data;
mod entry;
mod key;
mod table;

#[cfg(feature = "sampler")]
pub use data::{AudioFile, audio_file};
#[cfg(feature = "soundfont")]
pub use data::{SoundFontFile, soundfont};
pub use entry::{Declared, IntoPool, Life, Load, LoadError, PoolEntry, PoolSet};
pub use key::PoolKey;
pub use table::{HandleError, PoolError, PoolEvent, PoolState};
pub(crate) use table::{Pool, Start};

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
    /// load.
    ///
    /// Returns at once. Each load's outcome lands as a
    /// [`SessionEvent::Pool`] in a later [`update`](Self::update), those of one
    /// update in the order the set names them, then the releases, by key within a key type.
    /// An outcome that landed while its entry was held is reported when the
    /// pool declares the entry again.
    ///
    /// # Errors
    ///
    /// [`PoolError::DuplicateKey`] for a key the set names twice. The call
    /// refuses the set whole: nothing of it is applied.
    pub fn pool<K>(&mut self, parts: impl IntoPool<K>) -> Result<(), PoolError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + 'static,
    {
        let mut set = PoolSet::new();
        parts.into_pool(&mut set);
        for Start {
            key,
            generation,
            load,
        } in self.pool.declare(set)?
        {
            self.jobs.submit(
                move || load.run(),
                move |session, done, _| {
                    let done = done.unwrap_or_else(|failed| Err(LoadError::Job(failed)));
                    session.pool.land(&key, generation, done);
                },
            );
        }
        Ok(())
    }

    /// Where the entry under `key` stands, `None` when the pool has none: never
    /// declared, or released.
    #[must_use]
    pub fn pool_state<K>(&self, key: &K) -> Option<PoolState>
    where
        K: Ord + Clone + fmt::Debug + Send + 'static,
    {
        self.pool.state(&PoolKey::new(key))
    }

    /// The entries' reports since the last update, in declaration order.
    pub(crate) fn pool_events(&mut self, out: &mut Vec<SessionEvent>) {
        self.pool.sweep();
        self.pool.drain_events(out);
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
        K: Ord + Clone + fmt::Debug + Send + 'static,
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
        K: Ord + Clone + fmt::Debug + Send + 'static,
    {
        self.pool.handle::<K, SoundFontFile>(key)
    }
}
