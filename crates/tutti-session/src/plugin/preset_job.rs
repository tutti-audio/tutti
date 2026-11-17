//! A pooled plugin's presets as jobs: [`Session::load_preset`] and
//! [`Session::load_preset_file`], each answering a [`Job`] that lands in a
//! later [`update`](Session::update) or is waited for ([`Job::wait`]).
//!
//! A load is the plugin's main thread's (every format's preset load takes the
//! main thread's proof): it is posted to the session's main work and runs in
//! the first `update` on the main thread, for a served plugin as for an
//! in-process one, as [`Session::restore_state`] does. A plugin that declines
//! lands [`PresetError::Refused`] and one that is gone [`PresetError::Lost`]. A
//! job lands in an `update` of its session, and dropping the [`Job`] cancels a
//! load not yet run.

use std::fmt;
use std::path::PathBuf;

use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::{MainThreadMarker, PluginRefresh, PresetError, PresetId, PresetSupport};

use super::entry::PluginEntry;
use super::state_job::NodeLookup;
use super::{Notice, queue};
use crate::job::job_kind;
use crate::{HandleError, Job, ListedPreset, Resource, Session, WaitError, WorkerError};

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
    /// The preset was listed by another plugin: nothing is sent.
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
    /// [`Session::load_preset_file`]).
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

impl<C: crate::Clock> Session<C> {
    /// Loads the listed `preset` into the plugin in the node over the entry
    /// under `key`, in the first [`update`](Self::update) on the main thread:
    /// the outcome lands there, and the parameter mirror is read again by the
    /// next plugin phase when the plugin took it. **Dropping the [`Job`]
    /// cancels the load** if the main thread has not started it.
    ///
    /// # Errors
    ///
    /// [`PresetJobError::Entry`] when the key has no loaded plugin entry;
    /// [`PresetJobError::NoNode`] when the plugin is in no node;
    /// [`PresetJobError::OtherPlugin`] when `preset` was listed by another
    /// plugin (nothing is sent).
    pub fn load_preset<K>(
        &mut self,
        key: &K,
        preset: &ListedPreset,
    ) -> Result<Job<LoadPreset>, PresetJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        if self.pool.handle::<K, PluginEntry>(key)?.id() != preset.plugin {
            return Err(PresetJobError::OtherPlugin);
        }
        self.start_load(key, preset.id().clone(), None)
    }

    /// Loads the preset file at `path` into the plugin in the node over the
    /// entry under `key` (a plugin whose
    /// [`support`](crate::PluginPresets::support) is
    /// [`LoadByPath`](PresetSupport::LoadByPath), CLAP), as
    /// [`load_preset`](Self::load_preset) does. A plugin that lists its
    /// presets takes no file: the job lands [`PresetError::Unsupported`].
    ///
    /// # Errors
    ///
    /// [`PresetJobError::Entry`] when the key has no loaded plugin entry;
    /// [`PresetJobError::NoNode`] when the plugin is in no node.
    pub fn load_preset_file<K>(
        &mut self,
        key: &K,
        path: impl Into<PathBuf>,
    ) -> Result<Job<LoadPreset>, PresetJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let id = PresetId::Location(path.into());
        self.start_load(key, id, Some(PresetSupport::LoadByPath))
    }

    /// The preset the plugin says is loaded, when it will say: not every
    /// format has the query, and the plugin may decline. Never "the first
    /// one". A plugin in no node, or gone, answers `None`.
    pub fn current_preset<K>(&self, key: &K, main: MainThreadMarker) -> Option<PresetId>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let (handle, _, _) = self.state_parts(key).ok()?;
        handle.current_preset(main)
    }

    /// Posts the load of `id` to the main thread; with `needs`, a plugin whose
    /// support is another answers `Unsupported`.
    fn start_load<K>(
        &mut self,
        key: &K,
        id: PresetId,
        needs: Option<PresetSupport>,
    ) -> Result<Job<LoadPreset>, PresetJobError<K>>
    where
        K: Ord + Clone + fmt::Debug + Send + Sync + 'static,
    {
        let (handle, _, notices) = self.state_parts(key)?;
        let (job, ticket) = self.jobs.new_job();
        ticket.waiting_for_main_thread();
        self.work.defer(move |main| {
            // A dropped job loads nothing.
            ticket.complete(|| {
                let loaded = load(&handle, main, &id, needs);
                if loaded.is_ok() {
                    queue(&notices, Notice::Refresh(PluginRefresh::ParamValues));
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
