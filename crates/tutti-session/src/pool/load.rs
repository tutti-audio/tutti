//! The loads of a pool's data entries as values the app runs: handed out by
//! [`Session::take_loads`](crate::Session::take_loads), run anywhere, and taken
//! back by [`Session::apply`](crate::Session::apply).

use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_platform::{Asset, Identity};

use crate::apply::{Cut, Sealed};
use crate::jobs::panic_message;
use crate::pool::entry::{DataWork, LoadError, Value};
use crate::pool::key::PoolKey;
use crate::pool::table::{Handout, Offered};
use crate::{AppKey, Clock, Session, SessionFloat, WorkerError};

/// One declared entry's load, handed out by
/// [`Session::take_loads`](crate::Session::take_loads).
///
/// A plugin entry is loaded as before until it has its own variant here.
#[derive(Debug)]
#[non_exhaustive]
#[must_use = "a load does nothing until it runs"]
pub enum LoadEntry<F: SessionFloat> {
    /// An entry that reads and decodes an asset (`audio_file`, `soundfont`, or a
    /// kind built with [`PoolLoad::on_app`](crate::PoolLoad::on_app)): its load
    /// holds nothing of the session.
    Data(LoadData<F>),
}

/// The load of a data entry, cut from the session by
/// [`Session::take_loads`](crate::Session::take_loads).
///
/// It holds the kind's work, the asset it reads and nothing of the session: run
/// it on any thread, then hand the result to
/// [`Session::apply`](crate::Session::apply). Two keys of a kind that shares
/// with one identity are one load, and its apply fills both.
///
/// Dropped without its output being applied (the value, or the
/// [`LoadedEntry`] it ran to), it makes its entry available to the next
/// `take_loads`.
#[must_use = "a load does nothing until it runs"]
pub struct LoadData<F: SessionFloat> {
    cut: Cut,
    flight: u64,
    key: PoolKey,
    asset: Option<Asset>,
    work: DataWork,
    handout: Handout,
    _float: PhantomData<fn() -> F>,
}

impl<F: SessionFloat> LoadData<F> {
    pub(crate) fn new(cut: Cut, offered: Offered) -> Self {
        let Offered {
            flight,
            key,
            asset,
            work,
            handout,
        } = offered;
        Self {
            cut,
            flight,
            key,
            asset,
            work,
            handout,
            _float: PhantomData,
        }
    }

    /// The entry's key: for a load two keys share, the first one declared.
    #[must_use]
    pub const fn key(&self) -> &PoolKey {
        &self.key
    }

    /// Runs the load: the asset's identity is taken first, so a change during
    /// the read shows at the next [`refresh_pool`](Session::refresh_pool), and
    /// then the kind's work reads it. The session is not touched.
    ///
    /// A load that fails, or panics, is a [`LoadedEntry`] that carries the
    /// [`LoadError`]: applying it makes the entry read failed.
    pub fn run(self) -> LoadedEntry<F> {
        let Self {
            cut,
            flight,
            asset,
            work,
            handout,
            ..
        } = self;
        let loaded_from = asset.and_then(|asset| asset.identity());
        let done = catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|payload| {
            Err(LoadError::Job(WorkerError::Panicked {
                message: panic_message(payload.as_ref()),
            }))
        });
        LoadedEntry {
            cut,
            flight,
            loaded_from,
            done,
            handout,
            _float: PhantomData,
        }
    }
}

impl<F: SessionFloat> fmt::Debug for LoadData<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadData")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// What [`LoadData::run`] made: the loaded value, or why the load failed.
///
/// [`Session::apply`](crate::Session::apply) installs it as the entry's load
/// landing: the value, its handles and the [`when_loaded`](Session::when_loaded)
/// runs of the entry, the old value dropped when the identity changed; or, for a
/// failed load, the entry reads failed and the old value is dropped, its handles
/// closed. It returns the entry's key; for a load two keys share, the first one
/// declared that still waits on it, and the apply fills both.
///
/// An entry released, declared again with another identity or loaded again
/// since the load was handed out makes the apply [`PoolApplyError::Stale`], and
/// nothing is installed.
#[must_use = "a load changes nothing until the session applies it"]
pub struct LoadedEntry<F: SessionFloat> {
    cut: Cut,
    flight: u64,
    loaded_from: Option<Identity>,
    done: Result<Value, LoadError>,
    handout: Handout,
    _float: PhantomData<fn() -> F>,
}

impl<F: SessionFloat> fmt::Debug for LoadedEntry<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedEntry")
            .field("failed", &self.done.is_err())
            .finish_non_exhaustive()
    }
}

/// Why [`Session::apply`](crate::Session::apply) did not apply a
/// [`LoadedEntry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PoolApplyError {
    /// The entry was released, declared again with another identity or loaded
    /// again since the load was handed out, or the load was made for another
    /// session. Nothing was installed: take the loads again.
    #[error("the entry changed since its load was handed out: take the loads again")]
    Stale,
}

impl<F: SessionFloat> Sealed<F> for LoadedEntry<F> {
    type Applied = PoolKey;
    type Error = PoolApplyError;

    fn cut(&self) -> &Cut {
        &self.cut
    }

    fn stale() -> PoolApplyError {
        PoolApplyError::Stale
    }

    fn apply_to<K: AppKey, C: Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<PoolKey, PoolApplyError> {
        let Self {
            flight,
            loaded_from,
            done,
            mut handout,
            ..
        } = self;
        let entries = &mut session.pool.entries;
        let key = entries.first_waiting(flight).ok_or(PoolApplyError::Stale)?;
        handout.settle();
        entries.stamped(flight, loaded_from);
        entries.land(flight, done);
        Ok(key)
    }
}
