//! A plugin's state as work the app runs: [`Session::save_state`] and
//! [`Session::restore_state`] hand back a [`SaveState`] and a [`RestoreState`],
//! each run on the main thread with its proof (`run(main)`).
//!
//! Every format's state calls are the plugin's main thread's, so the value is
//! `!Send` and its `run` takes the [`MainThreadMarker`], whether the plugin is
//! out-of-process (its round trip runs on that thread) or in process (a direct call).
//! Nothing is posted to the session and no `update` has to run for it.
//!
//! A save is pure data ([`PeerState`]). A restore ends as a [`RestoredState`],
//! which [`Session::apply`] takes back: it queues the read of the plugin's
//! parameters, and refuses by name when the node it was cut from is gone or
//! now hosts another plugin instance.

use core::marker::PhantomData;
use std::fmt;

use tutti_graph::NodeKey;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PluginRef, StateError, StateUse};

use super::{AnyPlugin, Gate};
use crate::SessionFloat;
use crate::apply::{Cut, Sealed};
use crate::{AppKey, Clock, Key, NoSuchNode, OtherPeer, PeerState, SavedPeer, Session};

/// Why [`Session::restore_state`] made no value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StateStartError {
    /// No plugin is hosted at the node.
    #[error(transparent)]
    NoNode(#[from] NoSuchNode),
    /// The state was saved from another peer: nothing is sent to the plugin.
    #[error(transparent)]
    OtherPeer(OtherPeer),
}

/// Why [`Session::apply`] refused the output of a restore or a preset load:
/// nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PluginApplyError {
    /// The output was cut from another session, or from a plugin instance the
    /// node no longer hosts (a reload or a swap replaced it): run the work
    /// again.
    #[error("the output was cut from another session or another plugin instance: run it again")]
    Stale,
    /// The node the output was cut from is gone.
    #[error("the node the output was cut from is gone")]
    Gone,
}

/// A plugin's state to save ([`Session::save_state`]).
///
/// `!Send`: the plugin's state is read on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct SaveState {
    gate: Gate,
    handle: PluginHandle,
    plugin: PluginRef,
    state_use: StateUse,
    main_thread: PhantomData<*const ()>,
}

impl fmt::Debug for SaveState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaveState")
            .field("plugin", &self.plugin)
            .field("state_use", &self.state_use)
            .finish_non_exhaustive()
    }
}

impl SaveState {
    /// Asks the plugin for its state, on the main thread. The session is not
    /// touched, and no `update` has to run.
    ///
    /// # Errors
    ///
    /// [`StateError::Lost`] when the node was removed or the plugin replaced
    /// since the value was made (nothing is asked); otherwise the plugin's
    /// own: it is gone, busy, refused, or has no state route.
    pub fn run(self, main: MainThreadMarker) -> Result<PeerState, StateError> {
        if self.gate.is_ended() {
            return Err(StateError::Lost);
        }
        let bytes = self.handle.save_state_on(main, self.state_use)?;
        Ok(PeerState::of_plugin(self.plugin, bytes))
    }
}

/// A saved state to put back into a plugin ([`Session::restore_state`]).
///
/// `!Send`: the plugin's state is loaded on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct RestoreState {
    landed: Landed,
    handle: PluginHandle,
    state: PeerState,
    main_thread: PhantomData<*const ()>,
}

impl fmt::Debug for RestoreState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoreState")
            .field("node", &self.landed.node)
            .finish_non_exhaustive()
    }
}

impl RestoreState {
    /// Loads the state into the plugin, on the main thread. The output goes to
    /// [`Session::apply`], which has the session read the plugin's parameters
    /// again.
    ///
    /// # Errors
    ///
    /// [`StateError::Lost`] when the node was removed or the plugin replaced
    /// since the value was made (the plugin is not asked, so no instance the
    /// node left receives the state); otherwise the plugin's own: it is gone,
    /// busy, refused the state ([`StateError::Rejected`]) or has no state
    /// route.
    pub fn run(self, main: MainThreadMarker) -> Result<RestoredState, StateError> {
        if self.landed.gate.is_ended() {
            return Err(StateError::Lost);
        }
        self.handle.load_state(main, self.state.bytes())?;
        Ok(RestoredState(self.landed))
    }
}

