//! The error every call here returns.

use windows_sys::Win32::Media::MMSYSERR_NOERROR;

/// What winmm did not do: a call that failed, a call that never returned, a
/// message too long to send, or a thread that did not start.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A winmm call returned a failure.
    #[error("winmm error: {operation} ({code})")]
    Call {
        /// What was being done, as a static label.
        operation: &'static str,
        /// The `MMRESULT` winmm returned (`MMSYSERR_*` or `MIDIERR_*`).
        code: u32,
    },
    /// A call did not return within its bound, so winmm still holds what it was
    /// given: a buffer after a timeout.
    #[error("winmm error: {operation} did not return")]
    NotReturned {
        /// What was being done, as a static label.
        operation: &'static str,
    },
    /// A System Exclusive message longer than a buffer winmm takes.
    #[error("winmm error: a message of {len} bytes is too long to send")]
    TooLong {
        /// The message's length in bytes.
        len: usize,
        /// Why the length does not fit.
        #[source]
        source: std::num::TryFromIntError,
    },
    /// A worker thread did not start.
    #[error("winmm error: {operation} failed")]
    Spawn {
        /// What was being done, as a static label.
        operation: &'static str,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
}

/// `Ok` for `MMSYSERR_NOERROR`, the failure of `operation` otherwise.
pub(crate) fn check(operation: &'static str, code: u32) -> Result<(), Error> {
    if code == MMSYSERR_NOERROR {
        Ok(())
    } else {
        Err(Error::Call { operation, code })
    }
}
