//! A plugin's state as work the app runs: [`Session::save_state`] and
//! [`Session::restore_state`] hand back a [`SaveState`] and a [`RestoreState`],
//! each a value by where the plugin runs, as [`Session::load_plugin`] does.
//!
//! A plugin in this process has its state calls on the main thread: its value
//! (`SaveStateInProcess`, `RestoreStateInProcess`) is `!Send` and its `poll` takes
//! the [`MainThreadMarker`]. A plugin in a `plugin-server` process answers over
//! IPC from any thread: its value (`SaveStateOutOfProcess`,
//! `RestoreStateOutOfProcess`) is `Send` and a [`Future`] through [`IntoFuture`],
//! filled by the bridge's IPC reader thread when the server's reply lands, with
//! the bridge's control lane thread firing the deadline. It has no blocking form.
//! Nothing is posted to the session and no `update` has to run for either. Both are an
//! [`Op`] over a [`PluginOp`].
//!
//! A save is pure data ([`PeerState`]). A restore ends as a [`RestoredState`],
//! which [`Session::apply`] takes back: it queues the read of the plugin's
//! parameters, and refuses by name when the node it was cut from is gone or
//! now hosts another plugin instance. For an out-of-process plugin it also has
//! the plugin's editor-only in-process instance (an Audio Unit's) load the state,
//! on the main thread at the session's next update.

use std::task::Poll;
use std::time::Duration;

use tutti_graph::NodeKey;
use tutti_plugin::{Answer, PluginAnswers};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PluginRef, StateError, StateUse};
use tutti_rt::Generation;

use super::op::{InProcessOp, Op, OutOfProcessOp, PluginOp, Sealed as OpSealed};
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

/// A plugin's state to save ([`Session::save_state`]), by where the plugin
/// runs: a [`SaveStateInProcess`] the app runs on the main thread, or a
/// [`SaveStateOutOfProcess`] it awaits on any thread. This enum has no `poll`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::SaveStateInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::SaveStateOutOfProcess>();
/// send::<tutti_session::AwaitingOp<tutti_session::SaveStateOp>>();
/// ```
pub type SaveState = Op<SaveStateOp>;

/// The value [`Session::save_state`] returns for a plugin in this process:
/// `!Send`, run on the
/// main thread. Its [`poll`](InProcessOp::poll) asks the plugin for its state and
/// yields the [`PeerState`]; it ends in [`StateError::Lost`] when the node was
/// removed or the plugin replaced since the value was made (nothing is asked),
/// otherwise the plugin's own error: it is gone, busy, refused, or has no state
/// route.
pub type SaveStateInProcess = InProcessOp<SaveStateOp>;

/// The value [`Session::save_state`] returns for a plugin in a `plugin-server`
/// process: `Send`, and a `Future` through `IntoFuture` that yields the
/// [`PeerState`]. Dropping it unawaited asks nothing; dropped after its first poll
/// the future drops the reply, and a save changes nothing in the plugin.
///
/// The first poll queues the request with the plugin's bridge: its IPC reader
/// thread fills the reply, and its control lane thread fires the deadline, so a
/// poll never blocks and `Session::update` is not needed.
///
/// # Errors
///
/// The output is [`StateError::Lost`] when the node was removed or the plugin
/// replaced since the value was made (nothing is asked);
/// [`StateError::PluginCrashed`] when the plugin's bridge ended or its server
/// died before it answered; [`StateError::Stalled`] when the server did not
/// answer in time; otherwise the plugin's own: busy, refused, or too large.
///
/// # Panics
///
/// When polled after it resolved.
pub type SaveStateOutOfProcess = OutOfProcessOp<SaveStateOp>;

/// The save of a plugin's state ([`Session::save_state`]).
#[derive(Debug)]
pub struct SaveStateOp {
    gate: Gate,
    plugin: PluginRef,
    state_use: StateUse,
}

impl OpSealed for SaveStateOp {}

impl PluginOp for SaveStateOp {
    type Reply = Vec<u8>;
    type Output = PeerState;
    type Error = StateError;

    fn ended(&self) -> bool {
        self.gate.is_ended()
    }

    fn lost() -> StateError {
        StateError::Lost
    }

    fn crashed() -> StateError {
        StateError::PluginCrashed
    }

    fn stalled(after: Duration) -> StateError {
        StateError::Stalled { after }
    }

    fn run(
        &self,
        handle: &PluginHandle,
        main: MainThreadMarker,
        mark: Generation,
    ) -> Poll<Result<Vec<u8>, StateError>> {
        handle.save_state_on(main, self.state_use, mark)
    }

    fn send(&self, answers: &PluginAnswers) -> Answer<Result<Vec<u8>, StateError>> {
        answers.save_state(self.state_use)
    }

    fn finish(self, bytes: Vec<u8>, _answers: Option<&PluginAnswers>) -> PeerState {
        PeerState::of_plugin(self.plugin, bytes)
    }
}

/// A saved state to put back into a plugin ([`Session::restore_state`]), by
/// where the plugin runs: a [`RestoreStateInProcess`] the app runs on the main
/// thread, or a [`RestoreStateOutOfProcess`] it awaits on any thread. This enum
/// has no `poll`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::RestoreStateInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::RestoreStateOutOfProcess>();
/// send::<tutti_session::AwaitingOp<tutti_session::RestoreStateOp>>();
/// send::<tutti_session::RestoredState>();
/// ```
pub type RestoreState = Op<RestoreStateOp>;

