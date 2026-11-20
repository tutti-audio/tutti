//! A pooled plugin's state as jobs: [`Session::save_state`] and
//! [`Session::restore_state`], each answering a [`Job`] that lands in a
//! later [`update`](Session::update) or is waited for ([`Job::wait`]).
//!
//! A save is a round trip to the plugin, so it starts on a worker of the
//! session's executor, which asks the plugin's
//! [`AnyThread`](tutti_plugin_types::AnyThread) save. A served plugin answers
//! it there. An in-process plugin's save runs on the **main thread**, through
//! the session's main work: the worker waits for it, so the app must keep
//! calling [`update`](Session::update) on the main thread, and a save the
//! main thread does not run within the off-main deadline
//! (`tutti-plugin-host`'s `OFF_MAIN_DEADLINE`) ends
//! as [`StateJobFailed::Plugin`] with [`StateError::Unavailable`], as it does
//! when the session's main work was never run.
//!
//! A restore is the plugin's main thread's (every format's `load_state` takes
//! the main thread's proof): it is posted to the session's main work and runs
//! in the first `update` on the main thread, for a served plugin as for an
//! in-process one.
//!
//! A job lands in an `update` of its session: dropping the session ends it and
//! it never lands. Dropping the [`Job`] cancels it: a restore not yet run does
//! not run, and a save's bytes are discarded. A restore and a save of one plugin asked
//! before either landed are unordered; wait for one before asking the other.

use std::fmt;

use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::StateError;
use tutti_plugin_types::{PluginRefresh, StateUse};

use super::entry::{Hosted, PluginEntry};
use super::{PluginRef, PluginSignal, queue};
use crate::{HandleError, Job, PoolKey, RestoreState, SaveState, Session, StateJobFailed};
use crate::{OtherPeer, PeerState, SavedPeer};

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
    /// The state was saved from another peer: nothing is sent to the plugin.
    #[error(transparent)]
    OtherPeer(OtherPeer),
}

/// Why a key names no plugin node: shared by the plugin jobs that need one.
#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum NodeLookup<K: fmt::Debug> {
    /// The key has no loaded plugin entry.
    #[error(transparent)]
    Entry(#[from] HandleError<K>),
    /// The entry's plugin is in no node.
    #[error("the plugin of {0:?} has no node")]
    NoNode(K),
}

impl<K: fmt::Debug> From<NodeLookup<K>> for StateJobError<K> {
    fn from(lookup: NodeLookup<K>) -> Self {
        match lookup {
            NodeLookup::Entry(error) => Self::Entry(error),
            NodeLookup::NoNode(key) => Self::NoNode(key),
        }
    }
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Saves the state of the plugin in the node over the entry under `key`,
    /// for `state_use` ([`StateUse::Project`] is the state a project keeps).
    /// The round trip starts on the session's executor and the state lands in a
    /// later [`update`](Self::update), or [`Job::wait`] returns it. An in-process plugin saves on the main
    /// thread, through the session's main work: keep calling `update` there,
    /// or the save ends as [`StateJobFailed::Plugin`] with
    /// [`StateError::Unavailable`] after the off-main deadline (see the module
    /// docs).
    /// The wait never lands once the session is dropped.
    ///
    /// # Errors
    ///
    /// [`StateJobError::Entry`] when the key has no loaded plugin entry;
    /// [`StateJobError::NoNode`] when the plugin is in no node.
    pub fn save_state<P>(
        &mut self,
        key: &P,
        state_use: StateUse,
    ) -> Result<Job<SaveState>, StateJobError<P>>
    where
        P: crate::AppKey,
    {
        let (handle, plugin, _, _) = self.state_parts(key)?;
        // blocks: the state save is an IPC round trip, bounded by `state_wait`.
        // On a polled executor `spawn_dedicated` runs inside `update`'s poll, so this job stalls
        // `update` until the plugin lane makes it await its reply.
        Ok(self.jobs.submit_job_blocking(
            "tutti-state",
            move |cancel| {
                if cancel.is_cancelled() {
                    return Err(StateError::Unavailable);
                }
                handle.save_state(state_use)
            },
            move |_, done, ticket| {
                ticket.land(match done {
                    Ok(Ok(bytes)) => Ok(PeerState::of_plugin(plugin, bytes)),
                    Ok(Err(refused)) => Err(StateJobFailed::Plugin(refused)),
                    Err(failed) => Err(StateJobFailed::Worker(failed)),
                });
            },
        ))
    }

    /// Restores `state` into the plugin in the node over the entry under
    /// `key`, in the first [`update`](Self::update) on the main thread: the
    /// outcome lands there, and the parameter mirror is read again by the
    /// next plugin phase when it was applied. A restore and a save of the same
    /// plugin asked before either landed are unordered; wait for one before
    /// asking the other. The wait never lands once the session is dropped, or
    /// when no update runs on the main thread.
    ///
    /// **Dropping the [`Job`] cancels the restore** if the main thread has not
    /// started it, so a fire-and-forget call must keep the job until it
    /// lands. A drop after the main thread began the restore does not undo it:
    /// the restore applies whole and its result is discarded.
    ///
    /// # Errors
    ///
    /// [`StateJobError::Entry`] when the key has no loaded plugin entry;
    /// [`StateJobError::NoNode`] when the plugin is in no node;
    /// [`StateJobError::OtherPeer`] when `state` was saved from another
    /// peer than this plugin (nothing is sent).
    pub fn restore_state<P>(
        &mut self,
        key: &P,
        state: PeerState,
    ) -> Result<Job<RestoreState>, StateJobError<P>>
    where
        P: crate::AppKey,
    {
        let (handle, plugin, notices, _) = self.state_parts(key)?;
        state
            .check_for(&SavedPeer::Plugin(plugin))
            .map_err(StateJobError::OtherPeer)?;
        let (job, ticket) = self.jobs.new_job();
        ticket.waiting_for_main_thread();
        self.plugins.work.defer(move |main| {
            // A dropped job restores nothing.
            ticket.complete(|| {
                let applied = handle.load_state(main, state.bytes());
                if applied.is_ok() {
                    queue(&notices, PluginSignal::Refresh(PluginRefresh::ParamValues));
                }
                applied.map_err(StateJobFailed::Plugin)
            });
        });
        Ok(job)
    }

    /// The handle, the plugin and the notice queue of the node over `key`.
    pub(super) fn state_parts<P>(
        &self,
        key: &P,
    ) -> Result<(PluginHandle, PluginRef, super::Signals, Hosted), NodeLookup<P>>
    where
        P: crate::AppKey,
    {
        self.pool.entries.loaded::<P, PluginEntry>(key)?;
        let pool_key = PoolKey::new(key);
        // At most one node links a key: `add_pooled_plugin` refuses a key a
        // node already holds (`AddPluginError::Installed`) and a swap keeps
        // the node it replaces, so the first match is the only one.
        let (hosted, link) = self
            .graph
            .plugins
            .values()
            .find_map(|hosted| {
                let link = hosted.pooled.as_ref().filter(|link| link.key == pool_key)?;
                Some((hosted, link))
            })
            .ok_or_else(|| NodeLookup::NoNode(key.clone()))?;
        Ok((
            hosted.handle.clone(),
            hosted.plugin.clone(),
            hosted.notices.clone(),
            // The node's own instance: a reload that landed and is not yet
            // swapped in is not this one.
            link.hosted(),
        ))
    }
}
