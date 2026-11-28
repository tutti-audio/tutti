//! A plugin's presets as work the app runs: [`Session::load_preset`] and
//! [`Session::load_preset_file`] hand back a [`LoadPreset`], a value by where
//! the plugin runs as [`Session::restore_state`]'s is: `!Send` and run on the
//! main thread with its proof (`run(main)`) for a plugin in this process, `Send`
//! and awaited on any thread for a plugin in a `plugin-server` process (an
//! [`Op`] over a [`PluginOp`]).
//!
//! A plugin that has no preset with the id ends the work as
//! [`PresetError::Unlisted`], one that declines a preset it has as
//! [`PresetError::Refused`], and one that is gone as [`PresetError::Lost`]. A
//! preset the plugin took is a [`LoadedPreset`], which [`Session::apply`] takes
//! back.

use std::path::PathBuf;
use std::task::Poll;
use std::time::Duration;

use tutti_plugin::{Answer, PluginAnswers};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PresetError, PresetId, PresetSupport};

use super::op::{InProcessOp, Op, OutOfProcessOp, PluginOp, Sealed as OpSealed};
use super::state::{Landed, PluginApplyError};
use super::{AnyPlugin, HostedPlugin};
use crate::SessionFloat;
use crate::apply::{Cut, Sealed};
use crate::{AppKey, Clock, Key, ListedPreset, NoSuchNode, Session};

/// Why [`Session::load_preset`] made no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PresetStartError {
    /// No plugin is hosted at the node.
    #[error(transparent)]
    NoNode(#[from] NoSuchNode),
    /// The preset was listed by another plugin instance than the node's: nothing
    /// is sent.
    #[error("the preset was listed by another plugin")]
    OtherPlugin,
}

/// A preset to load into a plugin ([`Session::load_preset`],
/// [`Session::load_preset_file`]), by where the plugin runs: a
/// [`LoadPresetInProcess`] the app runs on the main thread, or a
/// [`LoadPresetOutOfProcess`] it awaits on any thread. This enum has no `poll`.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadPresetInProcess>();
/// ```
///
/// ```
/// fn send<T: Send>() {}
/// send::<tutti_session::LoadPresetOutOfProcess>();
/// send::<tutti_session::AwaitingOp<tutti_session::LoadPresetOp>>();
/// send::<tutti_session::LoadedPreset>();
/// ```
pub type LoadPreset = Op<LoadPresetOp>;

/// The value [`Session::load_preset`] and [`Session::load_preset_file`] return for
/// a plugin in this process: `!Send`, run on the main thread. Its [`poll`](InProcessOp::poll) asks the plugin to load the preset
/// and yields the [`LoadedPreset`], which [`Session::apply`] takes back and which
/// has the session read the plugin's parameters again. It ends in
/// [`PresetError::Lost`] when the node was removed or the plugin replaced since
/// the value was made, or the plugin is gone; [`PresetError::Unsupported`] when
/// the plugin has no such operation (a file for a plugin that lists);
/// [`PresetError::Unlisted`] when no listed preset has the id;
/// [`PresetError::Refused`] when it declined.
pub type LoadPresetInProcess = InProcessOp<LoadPresetOp>;

/// The value [`Session::load_preset`] and [`Session::load_preset_file`] return for
/// a plugin in a `plugin-server` process: `Send`, and a `Future` through
/// `IntoFuture` that yields the [`LoadedPreset`], which [`Session::apply`] takes
/// back on the main thread. Dropping it unawaited sends nothing.
///
/// Dropped after the first poll and before it resolves, the future drops the
/// receiver, but the request is already queued: **the server may still load the
/// preset**. No [`LoadedPreset`] exists for it, so [`Session::apply`] is never
/// run for that load and the session's view of the plugin's parameters is stale
/// until they are read again.
///
/// # Errors
///
/// The output is [`PresetError::Lost`] when the node was removed or the plugin
/// replaced since the value was made, or its bridge ended or its server died
/// before it answered; [`PresetError::Unsupported`] when the plugin has no such
/// operation; [`PresetError::Unlisted`] when no listed preset has the id;
/// [`PresetError::Refused`] when it declined; [`PresetError::Busy`] when the
/// bridge's queue was full (nothing was sent); [`PresetError::Stalled`] when the
/// server did not answer in time (the preset may have loaded);
/// [`PresetError::Server`] when the plugin server failed the request itself;
/// [`PresetError::UnexpectedReply`] when it answered with another kind of reply.
///
/// # Panics
///
/// When polled after it resolved.
pub type LoadPresetOutOfProcess = OutOfProcessOp<LoadPresetOp>;

