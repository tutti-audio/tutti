//! Where the C++ twin of the shared plugin is (feature `vst3-cpp`).

use std::path::{Path, PathBuf};

#[cfg_attr(windows, path = "vst3_cpp/ext_windows.rs")]
#[cfg_attr(target_os = "macos", path = "vst3_cpp/ext_macos.rs")]
#[cfg_attr(not(any(windows, target_os = "macos")), path = "vst3_cpp/ext_other.rs")]
mod ext;

/// The C++ plugin's module, in the bundle its crate's `build.rs` links.
#[must_use]
pub fn vst3_cpp_module() -> PathBuf {
    // The extension its `build.rs` gives the module.
    let ext = ext::MODULE_EXT;
    let module = vst3_cpp_bundle()
        .join("Contents")
        .join(tutti_plugin_types::bundle::native_arch_subdirs()[0])
        .join(format!("shared-vst3-cpp.{ext}"));
    assert!(
        module.is_file(),
        "no C++ plugin module at {}",
        module.display()
    );
    module
}

/// The `.vst3` directory around a module. The plugin-server picks the format
/// from a path's extension, so it takes this, not the module: a module path
/// ending in `.so` is loaded as VST2.
///
/// # Panics
///
/// When `module` is not inside a `.vst3` bundle.
#[must_use]
pub fn bundle_of(module: &Path) -> PathBuf {
    tutti_plugin_types::bundle::bundle_dir_of(module, tutti_plugin_types::bundle::ModuleKind::Vst3)
        .expect("a .vst3 bundle around the module")
        .to_path_buf()
}

/// The C++ plugin's bundle directory.
fn vst3_cpp_bundle() -> PathBuf {
    PathBuf::from(env!("SHARED_VST3_CPP_DIR")).join("shared-vst3-cpp.vst3")
}
