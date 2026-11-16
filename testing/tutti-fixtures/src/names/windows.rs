//! Windows: `name.dll`; a `.dll` already reads as VST2.

/// The prefix of a cdylib's file name.
pub(crate) const LIB_PREFIX: &str = "";
/// The extension of a cdylib's file name.
pub(crate) const LIB_EXT: &str = "dll";
/// The extension the VST2 link takes, where the cdylib's own does not read as VST2.
pub(crate) const VST2_LINK_EXT: Option<&str> = None;
