//! Windows: a published plugin is a copy of the artifact, which can go stale,
//! so it is always republished.

use std::path::Path;

/// A copy can be stale, so none is current.
pub(crate) fn is_published(_link: &Path, _real: &Path) -> bool {
    false
}

/// Makes `staging` a copy of `real`.
pub(crate) fn stage(real: &Path, staging: &Path) {
    std::fs::copy(real, staging).expect("stage the reference plugin copy");
}
