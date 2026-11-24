//! `kMIDIPropertyUniqueID`, the identity of a CoreMIDI object.

use std::fmt;

/// A CoreMIDI object's `kMIDIPropertyUniqueID`: an `SInt32` that persists across
/// restarts and device list refreshes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UniqueId(i32);

impl UniqueId {
    /// The id with value `raw`.
    #[must_use]
    pub(crate) const fn new(raw: i32) -> Self {
        Self(raw)
    }

    /// The value.
    #[must_use]
    pub(crate) const fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for UniqueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
