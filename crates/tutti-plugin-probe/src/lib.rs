#![doc = include_str!("../README.md")]
//! What `plugin-probe` does, as a library so a test can call it in-process: read
//! the descriptor of every plugin the file at a path holds, by the path's extension.
//!
//! The binary (`src/main.rs`) is the command line around [`describe`].

#![forbid(unsafe_code)]

use std::path::Path;
use tutti_plugin_db::format_from_path;
use tutti_plugin_types::{MainThreadMarker, PluginDescriptor, PluginFormat};

mod measure;

pub use measure::{
    BUDGET_MARGIN, CONFIGS, IMPULSE, NOTE, NOTE_LENGTH, SETTLE, SILENCE, Subject, measure,
    measure_at,
};

/// The format a file is read as: `format` when the caller names one, and the
/// one its extension names otherwise.
fn routing(path: &Path, format: Option<PluginFormat>) -> Option<PluginFormat> {
    format.or_else(|| format_from_path(path))
}

/// The descriptor of every plugin the file at `path` holds, read as `format` or,
/// with none, as its extension says, in the file's own order, or the reason it
/// could not be read. A CLAP factory gives each plugin it lists, a VST3 factory
/// each audio class, an AU bundle each unit it declares, a VST2 shell each
/// sub-plugin; any other file is one plugin.
/// `main` proves the caller is on the main thread, which VST2's load needs and the
/// other formats' probes do not.
///
/// # Errors
///
/// The loader's own reason, as text: the file is missing, the format is not
/// compiled in or unknown, or the file would not open.
pub fn describe(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
) -> Result<Vec<PluginDescriptor>, String> {
    if !path.exists() {
        return Err(format!("Plugin not found: {}", path.display()));
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
        Some(format) => Err(format!(
            "format {} is not compiled into this plugin-probe",
            format.extension_id()
        )),
        None => Err(format!("Unsupported format: {}", path.display())),
    }
}

#[cfg(feature = "clap")]
fn clap(path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    use tutti_clap_host::{ClapLoaded, descriptor_of};
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
    let module = native_module_path(path, ModuleKind::Clap).map_err(|e| e.to_string())?;
    let library = (module != path).then_some(module.as_path());
    let infos = ClapLoaded::probe_all(path, library).map_err(|e| e.to_string())?;
    // A probe does not instantiate, and an editor is a property of an instance.
    Ok(infos
        .iter()
        .map(|info| descriptor_of(info, EditorPresence::Unknown))
        .collect())
}

#[cfg(feature = "vst3")]
fn vst3(path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
    use tutti_vst3_host::{Vst3Loaded, descriptor_of};
    let module = native_module_path(path, ModuleKind::Vst3).map_err(|e| e.to_string())?;
    let infos = Vst3Loaded::probe_all(&module).map_err(|e| e.to_string())?;
    Ok(infos
        .iter()
        .map(|info| descriptor_of(info, EditorPresence::Unknown))
        .collect())
}

/// VST2 has no probe short of a load: the plugin's init runs, it is not
/// activated, and the instance is dropped. A shell's sub-plugins are each loaded
/// the same way.
#[cfg(feature = "vst2")]
fn vst2(main: MainThreadMarker, path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    use tutti_vst2_host::Vst2Loaded;
    Vst2Loaded::describe_all(main, path).map_err(|e| e.to_string())
}

/// An AU bundle declares its units in its `Info.plist`; each is described from
/// the registry's component, none created.
#[cfg(all(feature = "au", target_os = "macos"))]
fn au(path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    tutti_au_host::AuLoaded::describe_all(path).map_err(|e| e.to_string())
}

/// [`describe`], then measure each plugin the file holds:
/// the descriptors come back with [`PluginDescriptor::measured`] set where the
/// plugin could be measured, and `None` where it could not (it would not load
/// for a render, has no audio output, or failed a block).
///
/// `state` puts each plugin in a state before it is measured (bytes the plugin's
/// own `save_state` wrote); without it the plugin is measured as it loads. It is
/// meant for a file of one plugin.
///
/// A plugin that crashes or hangs while measuring takes this process with it, so
/// the host that spawned it sees the crash or the timeout of a probe that failed
/// to load.
///
/// # Errors
///
/// As [`describe`], or a plugin that refused `state`.
pub fn describe_measured(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    state: Option<&[u8]>,
) -> Result<Vec<PluginDescriptor>, String> {
    let mut plugins = describe(main, path, format)?;
    let read_as = routing(path, format);
    for plugin in &mut plugins {
        plugin.measured = match read_as {
            #[cfg(feature = "vst3")]
            Some(PluginFormat::Vst3) => {
                measure::<tutti_vst3_host::Vst3Loaded>(main, path, plugin, state)?
            }
            #[cfg(feature = "clap")]
            Some(PluginFormat::Clap) => {
                measure::<tutti_clap_host::ClapLoaded>(main, path, plugin, state)?
            }
            #[cfg(feature = "vst2")]
            Some(PluginFormat::Vst2) => {
                measure::<tutti_vst2_host::Vst2Loaded>(main, path, plugin, state)?
            }
            #[cfg(all(feature = "au", target_os = "macos"))]
            Some(PluginFormat::AudioUnit) => {
                measure::<tutti_au_host::AuLoaded>(main, path, plugin, state)?
            }
            _ => None,
        };
    }
    Ok(plugins)
}
