//! Every platform but macOS: there are no Audio Units.

use tutti_plugin_types::{
    Editor, IpcError, IpcResult, MainThreadMarker, MainWorkHandle, NoEditorHost, PluginRef,
};

/// An Audio Unit has no editor here: the format exists only on macOS.
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    plugin: &PluginRef,
) -> IpcResult<Box<dyn Editor>> {
    Err(IpcError::NoEditorHost {
        path: plugin.path().to_path_buf(),
        why: NoEditorHost::AudioUnitOffMacos,
    })
}
