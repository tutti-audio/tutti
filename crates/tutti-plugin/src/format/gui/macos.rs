//! macOS: an Audio Unit's editor loads through `tutti-au-host`.

use std::path::Path;
use tutti_plugin_types::{
    BridgeError, BridgeResult, Editor, Load, LoadStage, MainThreadMarker, MainWorkHandle,
};

/// The editor-only instance of the Audio Unit at `path`.
#[cfg(feature = "au")]
pub(super) fn audio_unit(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    path: &Path,
    load: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    super::editor::<tutti_au_host::AuLoaded>(main, work, path, load, "AU")
}

/// Without the `au` feature the Audio Unit host is not built.
#[cfg(not(feature = "au"))]
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    path: &Path,
    _load: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: Some(path.to_path_buf()),
        stage: LoadStage::Opening,
        reason: "Audio Unit editors need the `au` feature".into(),
    })
}
