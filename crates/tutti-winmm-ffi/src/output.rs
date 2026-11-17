//! An open winmm output.

use std::time::{Duration, Instant};

use windows_sys::Win32::Media::Audio::{
    CALLBACK_NULL, HMIDIOUT, MIDIERR_STILLPLAYING, MIDIHDR, midiOutClose, midiOutLongMsg,
    midiOutOpen, midiOutPrepareHeader, midiOutReset, midiOutShortMsg, midiOutUnprepareHeader,
};

use crate::error::{Error, check};

/// How long a SysEx send may take beyond what its bytes take on a MIDI 1.0
/// wire (320 microseconds each, rounded up to a millisecond per three).
const SLACK: Duration = Duration::from_secs(1);

/// An open output. Dropping it silences the port and closes it.
pub struct MidiOut {
    handle: HMIDIOUT,
}

impl std::fmt::Debug for MidiOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiOut").finish_non_exhaustive()
    }
}

// SAFETY: a winmm handle names a driver object and is not tied to the thread
// that opened it; `MidiOut` is used from one thread at a time (it is not
// `Sync`).
unsafe impl Send for MidiOut {}

impl MidiOut {
    /// Opens output port `index`.
    ///
    /// # Errors
    ///
    /// `midiOutOpen`'s: `MMSYSERR_ALLOCATED` when another application holds the
    /// port, `MMSYSERR_BADDEVICEID` when it is gone.
    pub fn open(index: u32) -> Result<Self, Error> {
        let mut handle: HMIDIOUT = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer; no callback is registered.
        let code = unsafe { midiOutOpen(&raw mut handle, index, 0, 0, CALLBACK_NULL) };
        check("midiOutOpen", code)?;
        Ok(Self { handle })
    }

    /// Sends one short message: the status byte, then its data bytes, low
    /// byte first.
    ///
    /// # Errors
    ///
    /// `midiOutShortMsg`'s.
    pub fn short(&mut self, message: u32) -> Result<(), Error> {
        // SAFETY: `handle` is the open output.
        let code = unsafe { midiOutShortMsg(self.handle, message) };
        check("midiOutShortMsg", code)
    }

    /// Sends `message`, a whole System Exclusive message from `0xF0` to
    /// `0xF7`, and returns when the driver has taken all of it.
    ///
    /// # Errors
    ///
    /// The failing call. Two failures leave the header and the bytes
    /// allocated for good, because the driver may still read them: the send
    /// outlasting its timeout ([`Error::NotReturned`]), and an unprepare
    /// that returns any code but `MIDIERR_STILLPLAYING` or success.
    pub fn long(&mut self, message: &[u8]) -> Result<(), Error> {
        let length = u32::try_from(message.len()).map_err(|source| Error::TooLong {
            len: message.len(),
            source,
        })?;
        // The bytes and the header are raw allocations while the driver may own
        // them; they are rebuilt as boxes, and freed, only once it is done.
        let data: *mut [u8] = Box::into_raw(message.to_vec().into_boxed_slice());
        let header: *mut MIDIHDR = Box::into_raw(Box::new(MIDIHDR {
            lpData: data.cast::<u8>(),
            dwBufferLength: length,
            ..MIDIHDR::default()
        }));
        let size = size_of::<MIDIHDR>() as u32;
        // SAFETY: `header` is a live header whose `lpData` is a live buffer of
        // `dwBufferLength` bytes, both allocated above and not freed before the
        // driver is done with them.
        let prepared = check("midiOutPrepareHeader", unsafe {
            midiOutPrepareHeader(self.handle, header, size)
        });
        let sent = prepared.and_then(|()| {
            // SAFETY: the header is prepared and stays allocated until it is
            // unprepared below.
            check("midiOutLongMsg", unsafe {
                midiOutLongMsg(self.handle, header, size)
            })
        });
        // Unpreparing a header the driver is still playing fails with
        // `MIDIERR_STILLPLAYING`, so the call that releases it is also the
        // wait for it.
        let bound = Duration::from_millis(message.len() as u64 / 3 + 1) + SLACK;
        let deadline = Instant::now() + bound;
        let released = loop {
            // SAFETY: `header` is the header prepared above (or one whose
            // preparation failed, which unprepare accepts).
            let code = unsafe { midiOutUnprepareHeader(self.handle, header, size) };
            if code != MIDIERR_STILLPLAYING {
                break code;
            }
            if Instant::now() >= deadline {
                // The driver may still read the header and the bytes: they
                // are never freed.
                return Err(Error::NotReturned {
                    operation: "midiOutLongMsg",
                });
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        if released != 0 {
            // The driver did not say it was done with the header (a failure
            // other than `MIDIERR_STILLPLAYING`): it may still read it and the
            // bytes, so neither is freed.
            sent?;
            return check("midiOutUnprepareHeader", released);
        }
        // SAFETY: the unprepare returned 0, so the driver holds neither the
        // header nor the bytes; nothing else points at either allocation, and
        // each is freed once, by the pointer that made it.
        unsafe {
            drop(Box::from_raw(header));
            drop(Box::from_raw(data));
        }
        sent
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        // SAFETY: `handle` is the open output, reset (all notes off) and
        // closed once.
        unsafe {
            midiOutReset(self.handle);
            midiOutClose(self.handle);
        }
    }
}
