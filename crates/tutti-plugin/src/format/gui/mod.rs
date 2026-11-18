//! In-process plugin editor.
//!
//! The plugin-server subprocess owns audio processing; the editor must live in
//! the host process because platform GUI toolkits are host-process-local. Each
//! format that is [`OutOfProcess`](tutti_plugin_types::OutOfProcess) loads an
//! editor-only second instance here through `load_editor`; each
//! lives in its format's host crate.

use std::path::Path;
use tutti_plugin_types::{BridgeError, BridgeResult, PluginFormat};
use tutti_plugin_types::{Editor, MainThreadMarker, MainWorkHandle};
use tutti_plugin_types::{Load, LoadStage};

// The one platform seam, built with a format host: the platform decides how an
// Audio Unit's editor is loaded (`audio_unit`), and nothing else here names a
// platform.
#[cfg(all(
    any(feature = "vst3", feature = "clap", feature = "au"),
    target_os = "macos"
))]
#[path = "macos.rs"]
mod platform;
#[cfg(all(
    any(feature = "vst3", feature = "clap", feature = "au"),
    not(target_os = "macos")
))]
#[path = "other.rs"]
mod platform;

#[cfg(any(feature = "vst3", feature = "clap", feature = "au"))]
pub(crate) fn load_gui_instance(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    path: &Path,
    format: PluginFormat,
    load: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    // A build whose only format here is AU has no arm that reads them.
    let _ = (main, work, load);
    match format {
        #[cfg(feature = "vst3")]
        PluginFormat::Vst3 => editor::<tutti_vst3_host::Vst3Loaded>(main, work, path, load, "VST3"),
        #[cfg(feature = "clap")]
        PluginFormat::Clap => editor::<tutti_clap_host::ClapLoaded>(main, work, path, load, "CLAP"),
        PluginFormat::AudioUnit => platform::audio_unit(main, work, path, load),
        PluginFormat::Vst2 => Err(BridgeError::LoadFailed {
            path: Some(path.to_path_buf()),
            stage: LoadStage::Opening,
            reason: "VST2 plugins use the in-process backend (enable the \
                     `vst2` feature) — they don't go through the subprocess \
                     GUI loader."
                .into(),
        }),
        // `PluginFormat` is `#[non_exhaustive]` and another crate's, so the
        // arm stays even when every format above is compiled in.
        _ => Err(BridgeError::LoadFailed {
            path: Some(path.to_path_buf()),
            stage: LoadStage::Opening,
            reason: format!("No in-process GUI support for format {format:?}"),
        }),
    }
}

/// `F`'s editor-only instance, boxed.
#[cfg(any(feature = "vst3", feature = "clap", feature = "au"))]
fn editor<F: tutti_plugin_types::OutOfProcess>(
    main: MainThreadMarker,
    work: &MainWorkHandle,
    path: &Path,
    load: &Load,
    format: &str,
) -> BridgeResult<Box<dyn Editor>>
where
    F::Editor: 'static,
{
    let editor = F::load_editor(main, work, path, load).map_err(|e| BridgeError::LoadFailed {
        path: Some(path.to_path_buf()),
        stage: LoadStage::Opening,
        reason: format!("{format} GUI-only load failed: {e}"),
    })?;
    Ok(Box::new(editor))
}

/// With no format host built there is no editor to load.
#[cfg(not(any(feature = "vst3", feature = "clap", feature = "au")))]
pub(crate) fn load_gui_instance(
    _main: MainThreadMarker,
    _work: &MainWorkHandle,
    path: &Path,
    _format: PluginFormat,
    _load: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: Some(path.to_path_buf()),
        stage: LoadStage::Opening,
        reason: "In-process GUI support not built (no format feature enabled: \
                 vst3, clap or au)"
            .into(),
    })
}
