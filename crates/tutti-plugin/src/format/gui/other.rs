//! Every platform but macOS: there are no Audio Units.

use std::path::Path;
use tutti_plugin_types::{
    BridgeError, BridgeResult, Editor, Load, LoadStage, MainThreadMarker, MainWorkHandle,
};

/// An Audio Unit has no editor here: the format exists only on macOS.
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    path: &Path,
    _load: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: Some(path.to_path_buf()),
        stage: LoadStage::Opening,
        reason: "Audio Units exist only on macOS".into(),
    })
}
