//! Where VST2 runs: in the host's process only (the reason is on
//! [`Vst2Loaded`]).

use std::path::Path;

use tutti_plugin_types::bundle::{ModuleKind, native_module_path};
use tutti_plugin_types::{
    Describe, InProcess, MainThreadMarker, MainWorkHandle, PluginDescriptor, PluginFormat, PluginId,
    PluginRef, PluginResult,
};

use crate::control::plugin_error;
use crate::loaded::Vst2Loaded;

impl InProcess for Vst2Loaded {
    /// # Errors
    ///
    /// Also refuses a chosen plugin that is not a VST2 id of a plugin the file
    /// holds: a shell's sub-plugin, or the one plugin of a file that is no shell.
    fn load_in_process(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<Self> {
        let module = native_module_path(plugin.path(), ModuleKind::Vst2)?;
        match plugin.id_for(PluginFormat::Vst2)? {
            Some(PluginId::Vst2(id)) => {
                // the plugin's unique id is signed; the id holds its bits.
                Vst2Loaded::load_plugin(main, work, &module, *id as i32)
            }
            _ => Vst2Loaded::load(main, work, &module),
        }
        .map_err(plugin_error)
    }

    fn origin(&self) -> &PluginRef {
        &self.origin
    }

    fn main_work(&self) -> &MainWorkHandle {
        &self.work
    }
}

impl Describe for Vst2Loaded {
    /// A load of the file (init runs, no resume), and of each sub-plugin of a shell.
    fn describe(
        main: MainThreadMarker,
        path: &Path,
    ) -> PluginResult<Vec<(PluginId, PluginDescriptor)>> {
        Vst2Loaded::describe_all(main, path).map_err(plugin_error)
    }
}
