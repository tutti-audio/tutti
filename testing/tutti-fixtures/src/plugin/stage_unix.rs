//! Unix: the bundle's module is a symlink to the shared plugin.

use std::path::Path;

/// Puts a symlink to `real` at `module` inside `dir`, unless it is there already.
pub(super) fn module(real: &Path, dir: &Path, module: &Path) {
    if !std::fs::read_link(module).is_ok_and(|to| to == real) {
        let staging = dir.join(format!(".stage{}", std::process::id()));
        let _ = std::fs::remove_file(&staging);
        std::os::unix::fs::symlink(real, &staging).expect("stage the bundle link");
        let _ = std::fs::rename(&staging, module);
        let _ = std::fs::remove_file(&staging);
    }
}
