//! A plugin's state as work the app runs: [`Session::save_state`] and
//! [`Session::restore_state`] hand back a [`SaveState`] and a [`RestoreState`],
//! each a value by where the plugin runs, as [`Session::load_plugin`] does.
//!
//! A plugin in this process has its state calls on the main thread: its value
//! (`SaveStateInProcess`, `RestoreStateInProcess`) is `!Send` and its `run` takes
//! the [`MainThreadMarker`]. A plugin in a `plugin-server` process answers over
//! IPC from any thread: its value (`SaveStateOutOfProcess`,
//! `RestoreStateOutOfProcess`) is `Send` and a [`Future`] through [`IntoFuture`],
//! filled by the bridge's IPC reader thread when the server's reply lands, with
//! the bridge's control lane thread firing the deadline. It has no blocking form.
//! Nothing is posted to the session and no `update` has to run for either.
//!
//! A save is pure data ([`PeerState`]). A restore ends as a [`RestoredState`],
//! which [`Session::apply`] takes back: it queues the read of the plugin's
//! parameters, and refuses by name when the node it was cut from is gone or
//! now hosts another plugin instance. For an out-of-process plugin it also has
//! the plugin's editor-only in-process instance (an Audio Unit's) load the state,
//! on the main thread at the session's next update.

use core::marker::PhantomData;
use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use tutti_graph::NodeKey;
use tutti_plugin::PluginAnswers;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PluginRef, StateError, StateUse};

use super::awaiting::Stage;
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
/// [`SaveStateOutOfProcess`] it awaits on any thread. This enum has no `run`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::SaveStateInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::SaveStateOutOfProcess>();
/// send::<tutti_session::SavingStateOutOfProcess>();
/// ```
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub enum SaveState {
    /// The plugin runs in this process.
    InProcess(SaveStateInProcess),
    /// The plugin runs in a `plugin-server` process.
    OutOfProcess(SaveStateOutOfProcess),
}

/// A state to save from a plugin that runs in this process.
///
/// `!Send`: the plugin's state is read on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct SaveStateInProcess {
    gate: Gate,
    handle: PluginHandle,
    plugin: PluginRef,
    state_use: StateUse,
    main_thread: PhantomData<*const ()>,
}

impl fmt::Debug for SaveStateInProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaveStateInProcess")
            .field("plugin", &self.plugin)
            .field("state_use", &self.state_use)
            .finish_non_exhaustive()
    }
}

impl SaveStateInProcess {
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

/// A state to save from a plugin that runs in a `plugin-server` process. It is
/// `Send`, and it is a [`Future`] through [`IntoFuture`]: awaiting it queues the
/// request with the plugin's bridge and yields the [`PeerState`]. Nothing of it
/// needs the main thread, so any thread awaits it. There is no blocking form.
///
/// Dropping it unawaited asks nothing.
#[must_use = "work does nothing until it is awaited"]
pub struct SaveStateOutOfProcess {
    gate: Gate,
    answers: PluginAnswers,
    plugin: PluginRef,
    state_use: StateUse,
}

impl fmt::Debug for SaveStateOutOfProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaveStateOutOfProcess")
            .field("plugin", &self.plugin)
            .field("state_use", &self.state_use)
            .finish_non_exhaustive()
    }
}

impl IntoFuture for SaveStateOutOfProcess {
    type Output = Result<PeerState, StateError>;
    type IntoFuture = SavingStateOutOfProcess;

    fn into_future(self) -> SavingStateOutOfProcess {
        let Self {
            gate,
            answers,
            plugin,
            state_use,
        } = self;
        SavingStateOutOfProcess {
            stage: Stage::Unstarted((gate, answers, state_use)),
            plugin,
        }
    }
}

/// An out-of-process save being awaited ([`SaveStateOutOfProcess`]): resolves
/// to the plugin's [`PeerState`].
///
/// The first poll queues the request with the plugin's bridge and awaits the
/// receiver of a oneshot. **The bridge's IPC reader thread fills the sender**
/// when the server's reply lands, which wakes the future: a poll never blocks,
/// and `Session::update` is not needed. **The bridge's control lane thread
/// fires the deadline**: a server that never answers is cut off there, at the
/// bridge's state timeout, and the lane sends [`StateError::Stalled`] into the
/// sender with no one polling.
///
/// Dropped before it resolves, the future drops the receiver and the state the
/// server sends is discarded: a save changes nothing in the plugin.
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
#[must_use = "a save does nothing unless it is awaited"]
pub struct SavingStateOutOfProcess {
    stage: Stage<(Gate, PluginAnswers, StateUse), Vec<u8>, StateError>,
    plugin: PluginRef,
}

impl fmt::Debug for SavingStateOutOfProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavingStateOutOfProcess")
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

impl Future for SavingStateOutOfProcess {
    type Output = Result<PeerState, StateError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.stage
            .poll(
                cx,
                |(gate, answers, state_use)| {
                    if gate.is_ended() {
                        return Err(StateError::Lost);
                    }
                    Ok(answers.save_state(state_use))
                },
                StateError::PluginCrashed,
            )
            .map_ok(|bytes| PeerState::of_plugin(this.plugin.clone(), bytes))
    }
}

