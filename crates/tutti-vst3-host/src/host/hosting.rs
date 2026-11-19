//! Where VST3 runs: in the host's process, and served from another one with
//! an editor-only instance here.

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{
    Describe, EditorPresence, InProcess, MainThreadMarker, MainWorkHandle, OutOfProcess,
    PluginDescriptor, PluginFormat, PluginId, PluginRef, PluginResult,
};

use super::control::{descriptor_of, plugin_error};
use super::editor_instance::Vst3Editor;
use super::loaded::{LoadOptions, Vst3Loaded};

impl InProcess for Vst3Loaded {
    fn load_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<Self> {
        let module = native_module_path(plugin.path(), ModuleKind::Vst3)?;
        let options = match plugin.id_for(PluginFormat::Vst3)? {
            Some(PluginId::Vst3(class)) => LoadOptions::new().class_id(*class),
            _ => LoadOptions::new(),
        };
        Vst3Loaded::load(main, work, &module, options).map_err(plugin_error)
    }

    fn origin(&self) -> &PluginRef {
        &self.origin
    }

    fn main_work(&self) -> &MainWorkHandle {
        &self.work
    }
}

impl Describe for Vst3Loaded {
    /// The factory's audio classes that can be created, read without
    /// initializing one: an editor is a property of an instance, so each is
    /// [`EditorPresence::Unknown`].
    fn describe(
        _main: MainThreadMarker,
        path: &Path,
    ) -> PluginResult<Vec<(PluginId, PluginDescriptor)>> {
        let module = native_module_path(path, ModuleKind::Vst3)?;
        let infos = Vst3Loaded::probe_all(&module).map_err(plugin_error)?;
        Ok(infos
            .iter()
            .map(|info| {
                (
                    PluginId::Vst3(info.id),
                    descriptor_of(info, EditorPresence::Unknown),
                )
            })
            .collect())
    }
}

impl OutOfProcess for Vst3Loaded {
    type Editor = Vst3Editor;

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
    ) -> PluginResult<Vst3Editor> {
        Vst3Editor::load(main, work, plugin)
    }
}
