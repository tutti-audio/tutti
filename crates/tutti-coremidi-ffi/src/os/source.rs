//! A **native-UMP** virtual MIDI source: an endpoint created with
//! `MIDISourceCreateWithProtocol` and fed raw UMP words through
//! `MIDIReceivedEventList`, so MIDI 2.0 messages (per-note controllers, JR
//! Timestamps) reach the wire.

use super::client::SharedClient;
use super::error::{Error, Operation, OsStatus};
use super::event_list::{EventList, capacity};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    MIDIEndpointDispose, MIDIEndpointRef, MIDIProtocolID, MIDIReceivedEventList,
    MIDISourceCreateWithProtocol, MIDITimeStamp, kMIDIProtocol_1_0, kMIDIProtocol_2_0,
};
use tutti_ump::{Protocol, Ump};

/// CoreMIDI's id (`MIDIProtocolID`) for the protocol a virtual endpoint
/// carries: `kMIDIProtocol_1_0` for MIDI 1.0 channel voice messages in UMP
/// packets, `kMIDIProtocol_2_0` for MIDI 2.0 Protocol messages.
///
/// # Errors
///
/// [`Error::ReservedProtocol`] for a reserved protocol, which has no id.
pub(crate) fn protocol_id(protocol: Protocol) -> Result<MIDIProtocolID, Error> {
    let id = match protocol {
        Protocol::Midi1 => kMIDIProtocol_1_0,
        Protocol::Midi2 => kMIDIProtocol_2_0,
        _ => return Err(Error::ReservedProtocol),
    };
    Ok(id as MIDIProtocolID)
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
    protocol: MIDIProtocolID,
}

impl UmpSource {
    /// Creates a virtual source named `name` carrying `protocol` on `client`,
    /// visible to other apps as a UMP-capable MIDI source.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, and
    /// [`Error::Call`] when CoreMIDI refuses the endpoint.
    pub fn new(client: SharedClient, name: &str, protocol: Protocol) -> Result<Self, Error> {
        let protocol = protocol_id(protocol)?;
        let source_name = CFString::new(name);
        let client = client.get();

        let mut source: MIDIEndpointRef = 0;
        // SAFETY: `client` is the live process-wide client; `source_name` outlives
        // the call and `source` is a valid out-pointer.
        let status = unsafe {
            MIDISourceCreateWithProtocol(
                client,
                source_name.as_concrete_TypeRef(),
                protocol,
                &mut source,
            )
        };
        OsStatus::check(status).map_err(|status| Error::call(Operation::CreateSource, status))?;
        Ok(Self { source, protocol })
    }

    /// Publishes one UMP message to the endpoint's subscribers, delivered now.
    ///
    /// The message's words are its type's length (`Ump::words`: a JR Timestamp
    /// is 1 word, a note 1 to 2, SysEx8 4). CoreMIDI copies them, so no lifetime
    /// escapes this call.
    ///
    /// # Errors
    ///
    /// [`Error::Call`] when `MIDIReceivedEventList` returns a non-zero status, and
    /// [`Error::EventListFull`] when the message does not fit an event list (a
    /// message is at most 4 words, so it always does).
    pub fn send(&self, ump: &Ump) -> Result<(), Error> {
        publish(self.source, self.protocol, ump)
    }
}

/// Publishes one UMP message on the source `endpoint` in `protocol`, delivered now.
///
/// `endpoint` is a live source: the caller keeps it alive across the call, and
/// disposes it only when no call is running. The list CoreMIDI reads is built here and copied by the call.
pub(crate) fn publish(
    endpoint: MIDIEndpointRef,
    protocol: MIDIProtocolID,
    ump: &Ump,
) -> Result<(), Error> {
    // A single message is at most 4 words, one packet.
    let mut buffer = [0u32; capacity(1, 4)];
    let list = EventList::new(&mut buffer, protocol, [(MIDI_TIMESTAMP_NOW, ump.words())])?;
    // SAFETY: `endpoint` is a live source (the caller's contract), and `list`
    // points to an initialised event list that outlives the call, which CoreMIDI
    // copies.
    let status = unsafe { MIDIReceivedEventList(endpoint, list.as_ptr()) };

    OsStatus::check(status).map_err(|status| Error::call(Operation::ReceivedEventList, status))
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
    use crate::os::client::test_client;
    use tutti_ump::read_protocol;

    /// A reserved protocol has no CoreMIDI id: no endpoint is created in it, and
    /// the two defined ones map to the two ids.
    #[test]
    fn a_reserved_protocol_is_refused() {
        assert_eq!(protocol_id(read_protocol(0)), Err(Error::ReservedProtocol));
        assert_eq!(
            protocol_id(Protocol::Midi1),
            Ok(kMIDIProtocol_1_0 as MIDIProtocolID)
        );
        assert_eq!(
            protocol_id(Protocol::Midi2),
            Ok(kMIDIProtocol_2_0 as MIDIProtocolID)
        );
        assert!(matches!(
            UmpSource::new(test_client(), "tutti-ffi-test", read_protocol(3)),
            Err(Error::ReservedProtocol)
        ));
    }

    /// A message of every length reaches the endpoint: a 1-word JR Timestamp, a
    /// 2-word MIDI 2.0 note and a 4-word SysEx8 each fit the event list.
    #[test]
    fn a_message_of_each_length_is_sent() {
        let source =
            UmpSource::new(test_client(), "tutti-ffi-test", Protocol::Midi2).expect("CoreMIDI");
        assert_eq!(source.send(&Ump([0x0020_1234, 0, 0, 0])), Ok(()));
        assert_eq!(source.send(&Ump([0x4090_3C00, 0x8000_0000, 0, 0])), Ok(()));
        assert_eq!(source.send(&Ump([0x5001_0000, 1, 2, 3])), Ok(()));
    }
}
