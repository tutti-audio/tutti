//! The error every call here returns.

use windows_sys::Win32::Media::Audio::{MIDIERR_STILLPLAYING, midiOutGetErrorTextW};
use windows_sys::Win32::Media::{
    MMSYSERR_ALLOCATED, MMSYSERR_BADDEVICEID, MMSYSERR_INVALHANDLE, MMSYSERR_NODRIVER,
    MMSYSERR_NOERROR, MMSYSERR_NOMEM,
};

/// The call that failed or did not return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// `midiInOpen`.
    MidiInOpen,
    /// `midiInPrepareHeader`.
    MidiInPrepareHeader,
    /// `midiInAddBuffer`.
    MidiInAddBuffer,
    /// `midiInStart`.
    MidiInStart,
    /// `midiInGetDevCaps`.
    MidiInGetDevCaps,
    /// `midiOutOpen`.
    MidiOutOpen,
    /// `midiOutShortMsg`.
    MidiOutShortMsg,
    /// `midiOutLongMsg`.
    MidiOutLongMsg,
    /// `midiOutPrepareHeader`.
    MidiOutPrepareHeader,
    /// `midiOutUnprepareHeader`.
    MidiOutUnprepareHeader,
    /// `midiOutGetDevCaps`.
    MidiOutGetDevCaps,
    /// `CreateEventW`.
    CreateEvent,
    /// `CreateWaitableTimerExW`.
    CreateWaitableTimer,
    /// `SetWaitableTimer`.
    SetWaitableTimer,
    /// `SetEvent`.
    SetEvent,
    /// `WaitForMultipleObjects`.
    WaitForMultipleObjects,
    /// Starting the input's worker thread.
    SpawnInputWorker,
}

impl Operation {
    /// The call's name, as the Windows documentation spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MidiInOpen => "midiInOpen",
            Self::MidiInPrepareHeader => "midiInPrepareHeader",
            Self::MidiInAddBuffer => "midiInAddBuffer",
            Self::MidiInStart => "midiInStart",
            Self::MidiInGetDevCaps => "midiInGetDevCaps",
            Self::MidiOutOpen => "midiOutOpen",
            Self::MidiOutShortMsg => "midiOutShortMsg",
            Self::MidiOutLongMsg => "midiOutLongMsg",
            Self::MidiOutPrepareHeader => "midiOutPrepareHeader",
            Self::MidiOutUnprepareHeader => "midiOutUnprepareHeader",
            Self::MidiOutGetDevCaps => "midiOutGetDevCaps",
            Self::CreateEvent => "CreateEventW",
            Self::CreateWaitableTimer => "CreateWaitableTimerExW",
            Self::SetWaitableTimer => "SetWaitableTimer",
            Self::SetEvent => "SetEvent",
            Self::WaitForMultipleObjects => "WaitForMultipleObjects",
            Self::SpawnInputWorker => "spawn the input worker",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// An `MMRESULT` a winmm call returned: the `MMSYSERR_*` and `MIDIERR_*` code
/// space, which is not the Win32 `GetLastError` space. Its text is winmm's own
/// for the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MmResult {
    /// `MMSYSERR_ALLOCATED`: another application holds the port.
    #[error("{}", text(MMSYSERR_ALLOCATED))]
    Allocated,
    /// `MMSYSERR_BADDEVICEID`: the port is gone, or never was.
    #[error("{}", text(MMSYSERR_BADDEVICEID))]
    BadDeviceId,
    /// `MIDIERR_STILLPLAYING`: a buffer is still queued with the driver.
    #[error("{}", text(MIDIERR_STILLPLAYING))]
    StillPlaying,
    /// `MMSYSERR_NODRIVER`: the port's driver is gone.
    #[error("{}", text(MMSYSERR_NODRIVER))]
    NoDriver,
    /// `MMSYSERR_NOMEM`: the driver could not allocate.
    #[error("{}", text(MMSYSERR_NOMEM))]
    NoMem,
    /// `MMSYSERR_INVALHANDLE`: the handle names no open port.
    #[error("{}", text(MMSYSERR_INVALHANDLE))]
    InvalHandle,
    /// Any other nonzero `MMRESULT`.
    #[error("{}", text(*.0))]
    Other(u32),
}

impl MmResult {
    /// The variant for a nonzero `MMRESULT`.
    #[must_use]
    pub const fn from_code(code: u32) -> Self {
        match code {
            MMSYSERR_ALLOCATED => Self::Allocated,
            MMSYSERR_BADDEVICEID => Self::BadDeviceId,
            MIDIERR_STILLPLAYING => Self::StillPlaying,
            MMSYSERR_NODRIVER => Self::NoDriver,
            MMSYSERR_NOMEM => Self::NoMem,
            MMSYSERR_INVALHANDLE => Self::InvalHandle,
            other => Self::Other(other),
        }
    }
}

/// How long winmm's error text may be, in UTF-16 units (`MAXERRORLENGTH`).
const TEXT_UNITS: usize = 256;

/// winmm's text for `code`, or the code when it has none.
fn text(code: u32) -> String {
    let mut buffer = [0u16; TEXT_UNITS];
    // SAFETY: `buffer` is a live array of `TEXT_UNITS` units, which is the
    // length passed; the call writes a NUL-terminated string into it.
    let result = unsafe { midiOutGetErrorTextW(code, buffer.as_mut_ptr(), TEXT_UNITS as u32) };
    if result != MMSYSERR_NOERROR {
        return format!("code {code}");
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(TEXT_UNITS);
    String::from_utf16_lossy(&buffer[..end])
}

/// What winmm did not do: a call that failed, a call that never returned, or a
/// message too long to send.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A winmm call returned a failure.
    #[error("winmm error: {operation}: {source}")]
    Call {
        /// The call that failed.
        operation: Operation,
        /// The `MMRESULT` it returned.
        source: MmResult,
    },
    /// A Win32 or standard-library call failed; `source` is its
    /// `GetLastError` or the operating system's error.
    #[error("winmm error: {operation}: {source}")]
    Os {
        /// The call that failed.
        operation: Operation,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// A call did not return within its bound, so winmm still holds what it was
    /// given: a buffer after a timeout.
    #[error("winmm error: {operation} did not return")]
    NotReturned {
        /// The call that did not return.
        operation: Operation,
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
}

/// `Ok` for `MMSYSERR_NOERROR`, the failure of `operation` otherwise.
pub(crate) fn check(operation: Operation, code: u32) -> Result<(), Error> {
    if code == MMSYSERR_NOERROR {
        Ok(())
    } else {
        Err(Error::Call {
            operation,
            source: MmResult::from_code(code),
        })
    }
}

/// The calling thread's `GetLastError` as the failure of `operation`.
pub(crate) fn last_os(operation: Operation) -> Error {
    Error::Os {
        operation,
        source: std::io::Error::last_os_error(),
    }
}
