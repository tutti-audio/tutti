//! An open winmm output.

use std::time::Duration;

use tutti_platform::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Media::Audio::{
    CALLBACK_EVENT, HMIDIOUT, MIDIERR_STILLPLAYING, MIDIHDR, midiOutClose, midiOutLongMsg,
    midiOutOpen, midiOutPrepareHeader, midiOutReset, midiOutShortMsg, midiOutUnprepareHeader,
};

use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use tutti_ump::{SysexMessage, Ump};

use super::OutputPort;
use super::error::{Error, MmOperation, OsOperation, check, last_os};
use crate::ump::pack;

/// How long a SysEx send may take beyond what its bytes take on a MIDI 1.0
/// wire (320 microseconds each, rounded up to a millisecond per three).
const SLACK: Duration = Duration::from_secs(1);

/// An open output. Dropping it silences the port and closes it, except after a
/// SysEx send the driver did not give back (see [`long`](Self::long)): the port
/// is then silenced and left open.
pub struct MidiOut {
    handle: HMIDIOUT,
    /// The auto-reset event the driver sets when it finishes a buffer
    /// (`CALLBACK_EVENT`): what a SysEx send waits on. Closed after the port,
    /// and never when the port is left open, because the driver may still set it.
    done: HANDLE,
    /// Set when a SysEx send left its header and bytes with the driver for
    /// good: the port is then reset on drop but never closed.
    leaked: bool,
}

impl std::fmt::Debug for MidiOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiOut").finish_non_exhaustive()
    }
}

// SAFETY: a winmm handle names a driver object, and the event is a kernel
// object; neither is tied to the thread that made it. `MidiOut` is used from one
// thread at a time (it is not `Sync`).
unsafe impl Send for MidiOut {}

/// An output opened but not yet known to be the listed device: dropping it
/// closes the port and sends it nothing.
struct Unconfirmed {
    handle: HMIDIOUT,
    done: HANDLE,
}

impl Unconfirmed {
    /// The open output, once the device behind it is confirmed.
    fn confirm(self) -> MidiOut {
        let out = MidiOut {
            handle: self.handle,
            done: self.done,
            leaked: false,
        };
        std::mem::forget(self);
        out
    }
}

impl Drop for Unconfirmed {
    fn drop(&mut self) {
        // SAFETY: the handle is the output just opened, closed once. The event
        // is closed only after a successful close, once, when the driver has no
        // port to set it for; after a failed close the port may still be open and
        // the event is left open.
        unsafe {
            if midiOutClose(self.handle) == 0 {
                CloseHandle(self.done);
            }
        }
    }
}

impl MidiOut {
    /// Opens `port`.
    ///
    /// winmm refuses a handle in the device interface query, so the open
    /// cannot ask the open handle which device it is. It reads the port's
    /// position before `midiOutOpen`, and opens only if the device there is the
    /// one listed, then reads it again once open and closes the port (with no
    /// reset: a port not confirmed is never sent anything) unless it still is.
    /// Only a shift between those two reads that puts the listed device back
    /// at the position goes unseen.
    ///
    /// # Errors
    ///
    /// [`Error::PortMoved`] when the device at the position is not the one
    /// listed (checked before and after the open); otherwise `midiOutOpen`'s: [`MmResult::Allocated`](crate::MmResult::Allocated) when
    /// another application holds the port,
    /// [`MmResult::BadDeviceId`](crate::MmResult::BadDeviceId) when it is gone.
    pub fn open(port: &OutputPort) -> Result<Self, Error> {
        port.verify()?;
        // SAFETY: null security attributes and name; an auto-reset event that
        // starts unsignalled.
        let done = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if done.is_null() {
            return Err(last_os(OsOperation::CreateEvent));
        }
        let mut handle: HMIDIOUT = std::ptr::null_mut();
        // SAFETY: `handle` is a live out-pointer; `done` is an open event, which
        // `CALLBACK_EVENT` takes as the callback and which outlives the port (it
        // is closed after it, or never).
        let code = unsafe {
            midiOutOpen(
                &raw mut handle,
                port.index(),
                done as usize,
                0,
                CALLBACK_EVENT,
            )
        };
        if let Err(e) = check(MmOperation::MidiOutOpen, code) {
            // SAFETY: the open failed, so no port holds `done`; closed once.
            unsafe { CloseHandle(done) };
            return Err(e);
        }
        // Until confirmed the port is an `Unconfirmed`, which only closes.
        let unconfirmed = Unconfirmed { handle, done };
        port.verify()?;
        Ok(unconfirmed.confirm())
    }

