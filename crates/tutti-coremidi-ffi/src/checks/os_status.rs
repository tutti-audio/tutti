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

impl OsStatus {
    /// What a call that writes an out-reference (`MIDIClientCreate`) made: the
    /// reference when it is not 0, whatever the status says, and the failing status
    /// when it is 0.
    ///
    /// A call is trusted to leave 0 in the reference when it fails, but a reference
    /// it did write is an object that exists: a caller that dropped what it handed
    /// the call for it (a callback box) would leave the object a dangling pointer.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn made(raw: i32, reference: u32) -> Result<u32, Self> {
        if reference != 0 {
            return Ok(reference);
        }
        Self::check(raw).map(|()| reference)
    }
}

impl fmt::Display for OsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OSStatus {}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_the_call_wrote_is_made_whatever_the_status() {
        assert_eq!(OsStatus::made(0, 7), Ok(7));
        assert_eq!(OsStatus::made(-304, 7), Ok(7));
    }

    #[test]
    fn no_reference_is_the_failing_status() {
        assert_eq!(OsStatus::made(-304, 0), Err(OsStatus::SERVER_GONE));
        assert_eq!(OsStatus::made(0, 0), Ok(0));
    }
}
