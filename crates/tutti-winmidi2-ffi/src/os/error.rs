//! The error every call here returns.

use crate::checks::declared::Refusal;

/// Why a call into Windows MIDI Services failed.
///
/// A code is the `HRESULT` the call returned, as the signed 32-bit value Windows
/// documents it as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The API is not there: its activation failed. Until Windows has the API in the box,
    /// the app ships `Windows.Devices.Midi2.dll` beside its executable.
    #[error(
        "the Windows MIDI Services API is not present (activation returned {code:#010x}); \
         `Windows.Devices.Midi2.dll` must be beside the executable until Windows has it"
    )]
    ApiMissing {
        /// The activation's `HRESULT`.
        code: i32,
    },
    /// The API is there and the MIDI service is not running: it is disabled, or the PC is in
    /// its Legacy API mode.
    #[error("the Windows MIDI service is not available")]
    ServiceUnavailable,
    /// A call failed.
    #[error("{call} failed ({code:#010x})")]
    Call {
        /// The WinRT member that failed.
        call: &'static str,
        /// Its `HRESULT`.
        code: i32,
    },
    /// A call answered with nothing or `false`, which is how the API reports a refusal.
    #[error("{call} was refused")]
    Refused {
        /// The WinRT member that refused.
        call: &'static str,
    },
    /// The connection is closed: by `close`, by its session closing, or by the service.
    #[error("the endpoint connection is closed")]
    Closed,
    /// The service's message queue for the connection is full.
    #[error("the service's message buffer is full")]
    BufferFull,
    /// A time is not on the service's clock: it is before the clock's origin or beyond what
    /// a timestamp holds, or the clock reports no frequency.
    #[error("the time is not a timestamp of the service's clock")]
    Timestamp,
    /// The service delivered words that are not one whole UMP message.
    #[error("the service delivered {words} words, which are not one UMP message")]
    NotAMessage {
        /// How many words the service said the message had.
        words: u8,
    },
    /// The endpoint watcher stopped on its own: the service went away or the watcher failed.
    #[error("the endpoint device watcher stopped")]
    WatcherStopped,
    /// What a virtual device declares has a field the service has no number for, or a
    /// count that is not its blocks.
    #[error("the virtual device's declaration cannot be carried: {0}")]
    Declaration(#[from] Refusal),
    /// The service refused the message for a reason that is not a closed connection or a
    /// full buffer.
    #[error("the service refused the message (result {bits:#x})")]
    MessageRefused {
        /// The `MidiSendMessageResults` bits the send returned.
        bits: u32,
    },
}

/// The conversion of a failed call `call` to its error.
pub(super) fn call(call: &'static str) -> impl FnOnce(windows_core::Error) -> Error {
    move |e| Error::Call {
        call,
        code: e.code().0,
    }
}
