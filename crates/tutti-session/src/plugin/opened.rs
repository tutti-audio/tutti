//! What a [`PluginInstance`](crate::PluginInstance) observes of a plugin, and the
//! plugin itself.
//!
//! [`Shared`] is the observer's half: what the load says of the plugin
//! ([`PluginInfo`]), its ports, and its [`Standing`]: whether its live part is
//! gone, or it has no plugin behind it. [`Install`] is the plugin and its
//! parameter mirror, owned by one [`LoadedPlugin`](crate::LoadedPlugin). The parts
//! that only the main thread may drop (an in-process plugin) go to the main
//! thread when they are dropped on another: the format's own handle defers its
//! close to the session's main-work collector.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_buffers::ChannelLayout;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::Loadable;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::{Features, IpcError, MainWorkHandle, PluginFailure, Preset, PresetError};
use tutti_plugin_types::{PluginDescriptor, PluginId, PluginRef};
use tutti_rt::LockExt;
use tutti_units::SampleRate;

use super::{HostPlugin, Mirror, Open, Origin};
use crate::PluginPresets;
use crate::apply::Cut;
use crate::loaded::Stamp;
use crate::peer::ResourceIds;
use crate::{GiveUp, LoadError, PeerResourceId};

/// What a plugin's load needs of the session that built it.
#[derive(Clone, Debug)]
pub(super) struct Context {
    pub(super) rate: SampleRate,
    pub(super) work: MainWorkHandle,
    /// The session's executor, which an out-of-process plugin's exit watch runs on.
    pub(super) executor: tutti_platform::WeakExecutor,
    pub(super) ids: ResourceIds,
    /// The session that built the load: the one the plugin may be added to.
    pub(super) cut: Cut,
    /// Whether the session recovers a plugin that dies at its node.
    pub(super) recovery: bool,
}

/// A plugin opened and not yet in a graph: what the node takes.
pub(super) struct Install {
    pub(super) plugin: HostPlugin,
    pub(super) mirror: Option<Mirror>,
    pub(super) origin: Origin,
}

/// The reading of whether a loaded plugin's live part is gone, shared by the
/// opened plugin, the node's record and every clone of the handle.
///
/// It holds the plugin's own handle only while the plugin is in no node; the
/// node takes it out ([`release_handle`](Self::release_handle)), so a clone of
/// the handle never keeps a server alive past its node.
pub(super) struct Standing {
    pub(super) handle: Mutex<Option<PluginHandle>>,
    /// The session recovers a plugin that dies at its node.
    pub(super) recovery: bool,
    /// A node hosts the plugin (and so recovery can act on it).
    pub(super) hosted: AtomicBool,
    /// The session saw the bridge dead (once a node hosts the plugin), and why.
    pub(super) dead: Mutex<Option<PluginFailure>>,
    /// Recovery gave up on it, and why.
    pub(super) gave_up: Mutex<Option<GiveUp>>,
    /// No plugin is behind the handle any more: its node was dropped before it
    /// was added, its node's record retired, or a swap replaced it.
    pub(super) retired: AtomicBool,
}

impl Standing {
    /// The bridge is dead: seen by the session at the node, or by the handle
    /// while the plugin is still in no node.
    pub(super) fn crashed(&self) -> bool {
        self.dead.lock_recover().is_some()
            || self
                .handle
                .lock_recover()
                .as_ref()
                .is_some_and(|handle| handle.is_crashed())
    }

    /// The bridge's lifecycle: recovery gave up, or the server is dead and
    /// nothing is bringing it back (recovery is off, or no node hosts the
    /// plugin for recovery to act on). A plugin that recovery reloads in place
    /// reads live throughout.
    pub(super) fn lost(&self) -> bool {
        self.gave_up.lock_recover().is_some()
            || (self.crashed() && !(self.recovery && self.hosted.load(Ordering::Acquire)))
    }

    /// Why the plugin is not live: that recovery gave up, else the failure of
    /// the bridge the session or the plugin's own handle saw.
    pub(super) fn cause(&self) -> PluginCause {
        if let Some(why) = self.gave_up.lock_recover().clone() {
            return PluginCause::GaveUp(why);
        }
        let failure = self.dead.lock_recover().clone().or_else(|| {
            self.handle
                .lock_recover()
                .as_ref()
                .and_then(|handle| handle.status().cause().cloned())
        });
        PluginCause::Died(failure.unwrap_or(PluginFailure::Unreported))
    }

    /// The node has the plugin: its death is seen there from now on.
    pub(super) fn release_handle(&self) {
        let released = self.handle.lock_recover().take();
        drop(released);
    }
}

/// What a plugin says of itself: one snapshot, taken at the load.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    id: PluginId,
    plugin: PluginRef,
    descriptor: PluginDescriptor,
    hosting: ResolvedHosting,
    server_pid: Option<u32>,
    presets: PluginPresets,
}

