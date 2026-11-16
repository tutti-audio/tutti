//! macOS: `libname.dylib`, which a VST2 host reads as no format, so the VST2
//! plugin is linked under `.vst`.

/// The prefix of a cdylib's file name.
pub(crate) const LIB_PREFIX: &str = "lib";
/// The extension of a cdylib's file name.
pub(crate) const LIB_EXT: &str = "dylib";
/// The extension the VST2 link takes, where the cdylib's own does not read as VST2.
pub(crate) const VST2_LINK_EXT: Option<&str> = Some("vst");