    /// Sends `ump` as one short message: a MIDI 1.0 channel voice (type 2) or
    /// system common or real time (type 1) message, on any Group, which a byte
    /// stream does not carry.
    ///
    /// # Errors
    ///
    /// [`Error::NoMidi1Form`] for any other message type (a MIDI 2.0 channel
    /// voice message is one), with nothing sent; otherwise `midiOutShortMsg`'s.
    pub fn send(&mut self, ump: &Ump) -> Result<(), Error> {
        let message = pack(ump).ok_or(Error::NoMidi1Form {
            message_type: ump.message_type(),
        })?;
        // SAFETY: `handle` is the open output.
        let code = unsafe { midiOutShortMsg(self.handle, message) };
        check(MmOperation::MidiOutShortMsg, code)
    }

    /// Sends `message`, a whole System Exclusive message, and returns when the
    /// driver has taken all of it.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] for a message past `u32::MAX` bytes, with nothing
    /// sent; otherwise the failing call.
    ///
    /// Two failures leave the header and the bytes
    /// allocated for good, because the driver may still read them: the send
    /// outlasting its timeout ([`Error::NotReturned`]), and an unprepare
    /// that returns any code but `MIDIERR_STILLPLAYING` or success. The port
    /// is then never closed: dropping it only resets it.
    pub fn long(&mut self, message: SysexMessage<'_>) -> Result<(), Error> {
        let message = message.bytes();
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
        let prepared = check(MmOperation::MidiOutPrepareHeader, unsafe {
            midiOutPrepareHeader(self.handle, header, size)
        });
        let sent = prepared.and_then(|()| {
            // SAFETY: the header is prepared and stays allocated until it is
            // unprepared below.
            check(MmOperation::MidiOutLongMsg, unsafe {
                midiOutLongMsg(self.handle, header, size)
            })
        });
        // Unpreparing a header the driver is still playing fails with
        // `MIDIERR_STILLPLAYING`, so the call that releases it is the check, and
        // the driver's event ends the wait between checks: it is set as each
        // buffer is finished, and a set that predates this send only costs one
        // more check. This runs on the output's sender thread, which holds
        // everything after this message until it is out.
        let bound = Duration::from_millis(message.len() as u64 / 3 + 1) + SLACK;
        let deadline = Instant::now() + bound;
        let released = loop {
            // SAFETY: `header` is the header prepared above (or one whose
            // preparation failed, which unprepare accepts).
            let code = unsafe { midiOutUnprepareHeader(self.handle, header, size) };
            if code != MIDIERR_STILLPLAYING {
                break code;
            }
            let left = deadline.checked_duration_since(Instant::now());
            let waited = left.map(|left| self.wait_done(left));
            match waited {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    // The wait failed, so the driver's state is unknown: the
                    // header and the bytes are never freed.
                    self.leaked = true;
                    return Err(e);
                }
                None => {
                    // The driver may still read the header and the bytes: they
                    // are never freed.
                    self.leaked = true;
                    return Err(Error::NotReturned {
                        operation: MmOperation::MidiOutLongMsg,
                    });
                }
            }
        };
        if released != 0 {
            // The driver did not say it was done with the header (a failure
            // other than `MIDIERR_STILLPLAYING`): it may still read it and the
            // bytes, so neither is freed.
            self.leaked = true;
            sent?;
            return check(MmOperation::MidiOutUnprepareHeader, released);
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

impl MidiOut {
    /// Returns when the driver has set the event or `timeout` has passed.
    fn wait_done(&self, timeout: Duration) -> Result<(), Error> {
        // Rounded up, so the last wait does not end before the deadline.
        let millis = u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX);
        // SAFETY: `done` is the open event, which this output keeps open for the
        // call.
        let result = unsafe { WaitForSingleObject(self.done, millis) };
        match result {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
            // An event is never abandoned; the code has no `GetLastError`.
            WAIT_ABANDONED => Err(Error::Os {
                operation: OsOperation::WaitForSingleObject,
                source: std::io::Error::other("the wait was abandoned"),
            }),
            _ => Err(last_os(OsOperation::WaitForSingleObject)),
        }
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        // SAFETY: `handle` is the open output, reset (all notes off).
        unsafe { midiOutReset(self.handle) };
        if self.leaked {
            // The driver may still hold a header that a send leaked; the port
            // stays open so that header is never closed under it.
            return;
        }
        // SAFETY: `handle` is the open output, closed once. The event is closed
        // only after a successful close, once, when the driver has no port to set
        // it for; after a failed close the port may still be open and the event
        // is left open.
        unsafe {
            if midiOutClose(self.handle) == 0 {
                CloseHandle(self.done);
            }
        }
    }
}