/// The load of a preset into a plugin ([`Session::load_preset`],
/// [`Session::load_preset_file`]); with `needs`, a plugin whose preset support
/// is another answers `Unsupported`.
#[derive(Debug)]
pub struct LoadPresetOp {
    landed: Landed,
    id: PresetId,
    needs: Option<PresetSupport>,
}

impl OpSealed for LoadPresetOp {}

impl PluginOp for LoadPresetOp {
    type Reply = ();
    type Output = LoadedPreset;
    type Error = PresetError;

    fn ended(&self) -> bool {
        self.landed.gate.is_ended()
    }

    fn lost() -> PresetError {
        PresetError::Lost
    }

    fn crashed() -> PresetError {
        PresetError::Lost
    }

    fn admit(&self, support: PresetSupport) -> Result<(), PresetError> {
        match self.needs {
            Some(needs) if needs != support => Err(PresetError::Unsupported),
            _ => Ok(()),
        }
    }

    fn stalled(after: Duration) -> PresetError {
        PresetError::Stalled { after }
    }

    fn run(&self, handle: &PluginHandle, main: MainThreadMarker) -> Poll<Result<(), PresetError>> {
        handle.load_preset(main, &self.id)
    }

    fn send(&self, answers: &PluginAnswers) -> Answer<Result<(), PresetError>> {
        answers.load_preset(&self.id)
    }

    fn finish(self, (): (), _answers: Option<&PluginAnswers>) -> LoadedPreset {
        LoadedPreset(self.landed)
    }
}

/// A preset the plugin took ([`LoadPresetInProcess::poll`] or [`LoadPresetOutOfProcess`]), which [`Session::apply`]
/// takes back.
///
/// It names the node and the plugin instance it was cut from.
#[derive(Debug)]
#[must_use = "a loaded preset does nothing until it is applied"]
pub struct LoadedPreset(Landed);

impl<F: SessionFloat> Sealed<F> for LoadedPreset {
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
    /// The listed `preset`, to load into the plugin hosted at `node`: run it
    /// on the main thread with [`LoadPresetInProcess::poll`] or await it
    /// ([`LoadPresetOutOfProcess`]), then hand its output to
    /// [`apply`](Self::apply). The preset must be one the node's own plugin
    /// listed: the check compares the plugin instance that listed it with the
    /// node's. A crash recovery, a reload and a replace each load the plugin
    /// again, which is another instance, so a preset listed before one is
    /// refused after it as [`PresetStartError::OtherPlugin`]: list again from
    /// the node's plugin.
    ///
    /// # Errors
    ///
    /// [`PresetStartError::NoNode`] when no plugin is hosted at `node`;
    /// [`PresetStartError::OtherPlugin`] when `preset` was listed by another
    /// plugin (nothing is sent).
    pub fn load_preset(
        &self,
        node: &Key<AnyPlugin>,
        preset: &ListedPreset,
    ) -> Result<LoadPreset, PresetStartError> {
        let hosted = self.hosted_at(node)?;
        if preset.plugin != hosted.handle.instance() {
            return Err(PresetStartError::OtherPlugin);
        }
        Ok(self.preset_load(node, hosted, preset.id().clone(), None))
    }

    /// The preset file at `path`, to load into the plugin hosted at `node` (a
    /// plugin whose [`support`](crate::PluginPresets::support) is
    /// [`LoadByPath`](PresetSupport::LoadByPath), CLAP), as
    /// [`load_preset`](Self::load_preset) does. A plugin that lists its
    /// presets takes no file: the run ends [`PresetError::Unsupported`].
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when no plugin is hosted at `node`.
    pub fn load_preset_file(
        &self,
        node: &Key<AnyPlugin>,
        path: impl Into<PathBuf>,
    ) -> Result<LoadPreset, NoSuchNode> {
        let hosted = self.hosted_at(node)?;
        Ok(self.preset_load(
            node,
            hosted,
            PresetId::Location(path.into()),
            Some(PresetSupport::LoadByPath),
        ))
    }

    /// The preset the plugin hosted at `node` says is loaded, when it will say:
    /// not every format has the query, and the plugin may decline. Never "the
    /// first one". A node with no plugin, or a plugin that is gone, answers
    /// `None`.
    pub fn current_preset(
        &self,
        node: &Key<AnyPlugin>,
        main: MainThreadMarker,
    ) -> Option<PresetId> {
        self.hosted_at(node).ok()?.handle.current_preset(main)
    }

    /// The load of `id` into the plugin `hosted` at `node`; with `needs`, a
    /// plugin whose support is another answers `Unsupported`.
    fn preset_load(
        &self,
        node: &Key<AnyPlugin>,
        hosted: &HostedPlugin,
        id: PresetId,
        needs: Option<PresetSupport>,
    ) -> LoadPreset {
        let landed = Landed::of(self, *node, hosted.gate.clone());
        Op::at(hosted, LoadPresetOp { landed, id, needs })
    }
}
