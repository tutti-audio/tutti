//! Any other architecture: the mode is left as it is.

/// Nothing to save.
pub(super) struct Saved;

/// Changes nothing.
#[inline]
pub(super) fn set() -> Saved {
    Saved
}

/// Restores nothing.
#[inline]
pub(super) fn restore(_saved: &Saved) {}
