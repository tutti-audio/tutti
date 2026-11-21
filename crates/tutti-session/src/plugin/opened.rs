//! A plugin opened and not yet in a graph: what a [`LoadedPlugin`](crate::LoadedPlugin)
//! shares, and the readings of it that outlive its node.
//!
//! [`Shared`] holds the plugin ([`Install`], taken once by the node that hosts
//! it), the parameter mirror read at the load, what the load says of the plugin
//! ([`PluginInfo`]), and its [`Standing`]: whether its live part is gone. The
//! parts that only the main thread may drop (an in-process plugin) go to the
//! main thread when they are dropped on another ([`Affine`]).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tutti_buffers::ChannelLayout;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::{Hosting, Loadable};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::{Features, IpcError, MainThreadMarker, MainWorkHandle, PluginFailure};
use tutti_plugin_types::{PluginDescriptor, PluginRef};
use tutti_rt::LockExt;
use tutti_units::SampleRate;

use super::{HostPlugin, Mirror, Open, Origin};
use crate::PluginPresets;
use crate::loaded::Stamp;
use crate::peer::ResourceIds;
use crate::{GiveUp, LoadError, ResourceId};

/// What a plugin's load needs of the session that built it.
#[derive(Clone, Debug)]
pub(super) struct Context {
    pub(super) rate: SampleRate,
    pub(super) work: MainWorkHandle,
    /// The session's executor, which a served plugin's exit watch runs on.
    pub(super) executor: tutti_platform::WeakExecutor,
    pub(super) ids: ResourceIds,
    /// Whether the session recovers a plugin that dies at its node.
    pub(super) recovery: bool,
}

/// A plugin opened and not yet in a graph: what the node takes.
pub(super) struct Install {
    pub(super) plugin: HostPlugin,
    pub(super) mirror: Option<Mirror>,
    pub(super) origin: Origin,
}

/// A value that drops on the main thread when it must.
///
/// Dropped on another thread while `main_only`, its value is handed to the
/// session's main-work collector to drop at the next tick on the main thread
/// ([`MainWorkHandle::defer_drop`]); otherwise it drops where it is. The last
/// clone of a [`LoadedPlugin`](crate::LoadedPlugin) drops its plugin through
/// one, on whatever thread drops the clone.
pub(super) struct Affine<T: Send + 'static> {
    value: Option<T>,
    work: MainWorkHandle,
    main_only: bool,
}

impl<T: Send + 'static> Affine<T> {
    pub(super) fn new(value: T, work: &MainWorkHandle, main_only: bool) -> Self {
        Self {
            value: Some(value),
            work: work.clone(),
            main_only,
        }
    }

    pub(super) fn get(&self) -> &T {
        self.value.as_ref().expect("held until dropped")
    }

    /// The value, which now drops where its new owner says.
    pub(super) fn into_inner(mut self) -> T {
        self.value.take().expect("held until taken")
    }
}

impl<T: Send + 'static> Drop for Affine<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        if self.main_only && MainThreadMarker::new().is_none() {
            self.work.defer_drop(move |_main| drop(value));
        } else {
            drop(value);
        }
    }
}

/// The reading of whether a loaded plugin's live part is gone, shared by the
/// opened plugin, the node's record and every clone of the handle.
///
/// It holds the plugin's own handle only while the plugin is in no node; the
/// node takes it out ([`release_handle`](Self::release_handle)), so a clone of
/// the handle never keeps a server alive past its node.
pub(super) struct Standing {
    pub(super) handle: Mutex<Option<Affine<PluginHandle>>>,
    /// The session recovers a plugin that dies at its node.
    pub(super) recovery: bool,
    /// A node hosts the plugin (and so recovery can act on it).
    pub(super) hosted: AtomicBool,
    /// The session saw the bridge dead (once a node hosts the plugin), and why.
    pub(super) dead: Mutex<Option<PluginFailure>>,
    /// Recovery gave up on it, and why.
    pub(super) gave_up: Mutex<Option<GiveUp>>,
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
                .is_some_and(|handle| handle.get().is_crashed())
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
                .and_then(|handle| handle.get().status().cause().cloned())
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
    plugin: PluginRef,
    descriptor: PluginDescriptor,
    hosting: Hosting,
    server_pid: Option<u32>,
    presets: PluginPresets,
}

impl PluginInfo {
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

    /// Where the plugin runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`].
    #[must_use]
    pub const fn hosting(&self) -> Hosting {
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

/// A plugin opened and not yet in a graph: what a [`LoadedPlugin`](crate::LoadedPlugin)
/// shares.
///
/// An opened plugin that no graph holds yet, its parameter mirror, and what
/// the handles read of it. The parts that only the main thread may drop
/// (an in-process plugin) go to the main thread when it is dropped on another.
pub struct Shared {
    pub(super) id: ResourceId,
    pub(super) stamp: Stamp,
    pub(super) info: Arc<PluginInfo>,
    pub(super) standing: Arc<Standing>,
    /// The plugin's ports as a node over it would have them.
    pub(super) ports: PluginPorts,
    /// What the node takes, once.
    install: Mutex<Option<Affine<Install>>>,
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
    /// `main` is the proof of the main thread the load runs on: the plugin's
    /// preset list is read with it.
    pub(super) fn new(
        cx: &Context,
        stamp: Stamp,
        open: Open,
        loadable: Loadable,
        config: AudioConfig,
        main: MainThreadMarker,
    ) -> Result<Self, LoadError> {
        let Open {
            plugin,
            restore: _,
            mirror,
        } = open;
        let plugin = plugin.map_err(plugin_failed)?;
        let hosting = match plugin.hosting() {
            ResolvedHosting::OutOfProcess => Hosting::OutOfProcess,
            ResolvedHosting::InProcess => Hosting::InProcess,
        };
        let served = hosting == Hosting::OutOfProcess;
        let handle = plugin.handle().clone();
        let ports = ports_of(&handle);
        let id = cx.ids.mint();
        let presets = PluginPresets::new(handle.preset_support(), handle.instance());
        if presets.support().can_list() {
            presets.fill(handle.presets(main));
        }
        let info = PluginInfo {
            plugin: plugin.plugin().clone(),
            descriptor: handle.descriptor().clone(),
            hosting,
            server_pid: handle.server_pid(),
            presets,
        };
        let main_only = !served;
        Ok(Self {
            id,
            stamp,
            info: Arc::new(info),
            standing: Arc::new(Standing {
                handle: Mutex::new(Some(Affine::new(handle, &cx.work, main_only))),
                recovery: cx.recovery,
                hosted: AtomicBool::new(false),
                dead: Mutex::new(None),
                gave_up: Mutex::new(None),
            }),
            ports,
            install: Mutex::new(Some(Affine::new(
                Install {
                    plugin,
                    mirror,
                    origin: Origin { config, loadable },
                },
                &cx.work,
                main_only,
            ))),
        })
    }

    /// The plugin and what opened it, once: `None` after a node took it.
    pub(super) fn take_install(&self) -> Option<Install> {
        self.install.lock_recover().take().map(Affine::into_inner)
    }

    /// Whether a node has not taken the plugin yet.
    pub(super) fn has_install(&self) -> bool {
        self.install.lock_recover().is_some()
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