/// Where a plugin took a state or a preset: the session, the node and the
/// plugin instance the work was cut from. What [`Session::apply`] checks.
#[derive(Debug)]
pub(super) struct Landed {
    pub(super) cut: Cut,
    node: Key<AnyPlugin>,
    pub(super) gate: Gate,
}

impl Landed {
    pub(super) fn of<K: AppKey, C: Clock>(
        session: &Session<K, C>,
        node: Key<AnyPlugin>,
        gate: Gate,
    ) -> Self {
        Self {
            cut: Cut::of(session),
            node,
            gate,
        }
    }

    /// Has the session read the parameters of the plugin at the node again,
    /// when the node still hosts the instance: all or nothing.
    pub(super) fn apply<K: AppKey, C: Clock>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<(), PluginApplyError> {
        let hosted = session
            .graph
            .plugins
            .get(&self.node.erase())
            .ok_or(PluginApplyError::Gone)?;
        if !hosted.gate.is(&self.gate) {
            return Err(PluginApplyError::Stale);
        }
        hosted.refresh_values();
        Ok(())
    }
}

/// A state the plugin took ([`RestoreState::run`]), which [`Session::apply`]
/// takes back.
///
/// It names the node and the plugin instance it was cut from.
#[derive(Debug)]
#[must_use = "a restored state does nothing until it is applied"]
pub struct RestoredState(Landed);

impl<F: SessionFloat> Sealed<F> for RestoredState {
    type Applied = ();
    type Error = PluginApplyError;

    fn cut(&self) -> &Cut {
        &self.0.cut
    }

    fn stale() -> PluginApplyError {
        PluginApplyError::Stale
    }

    fn apply_to<K: AppKey, C: Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<(), PluginApplyError> {
        self.0.apply(session)
    }
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: AppKey, C: Clock<Float = f32>> Session<K, C> {
    /// The state of the plugin hosted at `node`, to save for `state_use`
    /// ([`StateUse::Project`] is the state a project keeps): run it on the main
    /// thread with [`SaveState::run`].
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when no plugin is hosted at `node`.
    pub fn save_state(
        &self,
        node: &Key<AnyPlugin>,
        state_use: StateUse,
    ) -> Result<SaveState, NoSuchNode> {
        let hosted = self.hosted_at(node)?;
        Ok(SaveState {
            gate: hosted.gate.clone(),
            handle: hosted.handle.clone(),
            plugin: hosted.plugin.clone(),
            state_use,
            main_thread: PhantomData,
        })
    }

    /// Puts `state` back into the plugin hosted at `node`: run it on the main
    /// thread with [`RestoreState::run`], then hand its output to
    /// [`apply`](Self::apply).
    ///
    /// # Errors
    ///
    /// [`StateStartError::NoNode`] when no plugin is hosted at `node`;
    /// [`StateStartError::OtherPeer`] when `state` was saved from another peer
    /// than this plugin (nothing is sent).
    pub fn restore_state(
        &self,
        node: &Key<AnyPlugin>,
        state: PeerState,
    ) -> Result<RestoreState, StateStartError> {
        let hosted = self.hosted_at(node)?;
        state
            .check_for(&SavedPeer::Plugin(hosted.plugin.clone()))
            .map_err(StateStartError::OtherPeer)?;
        Ok(RestoreState {
            landed: Landed::of(self, *node, hosted.gate.clone()),
            handle: hosted.handle.clone(),
            state,
            main_thread: PhantomData,
        })
    }

    /// The record of the plugin hosted at `node`.
    pub(super) fn hosted_at(
        &self,
        node: &Key<AnyPlugin>,
    ) -> Result<&super::HostedPlugin, NoSuchNode> {
        let node: NodeKey = node.erase();
        self.graph
            .plugins
            .get(&node)
            .ok_or_else(|| NoSuchNode::new(node))
    }
}
