#![doc = include_str!("../README.md")]
//! What `plugin-probe` does, as a library so a test can call it in-process: read
//! the descriptor of every plugin the file at a path holds, by the path's extension.
//!
//! The binary (`src/main.rs`) is the command line around [`describe`].

#![forbid(unsafe_code)]

use std::path::Path;
use tutti_plugin_types::{
    Deadline, MainThreadMarker, MeasureFailure, PluginDescriptor, PluginFormat, ProbeRefusal,
    Unmeasured,
};

mod measure;

pub use measure::{
    BUDGET_MARGIN, CONFIGS, IMPULSE, NOTE, NOTE_LENGTH, RENDER_DEADLINE, SETTLE, SILENCE, Subject,
    measure, measure_at, measure_within,
};

/// The format a file is read as: `format` when the caller names one, and the
/// one its extension names otherwise.
fn routing(path: &Path, format: Option<PluginFormat>) -> Option<PluginFormat> {
    format.or_else(|| PluginFormat::from_path(path))
}

/// The descriptor of every plugin the file at `path` holds, read as `format` or,
/// with none, as its extension says, in the file's own order, or why it
/// could not be read. A CLAP factory gives each plugin it lists, a VST3 factory
/// each audio class, an AU bundle each unit it declares, a VST2 shell each
/// sub-plugin; any other file is one plugin.
/// `main` proves the caller is on the main thread, which VST2's load needs and the
/// other formats' probes do not.
///
/// # Errors
///
/// A [`ProbeRefusal`]: the file is missing, the format is not compiled in or
/// unknown, or the loader refused the file (at the stage it names, where it
/// does).
pub fn describe(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    if !path.exists() {
        return Err(ProbeRefusal::Missing);
    }
    // Only VST2 reads it; a build without that format has nothing to hand it to.
    let _ = main;
    match routing(path, format) {
        #[cfg(feature = "vst3")]
        Some(PluginFormat::Vst3) => vst3(path),
        #[cfg(feature = "clap")]
        Some(PluginFormat::Clap) => clap(path),
        #[cfg(feature = "vst2")]
        Some(PluginFormat::Vst2) => vst2(main, path),
        #[cfg(all(feature = "au", target_os = "macos"))]
        Some(PluginFormat::AudioUnit) => au(path),
        Some(format) => Err(ProbeRefusal::FormatNotCompiled(format)),
        None => Err(ProbeRefusal::UnsupportedFormat),
    }
}

#[cfg(feature = "clap")]
fn clap(path: &Path) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    use tutti_clap_host::{ClapError, ClapLoaded, descriptor_of};
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
    let module = native_module_path(path, ModuleKind::Clap)
        .map_err(|e| ProbeRefusal::Describe(e.to_string()))?;
    let library = (module != path).then_some(module.as_path());
    let infos = ClapLoaded::probe_all(path, library).map_err(|e| match e {
        ClapError::LoadFailed { stage, reason, .. } => ProbeRefusal::Load { stage, reason },
        other => ProbeRefusal::Describe(other.to_string()),
    })?;
    // A probe does not instantiate, and an editor is a property of an instance.
    Ok(infos
        .iter()
        .map(|info| descriptor_of(info, EditorPresence::Unknown))
        .collect())
}

#[cfg(feature = "vst3")]
fn vst3(path: &Path) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
    use tutti_vst3_host::{Vst3Error, Vst3Loaded, descriptor_of};
    let module = native_module_path(path, ModuleKind::Vst3)
        .map_err(|e| ProbeRefusal::Describe(e.to_string()))?;
    let infos = Vst3Loaded::probe_all(&module).map_err(|e| match e {
        Vst3Error::LoadFailed { stage, cause, .. } => ProbeRefusal::Load {
            stage,
            reason: cause.to_string(),
        },
        Vst3Error::PluginError { stage, .. } => ProbeRefusal::Load {
            stage,
            reason: e.to_string(),
        },
        other => ProbeRefusal::Describe(other.to_string()),
    })?;
    Ok(infos
        .iter()
        .map(|info| descriptor_of(info, EditorPresence::Unknown))
        .collect())
}

/// VST2 has no probe short of a load: the plugin's init runs, it is not
/// activated, and the instance is dropped. A shell's sub-plugins are each loaded
/// the same way.
#[cfg(feature = "vst2")]
fn vst2(main: MainThreadMarker, path: &Path) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    use tutti_vst2_host::{Vst2Error, Vst2Loaded};
    Vst2Loaded::describe_all(main, path).map_err(|e| match e {
        Vst2Error::LoadFailed { stage, reason, .. } => ProbeRefusal::Load { stage, reason },
        Vst2Error::Loader { stage, source, .. } => ProbeRefusal::Load {
            stage,
            reason: source.to_string(),
        },
        other => ProbeRefusal::Describe(other.to_string()),
    })
}

