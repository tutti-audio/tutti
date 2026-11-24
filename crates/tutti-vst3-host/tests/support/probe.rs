//! Where `build.rs` put the in-repo `audio-probe` plugin, for every test binary
//! that loads it.

use std::path::{Path, PathBuf};

/// The directory `build.rs` builds the in-repo plugins into. Empty only when the
/// `conformance` feature is off.
const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// The path of `audio-probe`'s module, panicking with the build command when it
/// is absent: the probe is built from in-repo sources by this very `cargo test`,
/// so its absence is a build failure, not an environmental one.
#[track_caller]
pub(crate) fn probe_path() -> PathBuf {
    let bundle = Path::new(PROBE_DIR_BUILT).join("audio-probe.vst3");
    tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or_else(|| {
        panic!(
            "audio-probe.vst3 is not present in {PROBE_DIR_BUILT:?}. `build.rs` builds it \
             when the `conformance` feature is on:\n\n  \
             cargo build -p tutti-vst3-host --features conformance --tests\n"
        )
    })
}