impl PluginInfo {
    /// The catalog id the load resolved: what a reload of this plugin passes to
    /// [`Session::load_plugin_as`](crate::Session::load_plugin_as).
    #[must_use]
    pub const fn id(&self) -> &PluginId {
        &self.id
    }

    /// The plugin the load opened: its file, its format and its id there.
    #[must_use]
    pub const fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's descriptor: name, vendor, version.
    #[must_use]
    pub const fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// The `plugin-server` process that hosts the plugin, `None` for an
    /// in-process plugin.
    #[must_use]
    pub const fn server_pid(&self) -> Option<u32> {
        self.server_pid
    }

    /// Where the plugin runs, as the load placed it.
    #[must_use]
    pub const fn hosting(&self) -> ResolvedHosting {
        self.hosting
    }

    /// What the plugin does with presets, and its list. The list belongs to this
    /// plugin instance: read once for it ([`PluginPresets::is_read`]), never per
    /// call, and a reload makes a new instance with a new info.
    #[must_use]
    pub const fn presets(&self) -> &PluginPresets {
        &self.presets
    }
}

/// Why a loaded plugin is not live.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PluginCause {
    /// The plugin's server died and nothing brought it back: recovery is off,
    /// or no node hosts the plugin. The failure is the bridge's latched cause.
    Died(PluginFailure),
    /// Recovery spent its budget and gave up.
    GaveUp(GiveUp),
}

/// What a [`PluginInstance`](crate::PluginInstance) shares: the readings of one
/// opened plugin.
pub struct Shared {
    pub(super) id: PeerResourceId,
    pub(super) stamp: Stamp,
    pub(super) info: Arc<PluginInfo>,
    pub(super) standing: Arc<Standing>,
    /// The plugin's ports as a node over it would have them.
    pub(super) ports: PluginPorts,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("id", &self.id)
            .field("info", &self.info)
            .field("lost", &self.standing.lost())
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// `read` reads the plugin's preset list, as the load's hosting allows.
    pub(super) fn new(
        cx: &Context,
        stamp: Stamp,
        plugin_id: PluginId,
        open: Open,
        loadable: Loadable,
        config: AudioConfig,
        read: impl FnOnce(&HostPlugin) -> Result<Vec<Preset>, PresetError>,
    ) -> Result<(Self, Install), LoadError> {
        let Open {
            plugin,
            restore: _,
            mirror,
        } = open;
        let plugin = plugin.map_err(plugin_failed)?;
        let handle = plugin.handle().clone();
        let ports = ports_of(&handle);
        let id = cx.ids.mint();
        let presets = PluginPresets::new(handle.preset_support(), handle.instance());
        if presets.support().can_list() {
            presets.fill(read(&plugin));
        }
        let info = PluginInfo {
            id: plugin_id,
            plugin: plugin.plugin().clone(),
            descriptor: handle.descriptor().clone(),
            hosting: plugin.hosting(),
            server_pid: handle.server_pid(),
            presets,
        };
        let shared = Self {
            id,
            stamp,
            info: Arc::new(info),
            standing: Arc::new(Standing {
                handle: Mutex::new(Some(handle)),
                recovery: cx.recovery,
                hosted: AtomicBool::new(false),
                dead: Mutex::new(None),
                gave_up: Mutex::new(None),
                retired: AtomicBool::new(false),
            }),
            ports,
        };
        let install = Install {
            plugin,
            mirror,
            origin: Origin { config, loadable },
        };
        Ok((shared, install))
    }
}

impl Drop for Shared {
    /// The value is going: its handles keep no server alive past it.
    fn drop(&mut self) {
        self.standing.release_handle();
    }
}

/// The buses a loaded plugin brings to a node: what a swap compares, so a
/// plugin whose ports differ from its node's is not swapped in.
///
/// The width of each input bus and each output bus, in bus order, and whether
/// the plugin has a MIDI output. Equal totals with another split of buses
/// (a main bus of 4 against a main of 2 and a sidechain of 2) differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginPorts {
    /// The width of each input bus.
    pub inputs: Vec<ChannelLayout>,
    /// The width of each output bus.
    pub outputs: Vec<ChannelLayout>,
    /// Whether the plugin has a MIDI output.
    pub midi_out: bool,
}

impl fmt::Display for PluginPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "inputs {:?}, outputs {:?}{}",
            self.inputs,
            self.outputs,
            if self.midi_out { ", MIDI out" } else { "" }
        )
    }
}

/// The ports a node over `handle`'s plugin has: the one reading a swap
/// compares, for the node's plugin and the incoming one alike.
pub(super) fn ports_of(handle: &PluginHandle) -> PluginPorts {
    let loaded = handle.loaded();
    PluginPorts {
        inputs: loaded.input_widths(),
        outputs: loaded.output_widths(),
        midi_out: loaded.features.contains(Features::MIDI_OUT),
    }
}

pub(super) fn plugin_failed(error: IpcError) -> LoadError {
    LoadError::Plugin(Arc::new(error))
}
