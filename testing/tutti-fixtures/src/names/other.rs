//! Linux and the other platforms: `libname.so`; a `.so` already reads as VST2.

/// The prefix of a cdylib's file name.
pub(crate) const LIB_PREFIX: &str = "lib";
/// The extension of a cdylib's file name.
pub(crate) const LIB_EXT: &str = "so";
/// The extension the VST2 link takes, where the cdylib's own does not read as VST2.
pub(crate) const VST2_LINK_EXT: Option<&str> = None;