/// An AU bundle declares its units in its `Info.plist`; each is described from
/// the registry's component, none created.
#[cfg(all(feature = "au", target_os = "macos"))]
fn au(path: &Path) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    use tutti_plugin_types::PluginError;
    tutti_au_host::AuLoaded::describe_all(path).map_err(|e| match e {
        PluginError::Load { stage, reason } => ProbeRefusal::Load { stage, reason },
        other => ProbeRefusal::Describe(other.to_string()),
    })
}

/// The plugins a measuring probe was asked about: which one takes the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFor<'a> {
    /// The id of the plugin the state is for; `None` is the file's first plugin.
    pub plugin: Option<&'a str>,
    /// The bytes the plugin's own `save_state` wrote.
    pub bytes: &'a [u8],
}

impl StateFor<'_> {
    /// Whether the plugin `id` is the one the state is for: the one named, or, with
    /// none named, the file's only plugin (the caller has checked there is one).
    fn is_for(&self, id: &str) -> bool {
        self.plugin.is_none_or(|wanted| wanted == id)
    }
}

/// [`describe`], then measure each plugin the file holds: the descriptors come
/// back with [`PluginDescriptor::measured`] set where the plugin could be
/// measured, and the plugins that could not be come back beside them, each with
/// its [`MeasureFailure`].
///
/// `state` puts one plugin in a state before it is measured (bytes the plugin's
/// own `save_state` wrote): the plugin it names, or the file's only plugin when it
/// names none. The others are measured as they load, and without `state` every
/// plugin is. `run` is the deadline of the whole run: each plugin's renders are
/// bounded by what it leaves.
///
/// A plugin that crashes or hangs while measuring takes this process with it, so
/// the host that spawned it sees the crash or the timeout of a probe that failed
/// to load.
///
/// # Errors
///
/// As [`describe`]; [`ProbeRefusal::UnknownPlugin`] if `state` names a plugin the
/// file does not hold; [`ProbeRefusal::AmbiguousPlugin`] if it names none and the
/// file holds several; [`ProbeRefusal::StateRefused`] if the plugin took no such state.
pub fn describe_measured(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    state: Option<StateFor<'_>>,
    run: Deadline,
) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
    let mut plugins = describe(main, path, format)?;
    match state.map(|s| s.plugin) {
        Some(Some(wanted)) if !plugins.iter().any(|p| p.id == wanted) => {
            return Err(ProbeRefusal::UnknownPlugin(wanted.to_owned()));
        }
        // A state with no plugin named is for the file's plugin, which must be one.
        Some(None) if plugins.len() != 1 => {
            return Err(ProbeRefusal::AmbiguousPlugin {
                count: plugins.len(),
            });
        }
        _ => {}
    }
    let read_as = routing(path, format);
    let mut unmeasured = Vec::new();
    for plugin in &mut plugins {
        let state = state.filter(|s| s.is_for(&plugin.id)).map(|s| s.bytes);
        match measure_as(read_as, main, path, plugin, state, run) {
            Ok(measured) => plugin.measured = Some(measured),
            Err(MeasureFailure::StateRefused { reason }) => {
                return Err(ProbeRefusal::StateRefused {
                    plugin: plugin.id.clone(),
                    reason,
                });
            }
            Err(failure) => unmeasured.push(Unmeasured {
                plugin: plugin.id.clone(),
                failure,
            }),
        }
    }
    Ok((plugins, unmeasured))
}

/// [`measure()`] with the host of `format`.
fn measure_as(
    format: Option<PluginFormat>,
    main: MainThreadMarker,
    path: &Path,
    plugin: &PluginDescriptor,
    state: Option<&[u8]>,
    run: Deadline,
) -> Result<tutti_plugin_types::Measured, MeasureFailure> {
    match format {
        #[cfg(feature = "vst3")]
        Some(PluginFormat::Vst3) => {
            measure::<tutti_vst3_host::Vst3Loaded>(main, path, plugin, state, run)
        }
        #[cfg(feature = "clap")]
        Some(PluginFormat::Clap) => {
            measure::<tutti_clap_host::ClapLoaded>(main, path, plugin, state, run)
        }
        #[cfg(feature = "vst2")]
        Some(PluginFormat::Vst2) => {
            measure::<tutti_vst2_host::Vst2Loaded>(main, path, plugin, state, run)
        }
        #[cfg(all(feature = "au", target_os = "macos"))]
        Some(PluginFormat::AudioUnit) => {
            measure::<tutti_au_host::AuLoaded>(main, path, plugin, state, run)
        }
        // `describe` read this file as a compiled-in format, so this is not reached.
        _ => Err(MeasureFailure::Load {
            reason: "the format is not compiled into this plugin-probe".to_owned(),
        }),
    }
}
