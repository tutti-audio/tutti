//! Off Linux there is no `/proc` to read: the count is unknown.

/// The plugin servers this process has running: `None`, as `/proc` is not
/// there to read.
pub(super) fn servers() -> Option<usize> {
    None
}
