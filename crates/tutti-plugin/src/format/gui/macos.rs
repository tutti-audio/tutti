//! macOS: an Audio Unit's editor loads through `tutti-au-host`.

use tutti_plugin_types::{
    Editor, IpcError, IpcResult, LoadStage, MainThreadMarker, MainWorkHandle, PluginRef,
};

/// The editor-only instance of the Audio Unit at `path`.
#[cfg(feature = "au")]
pub(super) fn audio_unit(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    plugin: &PluginRef,
) -> IpcResult<Box<dyn Editor>> {
    let editor = <tutti_au_host::AuLoaded as tutti_plugin_types::OutOfProcess>::load_editor(
        main, work, plugin,
    )
    .map_err(|e| IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: format!("AU GUI-only load failed: {e}"),
    })?;
    Ok(Box::new(editor))
}

/// Without the `au` feature the Audio Unit host is not built.
#[cfg(not(feature = "au"))]
pub(super) fn audio_unit(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    plugin: &PluginRef,
) -> IpcResult<Box<dyn Editor>> {
    Err(IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: "Audio Unit editors need the `au` feature".into(),
    })
}
