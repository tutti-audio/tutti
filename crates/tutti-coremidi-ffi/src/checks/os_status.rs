//! A CoreMIDI `OSStatus`, the code a failed call returned.

use std::fmt;

/// A CoreMIDI `OSStatus`: the code a call returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OsStatus(i32);

impl OsStatus {
    /// The status `MIDIClientCreate` returns while the MIDI server is exiting.
    ///
    /// `MIDIServices.h` names no constant for it. The number is the Carbon code
    /// `smNoDir` (`MacErrors.h`), which CoreMIDI reports when its Mach connection
    /// to the server fails.
    pub(crate) const SERVER_GONE: Self = Self(-304);

    /// `kMIDINotPermitted` (`MIDIServices.h`): CoreMIDI refuses the call.
    pub(crate) const NOT_PERMITTED: Self = Self(-10844);

    /// The status with code `raw`.
    #[must_use]
    pub const fn new(raw: i32) -> Self {
        Self(raw)
    }

    /// The code.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// `Ok` for status 0 (`noErr`), the failing status otherwise.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn check(raw: i32) -> Result<(), Self> {
        if raw == 0 { Ok(()) } else { Err(Self(raw)) }
    }
}

impl fmt::Display for OsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OSStatus {}", self.0)
    }
}
