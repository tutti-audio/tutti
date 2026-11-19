//! Where CLAP runs: in the host's process, and served from another one with
//! an editor-only instance here.

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{
    Describe, EditorPresence, InProc, MainThreadMarker, MainWorkHandle, OutOfProcess,
    PluginDescriptor, PluginFormat, PluginId, PluginRef, PluginResult,
};

use super::ClapLoaded;
use super::control::descriptor_of;
use super::editor_instance::ClapEditor;
use super::lifecycle::plugin_error;
use super::load::LoadOptions;

fn options(plugin: &PluginRef) -> PluginResult<LoadOptions> {
    Ok(match plugin.id_for(PluginFormat::Clap)? {
        Some(PluginId::Clap(id)) => LoadOptions::new().plugin_id(id),
        _ => LoadOptions::new(),
    })
}

impl InProc for ClapLoaded {
    fn load_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<Self> {
        let module = native_module_path(plugin.path(), ModuleKind::Clap)?;
        ClapLoaded::load(main, work, &module, options(plugin)?).map_err(plugin_error)
    }

    fn origin(&self) -> &PluginRef {
        &self.origin
    }

    fn main_work(&self) -> &MainWorkHandle {
        &self.work
    }
}

impl Describe for ClapLoaded {
    /// The factory's plugins, read without instantiating one: an editor is a
    /// property of an instance, so each is [`EditorPresence::Unknown`].
    fn describe(
        _main: MainThreadMarker,
        path: &Path,
    ) -> PluginResult<Vec<(PluginId, PluginDescriptor)>> {
        let module = native_module_path(path, ModuleKind::Clap)?;
        let library = (module != path).then_some(module.as_path());
        let infos = ClapLoaded::probe_all(path, library).map_err(plugin_error)?;
        Ok(infos
            .iter()
            .map(|info| {
                (
                    PluginId::Clap(info.id.clone()),
                    descriptor_of(info, EditorPresence::Unknown),
                )
            })
            .collect())
    }
}

impl OutOfProcess for ClapLoaded {
    type Editor = ClapEditor;

    fn load_served(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<Self> {
        Self::load_in_process(main, work, plugin)
    }

    fn load_editor(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<ClapEditor> {
        ClapEditor::load(main, work, plugin)
    }
}
