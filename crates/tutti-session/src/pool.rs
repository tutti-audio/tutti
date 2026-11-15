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
//! Each kind of entry is a [`PoolEntry`]; [`audio_file`] and [`soundfont`]
//! are the data kinds. A load's outcome lands in [`Session::update`] as a
//! [`SessionEvent::Pool`](crate::SessionEvent::Pool), and
//! [`Session::pool_state`] reads where an entry stands.

use std::fmt;

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

impl Session {
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
    /// Returns at once. Each load's outcome lands as a
    /// [`SessionEvent::Pool`] in a later [`update`](Self::update), those of one
    /// update in the order the set names them.
    ///
    /// # Errors
    ///
    /// [`PoolError::DuplicateKey`] for a key the set names twice,
    /// [`PoolError::WrongKind`] for a key a node uses. The call refuses the set
    /// whole: nothing of it is applied.
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
