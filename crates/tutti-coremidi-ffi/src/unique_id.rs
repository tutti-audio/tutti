//! `kMIDIPropertyUniqueID`, the identity of a CoreMIDI object.

use coremidi_sys::{MIDIObjectGetIntegerProperty, MIDIObjectRef, kMIDIPropertyUniqueID};
use std::fmt;

/// A CoreMIDI object's `kMIDIPropertyUniqueID`: an `SInt32` that persists across
/// restarts and device list refreshes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UniqueId(i32);

impl UniqueId {
    /// The id with value `raw`.
    #[must_use]
    pub const fn new(raw: i32) -> Self {
        Self(raw)
    }

    /// The value.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// The id of `object`, `None` for the null reference (an endpoint with no
    /// source or no destination) or an object that has none.
    pub(crate) fn of(object: MIDIObjectRef) -> Option<Self> {
        if object == 0 {
            return None;
        }
        let mut id: i32 = 0;
        // SAFETY: `kMIDIPropertyUniqueID` is a constant CFString CoreMIDI exports,
        // and `id` is a valid out-pointer for the call. A stale reference makes the
        // call return an error status, which reads as `None`.
        let status =
            unsafe { MIDIObjectGetIntegerProperty(object, kMIDIPropertyUniqueID, &mut id) };
        (status == 0).then_some(Self(id))
    }
}

impl fmt::Display for UniqueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
