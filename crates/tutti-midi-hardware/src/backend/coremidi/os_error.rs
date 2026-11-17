//! The CoreMIDI call that failed, as [`Error::Os`](crate::Error::Os) carries it.

use tutti_coremidi_ffi::OsStatus;

/// A CoreMIDI call that failed: one this backend makes through the `coremidi`
/// wrapper, or one `tutti-coremidi-ffi` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OsError {
    /// `MIDIClientCreate` failed.
    #[error("MIDIClientCreate failed: {0}")]
    CreateClient(OsStatus),
    /// `MIDIInputPortCreateWithProtocol` failed.
    #[error("MIDIInputPortCreateWithProtocol failed: {0}")]
    CreateInputPort(OsStatus),
    /// `MIDIPortConnectSource` failed.
    #[error("MIDIPortConnectSource failed: {0}")]
    ConnectSource(OsStatus),
    /// `MIDIOutputPortCreate` failed.
    #[error("MIDIOutputPortCreate failed: {0}")]
    CreateOutputPort(OsStatus),
    /// `MIDISendEventList` failed.
    #[error("MIDISendEventList failed: {0}")]
    SendEventList(OsStatus),
    /// A call `tutti-coremidi-ffi` makes failed.
    #[error(transparent)]
    Ffi(#[from] tutti_coremidi_ffi::Error),
}
