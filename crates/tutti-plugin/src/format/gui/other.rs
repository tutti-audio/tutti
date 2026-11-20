//! Every platform but macOS: there are no Audio Units.

use tutti_plugin_types::{
    Editor, IpcError, IpcResult, LoadStage, MainThreadMarker, MainWorkHandle, PluginRef,
};

/// An Audio Unit has no editor here: the format exists only on macOS.
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    plugin: &PluginRef,
) -> IpcResult<Box<dyn Editor>> {
    Err(IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: "Audio Units exist only on macOS".into(),
    })
}