/// The value [`Session::restore_state`] returns for a plugin in this process:
/// `!Send`, run on the main thread. Its [`poll`](InProcessOp::poll) loads the state and yields the
/// [`RestoredState`], which [`Session::apply`] takes back and which has the
/// session read the plugin's parameters again. It ends in [`StateError::Lost`]
/// when the node was removed or the plugin replaced since the value was made
/// (the plugin is not asked, so no instance the node left receives the state),
/// otherwise the plugin's own error: it is gone, busy, refused the state
/// ([`StateError::Rejected`]) or has no state route.
pub type RestoreStateInProcess = InProcessOp<RestoreStateOp>;

/// The value [`Session::restore_state`] returns for a plugin in a
/// `plugin-server` process: `Send`, and a `Future` through `IntoFuture` that
/// yields the [`RestoredState`], which [`Session::apply`] takes back on the main
/// thread. Dropping it unawaited sends nothing.
///
/// Dropped after the first poll and before it resolves, the future drops the
/// receiver, but the request is already queued: **the server may still apply
/// the state**. No [`RestoredState`] exists for it, so [`Session::apply`] is
/// never run for that restore and the session's view of the plugin's parameters
/// is stale until they are read again.
///
/// # Errors
///
/// The output is [`StateError::Lost`] when the node was removed or the plugin
/// replaced since the value was made (the plugin is not asked);
/// [`StateError::PluginCrashed`] when the plugin's bridge ended or its server
/// died before it answered; [`StateError::Stalled`] when the server did not
/// answer in time; otherwise the plugin's own: busy, refused
/// ([`StateError::Rejected`]) or too large.
///
/// # Panics
///
/// When polled after it resolved.
pub type RestoreStateOutOfProcess = OutOfProcessOp<RestoreStateOp>;

/// The restore of a saved state into a plugin ([`Session::restore_state`]).
#[derive(Debug)]
pub struct RestoreStateOp {
    landed: Landed,
    state: PeerState,
}

impl OpSealed for RestoreStateOp {}

impl PluginOp for RestoreStateOp {
    type Reply = ();
    type Output = RestoredState;
    type Error = StateError;

    fn ended(&self) -> bool {
        self.landed.gate.is_ended()
    }

    fn lost() -> StateError {
        StateError::Lost
    }

    fn crashed() -> StateError {
        StateError::PluginCrashed
    }

    fn stalled(after: Duration) -> StateError {
        StateError::Stalled { after }
    }

    fn run(
        &self,
        handle: &PluginHandle,
        main: MainThreadMarker,
        _mark: Generation,
    ) -> Poll<Result<(), StateError>> {
        Poll::Ready(handle.load_state(main, self.state.bytes()))
    }

    fn send(&self, answers: &PluginAnswers) -> Answer<Result<(), StateError>> {
        answers.load_state(self.state.bytes())
    }

    fn finish(self, (): (), answers: Option<&PluginAnswers>) -> RestoredState {
        let Self { mut landed, state } = self;
        // What `Session::apply` has the editor-only instance load, once the
        // server took the state.
        landed.mirror = answers.map(|answers| Mirror {
            answers: answers.clone(),
            state,
        });
        RestoredState(landed)
    }
}

/// Where a plugin took a state or a preset: the session, the node and the
/// plugin instance the work was cut from. What [`Session::apply`] checks.
#[derive(Debug)]
pub(super) struct Landed {
    pub(super) cut: Cut,
    node: Key<AnyPlugin>,
    pub(super) gate: Gate,
    /// The state an awaited restore sent, for the plugin's in-process editor
    /// instance ([`Mirror`]).
    mirror: Option<Mirror>,
}

/// A restored state the plugin's editor-only in-process instance (an Audio
/// Unit's) has yet to load: it shows what the server took.
#[derive(Debug)]
struct Mirror {
    answers: PluginAnswers,
    state: PeerState,
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
            mirror: None,
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
        // Loaded on the main thread, where the instance is called from: the
        // session's own collector runs it at its next update.
        if let Some(Mirror { answers, state }) = self.mirror {
            session.plugins.work.defer(move |main| {
                answers.mirror_state(main, state.bytes());
            });
        }
        Ok(())
    }
}

/// A state the plugin took ([`RestoreStateInProcess::poll`] or
/// [`RestoreStateOutOfProcess`]), which [`Session::apply`]
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
    /// thread with [`SaveStateInProcess::poll`] or, for a plugin in a plugin
    /// server, await it ([`SaveStateOutOfProcess`]).
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
        Ok(Op::at(
            hosted,
            SaveStateOp {
                gate: hosted.gate.clone(),
                plugin: hosted.plugin.clone(),
                state_use,
            },
        ))
    }

    /// Puts `state` back into the plugin hosted at `node`: run it on the main
    /// thread with [`RestoreStateInProcess::poll`] or await it
    /// ([`RestoreStateOutOfProcess`]), then hand its output to
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
        let landed = Landed::of(self, *node, hosted.gate.clone());
        Ok(Op::at(hosted, RestoreStateOp { landed, state }))
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
