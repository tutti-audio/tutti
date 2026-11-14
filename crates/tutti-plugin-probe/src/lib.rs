#![doc = include_str!("../README.md")]
//! What `plugin-probe` does, as a library so a test can call it in-process: read
//! the descriptor of every plugin the file at a path holds, by the path's extension.
//!
//! The binary (`src/main.rs`) is the command line around [`describe`].

#![forbid(unsafe_code)]

use std::path::Path;
use tutti_plugin_types::{MainThreadMarker, PluginDescriptor};

mod measure;

pub use measure::{
    BUDGET_MARGIN, CONFIGS, IMPULSE, NOTE, NOTE_LENGTH, SETTLE, SILENCE, Subject, measure,
    measure_at,
};

/// A plugin format a caller can name, to have a file read as it whatever the
/// file's extension (a link `Foo.clap` to a file named `libfoo.so` is CLAP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// CLAP.
    Clap,
    /// Steinberg VST3.
    Vst3,
    /// Steinberg VST2.
    Vst2,
    /// Apple Audio Unit.
    Au,
}

impl Format {
    /// The format a command-line name (`clap`, `vst3`, `vst2`, `au`) names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "clap" => Some(Self::Clap),
            "vst3" => Some(Self::Vst3),
            "vst2" => Some(Self::Vst2),
            "au" => Some(Self::Au),
            _ => None,
        }
    }
}

/// The routing key of a file: the extension of the loader that reads it. It is
/// `format`'s when the caller names one, and the path's own extension otherwise.
fn routing(path: &Path, format: Option<Format>) -> String {
    match format {
        Some(Format::Clap) => "clap".to_string(),
        Some(Format::Vst3) => "vst3".to_string(),
        Some(Format::Vst2) => "vst".to_string(),
        Some(Format::Au) => "component".to_string(),
        None => path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase(),
    }
}

/// The descriptor of every plugin the file at `path` holds, read as `format` or,
/// with none, as its extension says, in the file's own order, or the reason it
/// could not be read. A CLAP factory
/// gives each plugin it lists, a VST3 factory each audio class, an AU bundle each
/// unit it declares, a VST2 shell each sub-plugin; any other file is one plugin.
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
    format: Option<Format>,
) -> Result<Vec<PluginDescriptor>, String> {
    if !path.exists() {
        return Err(format!("Plugin not found: {}", path.display()));
    }
    let extension = routing(path, format);
    // Only VST2 reads it; a build without that format has nothing to hand it to.
    let _ = main;
    match extension.as_str() {
        #[cfg(feature = "vst3")]
        "vst3" => vst3(path),
        #[cfg(feature = "clap")]
        "clap" => clap(path),
        #[cfg(feature = "vst2")]
        "vst" | "dll" | "so" => vst2(main, path),
        #[cfg(all(feature = "au", target_os = "macos"))]
        "component" => au(path),
        _ => Err(format!("Unsupported format: .{extension}")),
    }
}

/// The module a bundle directory holds for this target, or `path` itself when it
/// is a file (or names nothing, which the loader reports in its own words).
#[cfg(any(feature = "clap", feature = "vst3"))]
fn module_of(
    path: &Path,
    kind: tutti_plugin_types::bundle::ModuleKind,
) -> Result<std::path::PathBuf, String> {
    use tutti_plugin_types::bundle::{native_arch_subdirs, native_module_in_bundle};
    if path.is_file() || !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    // `native_*`, not `any_*`: a module built for another CPU is a load failure
    // with extra steps, and "no module for this architecture" says more.
    native_module_in_bundle(path, kind).ok_or_else(|| {
        format!(
            "no plugin module in {} for this architecture (looked in Contents/{{{}}})",
            path.display(),
            native_arch_subdirs().join(", ")
        )
    })
}

#[cfg(feature = "clap")]
fn clap(path: &Path) -> Result<Vec<PluginDescriptor>, String> {
    use tutti_clap_host::{ClapLoaded, descriptor_of};
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::ModuleKind;
    let module = module_of(path, ModuleKind::Clap)?;
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
    use tutti_plugin_types::bundle::ModuleKind;
    use tutti_vst3_host::{Vst3Loaded, descriptor_of};
    let module = module_of(path, ModuleKind::Vst3)?;
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
    format: Option<Format>,
    state: Option<&[u8]>,
) -> Result<Vec<PluginDescriptor>, String> {
    let mut plugins = describe(main, path, format)?;
    let extension = routing(path, format);
    for plugin in &mut plugins {
        plugin.measured = match extension.as_str() {
            #[cfg(feature = "vst3")]
            "vst3" => measure::<tutti_vst3_host::Vst3Loaded>(main, path, plugin, state)?,
            #[cfg(feature = "clap")]
            "clap" => measure::<tutti_clap_host::ClapLoaded>(main, path, plugin, state)?,
            #[cfg(feature = "vst2")]
            "vst" | "dll" | "so" => {
                measure::<tutti_vst2_host::Vst2Loaded>(main, path, plugin, state)?
            }
            #[cfg(all(feature = "au", target_os = "macos"))]
            "component" => measure::<tutti_au_host::AuLoaded>(main, path, plugin, state)?,
            _ => None,
        };
    }
    Ok(plugins)
}
