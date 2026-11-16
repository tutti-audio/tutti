//! The file names a platform gives a cdylib, and the extension a VST2 host
//! reads it under.

#[cfg_attr(target_os = "windows", path = "names/windows.rs")]
#[cfg_attr(target_os = "macos", path = "names/macos.rs")]
#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    path = "names/other.rs"
)]
mod platform;

pub(crate) use platform::{LIB_EXT, LIB_PREFIX, VST2_LINK_EXT};
