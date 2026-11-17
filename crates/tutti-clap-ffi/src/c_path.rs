//! A filesystem path as the C string a CLAP entry takes.

use std::ffi::{CString, NulError};
use std::path::Path;

/// The path as the bytes the platform's filesystem holds, nul-terminated.
///
/// # Errors
/// The path holds a nul byte.
pub(crate) fn c_path(path: &Path) -> Result<CString, NulError> {
    imp::bytes(path).and_then(CString::new)
}

#[cfg(unix)]
mod imp {
    use std::ffi::NulError;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// The OS string's own bytes: a path that is not UTF-8 is kept whole.
    pub(super) fn bytes(path: &Path) -> Result<Vec<u8>, NulError> {
        Ok(path.as_os_str().as_bytes().to_vec())
    }
}

#[cfg(not(unix))]
mod imp {
    use std::ffi::NulError;
    use std::path::Path;

    /// The path's UTF-8 form: the encoding Windows plugins load by.
    pub(super) fn bytes(path: &Path) -> Result<Vec<u8>, NulError> {
        Ok(path.to_string_lossy().into_owned().into_bytes())
    }
}
