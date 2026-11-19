//! Every platform but macOS: there are no Audio Units.

use tutti_plugin_types::{
    BridgeError, BridgeResult, Editor, LoadStage, MainThreadMarker, MainWorkHandle, PluginRef,
};

/// An Audio Unit has no editor here: the format exists only on macOS.
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    plugin: &PluginRef,
) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: "Audio Units exist only on macOS".into(),
    })
}
