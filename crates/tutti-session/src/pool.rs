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
//! Holds have a depth ([`PoolConfig::live_holds`]): the newest snapshot holds
//! keep their entries live, and an entry only older holds keep is **parked**
//! when its kind says so ([`PoolEntry::retain`]): its value is released and its
//! handles read closed, and declaring the key again loads it afresh. A node's
//! hold never parks.
//!
//! A key declared again with the same identity keeps what it has, a failed
//! load included: [`Session::retry`] loads it again, and also reopens a loaded
//! entry its kind reads as lost. When an identity changes, the old value
//! stays until the new load lands; if that load fails the old value is dropped,
//! its handles read closed, and the entry reads failed.
//!
//! Each kind of entry is a [`PoolEntry`]; `audio_file` (feature `sampler` and a `decode-*`) and
//! `soundfont` (feature `soundfont`) are the data kinds, and
//! `Session::plugin_entry` (feature `plugin`) declares a plugin, which a node
//! then takes (`Session::add_pooled_plugin`). A load's outcome is reported by
//! [`Session::update`] as a [`SessionEvent::Pool`](crate::SessionEvent::Pool), and
//! [`Session::pool_state`] reads where an entry stands. A handle read by key
//! (`Session::audio`, `Session::soundfont`) is a clone that owns nothing:
//! dropping it leaves its entry loaded. [`Session::when_loaded`] runs a
//! closure once an entry is loaded.
//!
//! An entry that reads an asset takes the asset's [`Identity`](tutti_platform::Identity)
//! on the worker that loads it (a file's is its path and stamp, bytes' is minted
//! once), and [`Session::refresh_pool`] takes it again, where the app runs it, and
//! [`Session::apply`] loads again the entries whose asset changed; the diff itself never reads metadata.
//!
//! # Stages
//!
//! # Loads
//!
//! A [`PoolLoad`] is one of a few values:
//!
//! - [`PoolLoad::on_app`], a data kind's (`audio_file`, `soundfont`): the work the
//!   app runs. [`Session::take_loads`] hands out each declared entry whose load
//!   has not been handed out, once, in declaration order, as a
//!   [`LoadEntry`]; [`LoadData::run`] reads the asset anywhere, without the
//!   session, to a [`LoadedEntry`]; and [`Session::apply`] installs it:
//!   handles, `when_loaded` runs, the old value dropped when the identity
//!   changed, and for a failed load the entry reads failed and its old value's
//!   handles read closed. A value dropped without its output being applied
//!   makes the entry available to the next `take_loads`; an output of an entry
//!   released, declared again with another identity or loaded again since is
//!   [`PoolApplyError::Stale`]. Until applied the entry reads
//!   [`PoolState::Loading`], and a bounce refuses it
//!   (`BounceError::PoolPending`, feature `export`).
//! - [`PoolLoad::on_session`], a MIDI port's or sink's: a stage on the session's
//!   own thread, which runs inside the call that declares the entry
//!   ([`Session::pool`], [`Session::retry`]) and never waits for an
//!   [`update`](Session::update). [`PoolLoad::ready`] is the entry given its
//!   value, which lands in the same call.
//! - [`PoolLoad::on_worker`] and [`PoolLoad::on_worker_then_main`], a plugin's:
//!   stages on a thread of the session's executor and then the main thread,
//!   at most [`PoolConfig::parallel_loads`] worker stages at once, which land in
//!   an [`update`](Session::update). They go when plugin entries are handed
//!   out as [`LoadEntry`] values too.
//!
//! Two keys of a kind that shares ([`Declared::shared`]) with the same
//! identity share one load and the one loaded value; any other kind is one
//! instance per key.
//!
//! [`Session::pool_loaded`] reports every declared entry's outcome once none
//! is pending: a data entry is pending until the app applies its load.

#[cfg(feature = "soundfont")]
use std::sync::Arc;

use crate::Session;

mod config;
#[cfg(any(all(feature = "sampler", codec), feature = "soundfont"))]
mod data;
mod entry;
mod job;
mod key;
mod load;
mod open;
mod refresh;
mod report;
mod run;
mod table;

