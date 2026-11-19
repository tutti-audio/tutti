//! The errors of a CoreMIDI call.

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
    pub(crate) fn check(raw: i32) -> Result<(), Self> {
        if raw == 0 { Ok(()) } else { Err(Self(raw)) }
    }
}

impl fmt::Display for OsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OSStatus {}", self.0)
    }
}

/// The CoreMIDI call that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// `MIDIClientCreate`.
    CreateClient,
    /// `MIDISourceCreateWithProtocol`.
    CreateSource,
    /// `MIDIDestinationCreateWithProtocol`.
    CreateDestination,
    /// `MIDIInputPortCreateWithProtocol`.
    CreateInputPort,
    /// `MIDIPortConnectSource`.
    ConnectSource,
    /// `MIDIOutputPortCreate`.
    CreateOutputPort,
    /// `MIDIReceivedEventList`: a virtual source publishing an event list.
    ReceivedEventList,
    /// `MIDISendEventList`: an output port sending to a destination.
    SendEventList,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreateClient => "MIDIClientCreate",
            Self::CreateSource => "MIDISourceCreateWithProtocol",
            Self::CreateDestination => "MIDIDestinationCreateWithProtocol",
            Self::CreateInputPort => "MIDIInputPortCreateWithProtocol",
            Self::ConnectSource => "MIDIPortConnectSource",
            Self::CreateOutputPort => "MIDIOutputPortCreate",
            Self::ReceivedEventList => "MIDIReceivedEventList",
            Self::SendEventList => "MIDISendEventList",
        })
    }
}

/// A CoreMIDI call that failed, or a message CoreMIDI could not be given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A call returned a non-zero status.
    #[error("{operation} failed: {status}")]
    Call {
        /// The call.
        operation: Operation,
        /// What it returned.
        status: OsStatus,
    },
    /// The protocol is a reserved value (only a reader of the wire builds
    /// one), and no endpoint is created in it.
    #[error("no endpoint is created in a reserved protocol")]
    ReservedProtocol,
    /// The process's client has been started: it has one, and a second is not made.
    #[error("the process's CoreMIDI client is already started")]
    ClientExists,
}

impl Error {
    /// `operation` failed with `status`.
    #[must_use]
    pub fn call(operation: Operation, status: OsStatus) -> Self {
        Self::Call { operation, status }
    }
}
