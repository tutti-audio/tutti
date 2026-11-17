//! A **native-UMP** virtual MIDI source: an endpoint created with
//! `MIDISourceCreateWithProtocol` and fed raw UMP words through
//! `MIDIReceivedEventList`, so MIDI 2.0 messages (per-note controllers, JR
//! Timestamps) reach the wire. The safe `coremidi` wrapper has only the MIDI 1.0
//! source, whose send path is legacy packet bytes.

use crate::client_retry::create_client;
use crate::error::{Error, Operation, OsStatus};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    ByteCount, MIDIClientCreate, MIDIClientRef, MIDIEndpointDispose, MIDIEndpointRef,
    MIDIEventList, MIDIEventListAdd, MIDIEventListInit, MIDIProtocolID, MIDIReceivedEventList,
    MIDISourceCreateWithProtocol, MIDITimeStamp, kMIDIProtocol_1_0, kMIDIProtocol_2_0,
};
use std::mem::MaybeUninit;
use std::sync::Mutex;

/// The UMP protocol a virtual endpoint carries (`MIDIProtocolID`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// MIDI 1.0 channel voice messages in UMP packets (`kMIDIProtocol_1_0`).
    Midi1,
    /// MIDI 2.0 Protocol messages (`kMIDIProtocol_2_0`).
    Midi2,
}

impl Protocol {
    /// CoreMIDI's id for the protocol.
    pub(crate) fn id(self) -> MIDIProtocolID {
        let id = match self {
            Self::Midi1 => kMIDIProtocol_1_0,
            Self::Midi2 => kMIDIProtocol_2_0,
        };
        id as MIDIProtocolID
    }
}

/// The process-wide raw client every endpoint here belongs to.
///
/// Never disposed: disposing the last client may make the MIDI server exit and
/// fail every later client creation. A failed creation is retried for a bounded
/// time and not cached.
pub(crate) fn shared_raw_client() -> Result<MIDIClientRef, OsStatus> {
    static CLIENT: Mutex<Option<MIDIClientRef>> = Mutex::new(None);
    let mut slot = CLIENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(client) = *slot {
        return Ok(client);
    }
    let name = CFString::new("tutti-ump-src");
    let client = create_client(|| {
        let mut client: MIDIClientRef = 0;
        // SAFETY: `name` outlives the call; `client` is a valid out-pointer. A NULL
        // notify proc with a NULL context is the documented "no notifications" form.
        let status = unsafe {
            MIDIClientCreate(
                name.as_concrete_TypeRef(),
                None,
                std::ptr::null_mut(),
                &mut client,
            )
        };
        OsStatus::check(status).map(|()| client)
    })?;
    *slot = Some(client);
    Ok(client)
}

/// Send this timestamp to mean "now" (CoreMIDI treats 0 as immediate delivery).
const MIDI_TIMESTAMP_NOW: MIDITimeStamp = 0;

/// A CoreMIDI virtual source that speaks the UMP protocol it was created with.
///
/// It carries UMP words end to end, so JR Timestamps and other MIDI-2-only
/// messages survive to the wire. Owns its endpoint, which is disposed on drop.
///
/// `Send` and `Sync` without an `unsafe impl`: the endpoint is an opaque `u32`
/// handle into CoreMIDI, whose objects are internally synchronised.
#[derive(Debug)]
#[must_use = "dropping it removes the virtual source"]
pub struct UmpSource {
    source: MIDIEndpointRef,
    protocol: Protocol,
}

impl UmpSource {
    /// Creates a virtual source named `name` carrying `protocol`, visible to
    /// other apps as a UMP-capable MIDI source.
    ///
    /// # Errors
    ///
    /// [`Error::Call`] when CoreMIDI refuses the client or the endpoint.
    pub fn new(name: &str, protocol: Protocol) -> Result<Self, Error> {
        let source_name = CFString::new(name);
        let client =
            shared_raw_client().map_err(|status| Error::call(Operation::CreateClient, status))?;

        let mut source: MIDIEndpointRef = 0;
        // SAFETY: `client` is the live process-wide client; `source_name` outlives
        // the call and `source` is a valid out-pointer.
        let status = unsafe {
            MIDISourceCreateWithProtocol(
                client,
                source_name.as_concrete_TypeRef(),
                protocol.id(),
                &mut source,
            )
        };
        OsStatus::check(status).map_err(|status| Error::call(Operation::CreateSource, status))?;
        Ok(Self { source, protocol })
    }

    /// Publishes one UMP message to the endpoint's subscribers.
    ///
    /// `words` is a complete UMP message; its length must match the message type
    /// (a JR Timestamp is 1 word, a note is 1 to 2, SysEx8 is 4). Empty input is
    /// a no-op. CoreMIDI copies the words, so no lifetime escapes this call.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] when the message will not fit the event list, and
    /// [`Error::Call`] when `MIDIReceivedEventList` returns a non-zero status.
    pub fn send(&self, words: &[u32]) -> Result<(), Error> {
        if words.is_empty() {
            return Ok(());
        }

        // A `MIDIEventList` holds one packet of up to 64 words inline, so a single
        // message of up to 4 words always fits.
        let mut list = MaybeUninit::<MIDIEventList>::uninit();
        let list_ptr = list.as_mut_ptr();
        let list_size = std::mem::size_of::<MIDIEventList>();

        // SAFETY: `list_ptr` addresses the whole `MIDIEventList`, and `Init`
        // is what initialises it; `assume_init_ref` below is reached only
        // after both calls have run. `list_size` is that allocation's real size,
        // so `Add` cannot write past it, and `words` outlives the call.
        let status = unsafe {
            let mut packet = MIDIEventListInit(list_ptr, self.protocol.id());
            packet = MIDIEventListAdd(
                list_ptr,
                list_size as ByteCount,
                packet,
                MIDI_TIMESTAMP_NOW,
                words.len() as ByteCount,
                words.as_ptr(),
            );
            if packet.is_null() {
                // The message didn't fit: nothing was queued.
                return Err(Error::TooLong);
            }
            MIDIReceivedEventList(self.source, list.assume_init_ref() as *const MIDIEventList)
        };

        OsStatus::check(status).map_err(|status| Error::call(Operation::ReceivedEventList, status))
    }
}

impl Drop for UmpSource {
    fn drop(&mut self) {
        // SAFETY: the endpoint was created in `new` and is disposed exactly once,
        // here. Its client is process-wide and outlives it.
        unsafe { MIDIEndpointDispose(self.source) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    /// The source crosses threads with no `unsafe impl`: if a field ever stopped
    /// being a plain handle, this stops compiling.
    #[test]
    fn a_source_is_send_and_sync() {
        assert_send_sync::<UmpSource>();
    }

    /// A message longer than the event list holds is refused with the packet
    /// error, not written past it; an empty one is a no-op.
    #[test]
    fn a_message_that_does_not_fit_is_refused() {
        let source = UmpSource::new("tutti-ffi-test", Protocol::Midi2).expect("CoreMIDI");
        assert_eq!(source.send(&[]), Ok(()));
        assert_eq!(source.send(&[0; 100_000]), Err(Error::TooLong));
        assert_eq!(source.send(&[0x2090_3C64]), Ok(()));
    }
}
