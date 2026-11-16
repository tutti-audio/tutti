//! Windows: the bundle's module is a copy of the shared plugin.

use std::path::Path;

/// Puts a copy of `real` at `module` inside `dir`, unless a whole one is there.
pub(super) fn module(real: &Path, dir: &Path, module: &Path) {
    // A copy, so a process loading it must never see it half written: stage
    // beside it and rename, and leave one that is already whole alone (a
    // loaded DLL cannot be replaced).
    let whole = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| Ok(m.len() == std::fs::metadata(real)?.len()))
            .unwrap_or(false)
    };
    if !whole(module) {
        let staging = dir.join(format!(".stage{}", std::process::id()));
        std::fs::copy(real, &staging).expect("stage the bundle copy");
        let _ = std::fs::rename(&staging, module);
        let _ = std::fs::remove_file(&staging);
    }
}
