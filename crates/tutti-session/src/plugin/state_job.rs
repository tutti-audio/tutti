//! A pooled plugin's state as jobs: [`Session::save_state`] and
//! [`Session::restore_state`], each answering a [`PoolWait`] that lands in a
//! later [`update`](Session::update).
//!
//! A save is a round trip to the plugin, so it starts on a worker of the
//! session's spawner, which asks the plugin's
//! [`AnyThread`](tutti_plugin_types::AnyThread) save. A served plugin answers
//! it there. An in-process plugin's save runs on the **main thread**, through
//! the session's main work: the worker waits for it, so the app must keep
//! calling [`update`](Session::update) on the main thread, and a save the
//! main thread does not run within the off-main deadline
//! (`Timeouts::off_main`) ends
//! as [`StateJobFailed::State`] with [`StateError::Unavailable`], as it does
//! when the session's main work was never run.
//!
//! A restore is the plugin's main thread's (every format's `load_state` takes
//! the main thread's proof): it is posted to the session's main work and runs
//! in the first `update` on the main thread, for a served plugin as for an
//! in-process one.
//!
//! A wait lands in an `update` of its session: dropping the session ends the
//! job and its wait never lands. A restore and a save of one plugin asked
//! before either landed are unordered; wait for one before asking the other.

use std::fmt;

use tutti_plugin::error::StateError;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::PluginRefresh;

use super::entry::PluginEntry;
use super::{Notice, PluginRef, PluginState, queue};
use crate::{HandleError, JobFailed, PoolKey, PoolWait, Session};

/// Why [`Session::save_state`] or [`Session::restore_state`] started no job.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateJobError<K: fmt::Debug> {
    /// The key has no loaded plugin entry.
    #[error(transparent)]
    Entry(#[from] HandleError<K>),
    /// The entry's plugin is in no node: its state is the node's to save and
    /// restore ([`Session::add_pooled_plugin`]).
    #[error("the plugin of {0:?} has no node")]
    NoNode(K),
    /// The state was saved from another plugin: nothing is sent to the plugin.
    #[error(transparent)]
    OtherPlugin(StateError),
}

/// How a state job ended without its result.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateJobFailed {
    /// The plugin's own refusal: it is gone, busy, refused, has no state route,
    /// or an in-process save the main thread did not run in time
    /// ([`StateError::Unavailable`]).
    #[error(transparent)]
    State(#[from] StateError),
    /// The worker did not finish.
    #[error(transparent)]
    Job(#[from] JobFailed),
}

impl<C: crate::Clock> Session<C> {
    /// Saves the state of the plugin in the node over the entry under `key`.
    /// The round trip starts on the session's spawner and the state lands in a
    /// later [`update`](Self::update). An in-process plugin saves on the main
    /// thread, through the session's main work: keep calling `update` there,
    /// or the save ends as [`StateJobFailed::State`] with
    /// [`StateError::Unavailable`] after the off-main deadline (see the module
    /// docs).
    /// The wait never lands once the session is dropped.
    ///
    /// # Errors
    ///
    /// [`StateJobError::Entry`] when the key has no loaded plugin entry;
    /// [`StateJobError::NoNode`] when the plugin is in no node.
    pub fn save_state<K>(
        &mut self,
        key: &K,
    ) -> Result<PoolWait<Result<PluginState, StateJobFailed>>, StateJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let (handle, plugin, _) = self.state_parts(key)?;
        let (wait, landing) = PoolWait::new();
        self.jobs.submit(
            move || handle.any_thread().save_state(),
            move |_, done, _| {
                landing.land(match done {
                    Ok(Ok(bytes)) => Ok(PluginState::new(plugin, bytes)),
                    Ok(Err(refused)) => Err(StateJobFailed::State(refused)),
                    Err(failed) => Err(StateJobFailed::Job(failed)),
                });
            },
        );
        Ok(wait)
    }

    /// Restores `state` into the plugin in the node over the entry under
    /// `key`, in the first [`update`](Self::update) on the main thread: the
    /// outcome lands there, and the parameter mirror is read again by the
    /// next plugin phase when it was applied. A restore and a save of the same
    /// plugin asked before either landed are unordered; wait for one before
    /// asking the other. The wait never lands once the session is dropped, or
    /// when no update runs on the main thread.
    ///
    /// # Errors
    ///
    /// [`StateJobError::Entry`] when the key has no loaded plugin entry;
    /// [`StateJobError::NoNode`] when the plugin is in no node;
    /// [`StateJobError::OtherPlugin`] when `state` was saved from another
    /// plugin (nothing is sent).
    pub fn restore_state<K>(
        &mut self,
        key: &K,
        state: PluginState,
    ) -> Result<PoolWait<Result<(), StateJobFailed>>, StateJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let (handle, plugin, notices) = self.state_parts(key)?;
        state
            .check_for(&plugin)
            .map_err(StateJobError::OtherPlugin)?;
        let (wait, landing) = PoolWait::new();
        self.work.defer(move |main| {
            let applied = handle.load_state(main, state.bytes());
            if applied.is_ok() {
                queue(&notices, Notice::Refresh(PluginRefresh::ParamValues));
            }
            landing.land(applied.map_err(StateJobFailed::State));
        });
        Ok(wait)
    }

    /// The handle, the plugin and the notice queue of the node over `key`.
    fn state_parts<K>(
        &self,
        key: &K,
    ) -> Result<(PluginHandle, PluginRef, super::Notices), StateJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        self.pool.loaded::<K, PluginEntry>(key)?;
        let pool_key = PoolKey::new(key);
        // At most one node links a key: `add_pooled_plugin` refuses a key a
        // node already holds (`AddPluginError::Installed`) and a swap keeps
        // the node it replaces, so the first match is the only one.
        let hosted = self
            .graph
            .plugins
            .values()
            .find(|hosted| {
                hosted
                    .pooled
                    .as_ref()
                    .is_some_and(|link| link.key == pool_key)
            })
            .ok_or_else(|| StateJobError::NoNode(key.clone()))?;
        Ok((
            hosted.handle.clone(),
            hosted.plugin.clone(),
            hosted.notices.clone(),
        ))
    }
}