/// A saved state to put back into a plugin ([`Session::restore_state`]), by
/// where the plugin runs: a [`RestoreStateInProcess`] the app runs on the main
/// thread, or a [`RestoreStateOutOfProcess`] it awaits on any thread. This enum
/// has no `run`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::RestoreStateInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::RestoreStateOutOfProcess>();
/// send::<tutti_session::RestoringStateOutOfProcess>();
/// send::<tutti_session::RestoredState>();
/// ```
#[must_use = "work does nothing until it runs"]
#[derive(Debug)]
pub enum RestoreState {
    /// The plugin runs in this process.
    InProcess(RestoreStateInProcess),
    /// The plugin runs in a `plugin-server` process.
    OutOfProcess(RestoreStateOutOfProcess),
}

/// A saved state to put into a plugin that runs in this process.
///
/// `!Send`: the plugin's state is loaded on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct RestoreStateInProcess {
    landed: Landed,
    handle: PluginHandle,
    state: PeerState,
    main_thread: PhantomData<*const ()>,
}

impl fmt::Debug for RestoreStateInProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoreStateInProcess")
            .field("node", &self.landed.node)
            .finish_non_exhaustive()
    }
}

impl RestoreStateInProcess {
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

/// A saved state to put into a plugin that runs in a `plugin-server` process. It
/// is `Send`, and it is a [`Future`] through [`IntoFuture`]: awaiting it queues
/// the state with the plugin's bridge and yields the [`RestoredState`], which
/// [`Session::apply`] takes back on the main thread. There is no blocking form.
///
/// Dropping it unawaited sends nothing.
#[must_use = "work does nothing until it is awaited"]
pub struct RestoreStateOutOfProcess {
    landed: Landed,
    answers: PluginAnswers,
    state: PeerState,
}

impl fmt::Debug for RestoreStateOutOfProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoreStateOutOfProcess")
            .field("node", &self.landed.node)
            .finish_non_exhaustive()
    }
}

impl IntoFuture for RestoreStateOutOfProcess {
    type Output = Result<RestoredState, StateError>;
    type IntoFuture = RestoringStateOutOfProcess;

    fn into_future(self) -> RestoringStateOutOfProcess {
        let Self {
            mut landed,
            answers,
            state,
        } = self;
        // What `Session::apply` has the editor-only instance load, once the
        // server took the state.
        let state = Arc::new(state);
        landed.mirror = Some(Mirror {
            answers: answers.clone(),
            state: Arc::clone(&state),
        });
        RestoringStateOutOfProcess {
            stage: Stage::Unstarted((answers, state)),
            landed: Some(landed),
        }
    }
}

/// An out-of-process restore being awaited ([`RestoreStateOutOfProcess`]):
/// resolves to the [`RestoredState`] once the server has taken the state.
///
/// Filled and bounded as [`SavingStateOutOfProcess`] is: **the bridge's IPC
/// reader thread fills the sender** with the server's reply, and **the bridge's
/// control lane thread fires the deadline**, sending [`StateError::Stalled`]
/// with no one polling.
///
/// Dropped after the first poll and before it resolves, the future drops the
/// receiver, but the request is already queued: **the server may still apply
/// the state**. No [`RestoredState`] exists for it, because one is made only by
/// this future resolving, so [`Session::apply`] is never run for that restore
/// and the session's view of the plugin's parameters is stale until they are
/// read again (a later [`apply`](Session::apply), or the plugin's own report).
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
#[must_use = "a restore does nothing unless it is awaited"]
pub struct RestoringStateOutOfProcess {
    stage: Stage<(PluginAnswers, Arc<PeerState>), (), StateError>,
    landed: Option<Landed>,
}

impl fmt::Debug for RestoringStateOutOfProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoringStateOutOfProcess")
            .finish_non_exhaustive()
    }
}

impl Future for RestoringStateOutOfProcess {
    type Output = Result<RestoredState, StateError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let landed = &this.landed;
        this.stage
            .poll(
                cx,
                |(answers, state)| {
                    // Checked when the request is made, so a plugin the node left
                    // is never asked.
                    if landed.as_ref().is_some_and(|l| l.gate.is_ended()) {
                        return Err(StateError::Lost);
                    }
                    Ok(answers.load_state(state.bytes()))
                },
                StateError::PluginCrashed,
            )
            .map_ok(|()| RestoredState(this.landed.take().expect("a restore resolves once")))
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
    state: Arc<PeerState>,
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

/// A state the plugin took ([`RestoreStateInProcess::run`] or
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
    /// thread with [`SaveStateInProcess::run`] or, for a plugin in a plugin
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
        let (gate, plugin) = (hosted.gate.clone(), hosted.plugin.clone());
        Ok(match &hosted.answers {
            None => SaveState::InProcess(SaveStateInProcess {
                gate,
                handle: hosted.handle.clone(),
                plugin,
                state_use,
                main_thread: PhantomData,
            }),
            Some(answers) => SaveState::OutOfProcess(SaveStateOutOfProcess {
                gate,
                answers: answers.clone(),
                plugin,
                state_use,
            }),
        })
    }

    /// Puts `state` back into the plugin hosted at `node`: run it on the main
    /// thread with [`RestoreStateInProcess::run`] or await it
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
        Ok(match &hosted.answers {
            None => RestoreState::InProcess(RestoreStateInProcess {
                landed,
                handle: hosted.handle.clone(),
                state,
                main_thread: PhantomData,
            }),
            Some(answers) => RestoreState::OutOfProcess(RestoreStateOutOfProcess {
                landed,
                answers: answers.clone(),
                state,
            }),
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
