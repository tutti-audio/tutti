//! Where CLAP runs: in the host's process, and served from another one with
//! an editor-only instance here.

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{
    Describe, EditorPresence, InProc, Load, MainThreadMarker, MainWorkHandle, Origin, OutOfProcess,
    PluginDescriptor, PluginResult,
};

use super::ClapLoaded;
use super::control::descriptor_of;
use super::editor_instance::ClapEditor;
use super::lifecycle::plugin_error;
use super::load::LoadOptions;

fn options(load: &Load) -> LoadOptions {
    match load.chosen() {
        Some(id) => LoadOptions::new().plugin_id(id),
        None => LoadOptions::new(),
    }
}

impl InProc for ClapLoaded {
    fn load_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<Self> {
        let module = native_module_path(path, ModuleKind::Clap)?;
        ClapLoaded::load(main, work, &module, options(load)).map_err(plugin_error)
    }

    fn origin(&self) -> &Origin {
        &self.origin
    }

    fn main_work(&self) -> &MainWorkHandle {
        &self.work
    }
}

impl Describe for ClapLoaded {
    /// The factory's plugins, read without instantiating one: an editor is a
    /// property of an instance, so each is [`EditorPresence::Unknown`].
    fn describe(_main: MainThreadMarker, path: &Path) -> PluginResult<Vec<PluginDescriptor>> {
        let module = native_module_path(path, ModuleKind::Clap)?;
        let library = (module != path).then_some(module.as_path());
        let infos = ClapLoaded::probe_all(path, library).map_err(plugin_error)?;
        Ok(infos
            .iter()
            .map(|info| descriptor_of(info, EditorPresence::Unknown))
            .collect())
    }
}

impl OutOfProcess for ClapLoaded {
    type Editor = ClapEditor;

    fn load_served(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<Self> {
        Self::load_in_process(main, work, path, load)
    }

    fn load_editor(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        path: &Path,
        load: &Load,
    ) -> PluginResult<ClapEditor> {
        ClapEditor::load(main, work, path, load)
    }
}
