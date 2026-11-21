//! A hosted plugin's presets as part of its [`PluginInfo`](crate::PluginInfo):
//! what the format does with them ([`PluginPresets::support`]) and the list
//! where there is one ([`ListedPreset`]).
//!
//! The list is read once, when the plugin loads, and held: an out-of-process
//! plugin's load ([`LoadOutOfProcess`](crate::LoadOutOfProcess)) reads
//! it in a round trip on the executor's thread, and an in-process plugin's
//! ([`LoadInProcess::run`](crate::LoadInProcess::run)) on the main thread
//! ([`PluginPresets::is_read`]). No host reports a change of a plugin's list, so
//! nothing refreshes it later. Loading is work the app runs
//! ([`Session::load_preset`](crate::Session::load_preset),
//! [`Session::load_preset_file`](crate::Session::load_preset_file)).

use std::sync::{Arc, OnceLock};

use tutti_plugin_types::{Preset, PresetError, PresetId, PresetSupport};

use tutti_plugin_host::InstanceId;

/// What a plugin does with presets and the list it has, in its
/// [`PluginInfo`](crate::PluginInfo).
///
/// Cloning shares the list: a clone sees the read land.
#[derive(Debug, Clone)]
pub struct PluginPresets {
    support: PresetSupport,
    plugin: InstanceId,
    list: Arc<OnceLock<Vec<Preset>>>,
}

impl PluginPresets {
    /// The presets of the plugin instance `plugin`, as `support` says. A
    /// plugin that lists none has its (empty) list read at once; any other has
    /// it set once by [`fill`](Self::fill).
    pub(crate) fn new(support: PresetSupport, plugin: InstanceId) -> Self {
        let this = Self {
            support,
            plugin,
            list: Arc::default(),
        };
        // A plugin that lists nothing has nothing to read.
        if !support.can_list() {
            this.fill(Ok(Vec::new()));
        }
        this
    }

    /// Sets the list from the plugin's answer, if it was not set. An answer
    /// that is an error (the plugin is gone, or has no route) sets nothing, so
    /// the list reads as not read rather than as empty.
    pub(crate) fn fill(&self, answer: Result<Vec<Preset>, PresetError>) {
        if let Ok(presets) = answer {
            let _ = self.list.set(presets);
        }
    }

    /// What the plugin's format does with presets: a list to load from
    /// ([`Full`](PresetSupport::Full), [`ListOnly`](PresetSupport::ListOnly)),
    /// a file to load ([`LoadByPath`](PresetSupport::LoadByPath), CLAP), or
    /// nothing.
    #[must_use]
    pub fn support(&self) -> PresetSupport {
        self.support
    }

    /// Whether the plugin's list has been read, which the load does. A read the
    /// plugin could not answer (it was gone) leaves this `false` for good, with
    /// [`iter`](Self::iter) empty although the plugin lists some.
    #[must_use]
    pub fn is_read(&self) -> bool {
        self.list.get().is_some()
    }

    /// How many presets the plugin listed (0 for a plugin that lists none, and
    /// before the read).
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.get().map_or(0, Vec::len)
    }

    /// Whether it listed none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The listed presets, in the plugin's own order.
    pub fn iter(&self) -> impl Iterator<Item = ListedPreset> + '_ {
        self.list
            .get()
            .into_iter()
            .flatten()
            .map(|preset| ListedPreset {
                plugin: self.plugin.clone(),
                preset: preset.clone(),
            })
    }

    /// The listed preset with `id`, if the plugin listed one.
    #[must_use]
    pub fn get(&self, id: &PresetId) -> Option<ListedPreset> {
        self.iter().find(|listed| listed.id() == id)
    }
}

/// One preset a plugin listed ([`PluginPresets::iter`]): an owned handle that
/// belongs to that plugin instance, loaded by
/// [`Session::load_preset`](crate::Session::load_preset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPreset {
    pub(crate) plugin: InstanceId,
    preset: Preset,
}

impl ListedPreset {
    /// The plugin's id for it: opaque, never a position in the list.
    #[must_use]
    pub fn id(&self) -> &PresetId {
        &self.preset.id
    }

    /// Its name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.preset.name
    }

    /// The bank it is in, for a format that groups.
    #[must_use]
    pub fn bank(&self) -> Option<&str> {
        self.preset.bank.as_deref()
    }
}
