//! A plugin's presets as work the app runs: [`Session::load_preset`] and
//! [`Session::load_preset_file`] hand back a [`LoadPreset`], run on the main
//! thread with its proof (`run(main)`), as [`Session::restore_state`] does.
//!
//! The value is `!Send`, whether the plugin is out of process or in process. A plugin
//! that declines ends the run as [`PresetError::Refused`] and one that is gone
//! as [`PresetError::Lost`]. A preset the plugin took is a [`LoadedPreset`],
//! which [`Session::apply`] takes back.

use core::marker::PhantomData;
use std::fmt;
use std::path::PathBuf;

use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PresetError, PresetId, PresetSupport};

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
/// [`Session::load_preset_file`]).
///
/// `!Send`: the plugin loads it on the main thread.
#[must_use = "work does nothing until it runs"]
pub struct LoadPreset {
    landed: Landed,
    handle: PluginHandle,
    id: PresetId,
    needs: Option<PresetSupport>,
    main_thread: PhantomData<*const ()>,
}

impl fmt::Debug for LoadPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadPreset")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl LoadPreset {
    /// Asks the plugin to load the preset, on the main thread. The output goes
    /// to [`Session::apply`], which has the session read the plugin's
    /// parameters again.
    ///
    /// # Errors
    ///
    /// [`PresetError::Lost`] when the node was removed or the plugin replaced
    /// since the value was made, or the plugin is gone;
    /// [`PresetError::Unsupported`] when the plugin has no such operation (a
    /// file for a plugin that lists); [`PresetError::Refused`] when it
    /// declined.
    pub fn run(self, main: MainThreadMarker) -> Result<LoadedPreset, PresetError> {
        if self.landed.gate.is_ended() {
            return Err(PresetError::Lost);
        }
        if self
            .needs
            .is_some_and(|needs| needs != self.handle.preset_support())
        {
            return Err(PresetError::Unsupported);
        }
        self.handle.load_preset(main, &self.id)?;
        Ok(LoadedPreset(self.landed))
    }
}

/// A preset the plugin took ([`LoadPreset::run`]), which [`Session::apply`]
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
    /// on the main thread with [`LoadPreset::run`], then hand its output to
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
        LoadPreset {
            landed: Landed::of(self, *node, hosted.gate.clone()),
            handle: hosted.handle.clone(),
            id,
            needs,
            main_thread: PhantomData,
        }
    }
}