pub use config::PoolConfig;
#[cfg(all(feature = "sampler", codec))]
pub use data::{AudioEntry, AudioFile, StreamInfo, Tier, audio_file};
#[cfg(feature = "soundfont")]
pub use data::{SoundFontFile, soundfont};
pub use entry::{
    Declared, IntoPool, Life, LoadError, Owned, PoolEntry, PoolLoad, PoolSet, Retention, Sharing,
    stranded_loads,
};
pub(crate) use entry::{Defer, Staged};
pub use job::{PoolJobError, PoolLoaded};
pub use key::PoolKey;
pub use load::{LoadData, LoadEntry, LoadedEntry, PoolApplyError};
pub use open::OpenCx;
pub use refresh::{RefreshError, RefreshPool, RefreshReport, RefreshedPool};
pub use report::{PoolOutcome, PoolReport};
pub(crate) use table::Pool;
pub use table::{HandleError, PoolError, PoolEvent, PoolState, RetryError, WhenLoadedError};
#[cfg(feature = "plugin")]
pub(crate) use table::{Hold, HoldKind};

#[cfg(test)]
mod tests;

impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// Declares what the session keeps loaded: the whole set, each time.
    ///
    /// The set is diffed against the one declared before, by key and by
    /// identity. A new key starts a load on the session's executor; a key whose
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
    /// Returns at once. A new key starts a load: a data entry's waits for
    /// [`take_loads`](Self::take_loads), a MIDI port's completes in this call, and
    /// a plugin's runs on a thread of the session's executor, at most
    /// [`PoolConfig::parallel_loads`] at once. Each load's outcome is reported
    /// as a [`SessionEvent::Pool`](crate::SessionEvent::Pool) in the next
    /// [`update`](Self::update), those of one update in the order the set names
    /// them, then the releases, by key within a key type. An outcome that
    /// landed while its entry was held is reported when the pool declares the
    /// entry again.
    ///
    /// # Errors
    ///
    /// [`PoolError::DuplicateKey`] for a key the set names twice;
    /// [`PoolError::NeedsMainThread`] for a key whose own load has a stage on
    /// the main thread ([`PoolLoad::on_worker_then_main`]) when no update has run
    /// on the main thread and this call is not on it. A key of the same kind
    /// whose load is [`PoolLoad::on_worker`] is not refused. The call refuses the
    /// set whole: nothing of it is applied.
    pub fn pool<P>(&mut self, parts: impl IntoPool<P, C::Float>) -> Result<(), PoolError<P>>
    where
        P: crate::AppKey,
    {
        let mut set = PoolSet::new();
        parts.into_pool(&mut set);
        let starts = self.pool.entries.declare(set)?;
        self.pool_start(starts);
        Ok(())
    }

    /// The loads of the declared entries that have not been handed out, once, in
    /// declaration order: each is a value the app runs, anywhere, and hands back
    /// to [`apply`](Self::apply).
    ///
    /// A data entry's load is handed out once; a second call finds it taken. One
    /// whose [`LoadData`] was dropped without its output being applied is handed
    /// out again, and [`retry`](Self::retry) arms a failed entry again. Two keys of
    /// a kind that shares with one identity are one load. An entry whose load
    /// completes inside the declare call (a MIDI port, a provided value) is
    /// never handed out, and a plugin entry is loaded by the session until it is.
    pub fn take_loads(&mut self) -> Vec<LoadEntry<C::Float>> {
        let cut = crate::apply::Cut::of(self);
        let (offered, restarts) = self.pool.entries.take_offers();
        // A dropped load whose kind now loads in another stage runs as a declare's.
        self.pool_start(restarts);
        offered
            .into_iter()
            .map(|offered| LoadEntry::Data(LoadData::new(cut, offered)))
            .collect()
    }

    /// Loads an entry again, with the identity it has: one that
    /// [failed](PoolState::Failed), or a loaded one its kind reads as lost.
    ///
    /// A failed entry loads as it did when declared (the plugin installed
    /// since, the file written): a data entry's load is handed out by the next
    /// [`take_loads`](Self::take_loads). A loaded entry whose handle reads lost
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
    pub fn retry<P>(&mut self, key: &P) -> Result<(), RetryError<P>>
    where
        P: crate::AppKey,
    {
        let start = self.pool.entries.retry(key)?;
        self.pool_start(start.into_iter().collect());
        Ok(())
    }

    /// The wait for every declared entry's outcome: it lands once no declared
    /// entry is pending, as a [`PoolReport`] of each key's outcome. Entries
    /// that failed are in it, as [`PoolOutcome::Failed`]. A data entry is
    /// pending until the app applies its load ([`take_loads`](Self::take_loads)),
    /// which nothing waits for here. Dropping the [`Job`](crate::Job) ends the
    /// wait.
    ///
    /// `P` is the type of the keys the pool was declared with; keys of another
    /// type are not in the report.
    pub fn pool_loaded<P>(&mut self) -> crate::Job<PoolLoaded<P>>
    where
        P: crate::AppKey,
    {
        let (job, ticket) = self.jobs.new_job();
        self.pool.entries.wait_loaded(ticket);
        job
    }

    /// Runs `then` in an [`update`](Self::update), once, when the declared
    /// entry under `key` is loaded: in the update where its load lands, or the
    /// next one when it already is. `then` runs after the update's other
    /// landings and is given the session, so it reads the entry's handle and
    /// declares what depends on it.
    ///
    /// A run waits through a failed load, so a [`retry`](Self::retry) that
    /// lands runs it, and through a reload (the entry reads
    /// [`PoolState::Reloading`] until it lands). It is dropped without
    /// running when its entry is released, and while the entry is only held it
    /// waits.
    ///
    /// The runs of one update go one at a time, each checked against the pool
    /// as the run before it left it: a run whose entry an earlier run released
    /// is dropped. A run asked from inside a run waits for the next update. A
    /// run that panics unwinds out of `update`, and the runs after it stay
    /// asked for the next one.
    ///
    /// # Errors
    ///
    /// [`WhenLoadedError::NotPooled`] when the key is not declared: never
    /// declared, or released.
    pub fn when_loaded<P>(
        &mut self,
        key: &P,
        then: impl FnOnce(&mut Self) + Send + 'static,
    ) -> Result<(), WhenLoadedError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.when_loaded(key, Box::new(then))
    }

    /// The check of the assets the pool's entries read, as a value the app
    /// runs.
    ///
    /// Each entry that reads an asset took the asset's
    /// [`Identity`](tutti_platform::Identity) when it loaded (a file's is its
    /// path with its length and modification time). [`RefreshPool::run`] takes
    /// each identity again, anywhere the app likes, and
    /// [`apply`](Self::apply) compares: an entry whose identity differs reloads
    /// as it does for a changed identity (the old value stays until the new one
    /// lands, as a [`PoolEvent::Reloaded`]), and two keys that share a load
    /// reload with one. An entry whose asset can no longer be opened keeps what
    /// it has, and is named as unreadable. An entry with no identity (its file
    /// could not be stamped when it loaded) loads again once its asset has one,
    /// and that load's worker stage takes it; a kind whose load has no worker
    /// stage takes none, so it loads again at every refresh. An asset of bytes
    /// never changes, so it never reloads, and an asset that never has an
    /// identity is not watched.
    ///
    /// The refresh is cut from the pool as it stands now: an entry released,
    /// declared again or loaded again before the apply makes it
    /// [`RefreshError::Stale`], and nothing is applied; run the refresh again.
    ///
    /// This is the one place the pool reads file metadata: [`pool`](Self::pool)
    /// never does, so a key declared after its file changed shares the loaded
    /// entry until the next refresh, and a plugin binary that changed on disk
    /// or a provided asset is never detected. Dropping the value reloads
    /// nothing.
    pub fn refresh_pool(&self) -> RefreshPool {
        RefreshPool::new(
            crate::apply::Cut::of(self),
            self.pool.entries.stamped_files(),
        )
    }

    /// Where the entry under `key` stands, `None` when the pool has none: never
    /// declared, or released.
    #[must_use]
    pub fn pool_state<P>(&self, key: &P) -> Option<PoolState>
    where
        P: crate::AppKey,
    {
        self.pool.entries.state(&PoolKey::new(key))
    }
}

#[cfg(all(feature = "sampler", codec))]
impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// What the `audio_file` entry under `key` read: its wave, or its stream's
    /// header ([`AudioEntry`]).
    ///
    /// The handle is a clone and owns nothing: dropping it leaves the entry
    /// loaded, and the entry's release leaves it reading what it had.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed.
    pub fn audio<P>(&self, key: &P) -> Result<AudioEntry<C::Float>, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, AudioFile>(key)
    }
}

#[cfg(feature = "soundfont")]
impl<K: crate::AppKey, C: crate::Clock> Session<K, C> {
    /// The SoundFont of the `soundfont` entry under `key`, shared by its `Arc`.
    /// The handle owns nothing: dropping it leaves the entry loaded.
    ///
    /// # Errors
    ///
    /// [`HandleError::NotPooled`] when the key is not declared (never, or
    /// released); [`HandleError::WrongKind`] when it is another kind of entry;
    /// [`HandleError::NotLoaded`] before the load lands, or after it failed.
    pub fn soundfont<P>(&self, key: &P) -> Result<Arc<tutti_sf2::SoundFont>, HandleError<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.handle::<P, SoundFontFile>(key)
    }
}
