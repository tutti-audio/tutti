//! A pooled plugin's presets as jobs: [`Session::load_preset`] and
//! [`Session::load_preset_file`], each answering a [`Job`] that lands in a
//! later [`update`](Session::update) or is waited for ([`Job::wait`]).
//!
//! A load is the plugin's main thread's (every format's preset load takes the
//! main thread's proof): it is posted to the session's main work and runs in
//! the first `update` on the main thread, for a served plugin as for an
//! in-process one, as [`Session::restore_state`] does. A plugin that declines
//! lands [`PresetError::Refused`] and one that is gone [`PresetError::Lost`].

use std::fmt;
use std::path::PathBuf;

use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PluginRefresh, PresetError, PresetId, PresetSupport};

use super::state::NodeLookup;
use super::{PluginSignal, queue};
use crate::job::job_kind;
use crate::{HandleError, Job, ListedPreset, ResourceId, Session, WaitError, WorkerError};

/// Why [`Session::load_preset`] or [`Session::load_preset_file`] started no job.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PresetJobError<K: fmt::Debug> {
    /// The key has no loaded plugin entry.
    #[error(transparent)]
    Entry(#[from] HandleError<K>),
    /// The entry's plugin is in no node.
    #[error("the plugin of {0:?} has no node")]
    NoNode(K),
    /// The preset was listed by another plugin instance than the node's: nothing
    /// is sent.
    #[error("the preset was listed by another plugin")]
    OtherPlugin,
}

impl<K: fmt::Debug> From<NodeLookup<K>> for PresetJobError<K> {
    fn from(lookup: NodeLookup<K>) -> Self {
        match lookup {
            NodeLookup::Entry(error) => Self::Entry(error),
            NodeLookup::NoNode(key) => Self::NoNode(key),
        }
    }
}

job_kind! {
    /// A preset loaded into a plugin ([`Session::load_preset`],
    /// [`Session::load_preset_file`]); the job lands `()` when the plugin took it.
    pub LoadPreset => (), PresetJobFailed
}

/// How a preset job ended without loading.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PresetJobFailed {
    /// The plugin's own answer: it has no such operation
    /// ([`PresetError::Unsupported`], a file for a plugin that lists), it
    /// declined ([`PresetError::Refused`]) or it is gone ([`PresetError::Lost`]).
    #[error(transparent)]
    Preset(#[from] PresetError),
    /// The worker did not finish.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The wait could not run ([`Job::wait`]).
    #[error(transparent)]
    Wait(#[from] WaitError),
}

/// `f32` because a plugin format is `f32` at its FFI.
impl<K: crate::AppKey, C: crate::Clock<Float = f32>> Session<K, C> {
    /// Loads the listed `preset` into the plugin in the node over the entry
    /// under `key`, in the first [`update`](Self::update) on the main thread:
    /// the outcome lands there, and the parameter mirror is read again by the
    /// next plugin phase when the plugin took it. **Dropping the [`Job`]
    /// cancels the load** if the main thread has not started it. The preset
    /// must be one the node's own plugin listed: after a reload landed and
    /// before its swap, the new plugin's presets are refused.
    ///
    /// # Errors
    ///
    /// [`PresetJobError::Entry`] when the key has no loaded plugin entry;
    /// [`PresetJobError::NoNode`] when the plugin is in no node;
    /// [`PresetJobError::OtherPlugin`] when `preset` was listed by another
    /// plugin (nothing is sent).
    pub fn load_preset<P>(
        &mut self,
        key: &P,
        preset: &ListedPreset,
    ) -> Result<Job<LoadPreset>, PresetJobError<P>>
    where
        P: crate::AppKey,
    {
        self.start_load(key, preset.id().clone(), Some(preset.plugin), None)
    }

    /// Loads the preset file at `path` into the plugin in the node over the
    /// entry under `key` (a plugin whose
    /// [`support`](crate::PluginPresets::support) is
    /// [`LoadByPath`](PresetSupport::LoadByPath), CLAP), as
    /// [`load_preset`](Self::load_preset) does. A plugin that lists its
    /// presets takes no file: the job lands [`PresetError::Unsupported`].
    /// Dropping the [`Job`] cancels a load the main thread has not started.
    ///
    /// # Errors
    ///
    /// [`PresetJobError::Entry`] when the key has no loaded plugin entry;
    /// [`PresetJobError::NoNode`] when the plugin is in no node.
    pub fn load_preset_file<P>(
        &mut self,
        key: &P,
        path: impl Into<PathBuf>,
    ) -> Result<Job<LoadPreset>, PresetJobError<P>>
    where
        P: crate::AppKey,
    {
        let id = PresetId::Location(path.into());
        self.start_load(key, id, None, Some(PresetSupport::LoadByPath))
    }

    /// The preset the plugin says is loaded, when it will say: not every
    /// format has the query, and the plugin may decline. Never "the first
    /// one". A plugin in no node, or gone, answers `None`.
    pub fn current_preset<P>(&self, key: &P, main: MainThreadMarker) -> Option<PresetId>
    where
        P: crate::AppKey,
    {
        let (handle, ..) = self.state_parts(key).ok()?;
        handle.current_preset(main)
    }

    /// Posts the load of `id` to the main thread; with `needs`, a plugin whose
    /// support is another answers `Unsupported`.
    fn start_load<P>(
        &mut self,
        key: &P,
        id: PresetId,
        listed_by: Option<ResourceId>,
        needs: Option<PresetSupport>,
    ) -> Result<Job<LoadPreset>, PresetJobError<P>>
    where
        P: crate::AppKey,
    {
        let (handle, _, notices, hosted) = self.state_parts(key)?;
        // The plugin the load reaches is the node's, which a landed reload
        // replaces only at its swap: a preset of any other instance is refused.
        if listed_by.is_some_and(|plugin| plugin != hosted.instance()) {
            return Err(PresetJobError::OtherPlugin);
        }
        let (job, ticket) = self.jobs.new_job();
        ticket.waiting_for_main_thread();
        self.plugins.work.defer(move |main| {
            // A dropped job loads nothing.
            ticket.complete(|| {
                // A node removed, or a plugin swapped out, since the call: the
                // plugin it reached is gone.
                if !hosted.is_hosted() {
                    return Err(PresetJobFailed::Preset(PresetError::Lost));
                }
                let loaded = load(&handle, main, &id, needs);
                if loaded.is_ok() {
                    queue(&notices, PluginSignal::Refresh(PluginRefresh::ParamValues));
                }
                loaded.map_err(PresetJobFailed::Preset)
            });
        });
        Ok(job)
    }
}

/// Asks the plugin to load `id`, if its support is `needs` (any when `None`).
fn load(
    handle: &PluginHandle,
    main: MainThreadMarker,
    id: &PresetId,
    needs: Option<PresetSupport>,
) -> Result<(), PresetError> {
    if needs.is_some_and(|needs| needs != handle.preset_support()) {
        return Err(PresetError::Unsupported);
    }
    handle.load_preset(main, id)
}
