//! A **native-UMP** output port: sends UMP messages to a destination, each send as
//! one event list, so CoreMIDI takes all its packets or none.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{MIDIOutputPortCreate, MIDIPortDispose, MIDIPortRef, MIDISendEventList};
use std::fmt;
use std::time::Duration;

use super::client::SharedClient;
use super::endpoint::Endpoint;
use super::error::{Error, Operation, OsStatus};
use super::event_list::{EventList, capacity};
use super::host_clock::Timebase;
use super::source::protocol_id;
use tutti_ump::{Protocol, Ump};

/// A CoreMIDI output port. Owns its port, which is disposed on drop.
///
/// `Send` and `Sync` without an `unsafe impl`: the port is an opaque `u32`
/// reference into CoreMIDI, whose objects are internally synchronised.
pub struct OutputPort {
    port: MIDIPortRef,
}

impl fmt::Debug for OutputPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputPort")
            .field("port", &self.port)
            .finish()
    }
}

impl OutputPort {
    /// Creates a port named `name` on `client`.
    ///
    /// # Errors
    ///
    /// [`Error::Call`] when CoreMIDI refuses the port.
    pub fn new(client: SharedClient, name: &str) -> Result<Self, Error> {
        let name = CFString::new(name);
        let mut port: MIDIPortRef = 0;
        // SAFETY: `client` is a live client, `name` outlives the call and `port` is
        // a valid out-pointer.
        let status =
            unsafe { MIDIOutputPortCreate(client.get(), name.as_concrete_TypeRef(), &mut port) };
        OsStatus::check(status)
            .map_err(|status| Error::call(Operation::CreateOutputPort, status))?;
        Ok(Self { port })
    }

    /// Sends `packets` to `destination` as one event list in `protocol`: each is a
    /// UMP message and the time CoreMIDI holds it until, as a duration since the
    /// host clock's origin (the clock [`host_ticks`](crate::host_ticks) reads), or
    /// `None` to deliver it now. CoreMIDI copies the words before the call returns.
    ///
    /// The list is built in a buffer this call allocates, so it is for a control
    /// thread.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, [`Error::EventListFull`]
    /// when a packet does not fit the list (a message is at most 4 words, so the
    /// buffer sized for `packets` holds them all), and [`Error::Call`] when
    /// CoreMIDI refuses the send (the destination went away).
    pub fn send(
        &self,
        destination: &Endpoint,
        protocol: Protocol,
        packets: &[(Option<Duration>, Ump)],
    ) -> Result<(), Error> {
        let protocol = protocol_id(protocol)?;
        let base = Timebase::get();
        let words: usize = packets.iter().map(|(_, ump)| ump.words().len()).sum();
        let mut buffer = vec![0u32; capacity(packets.len(), words)];
        let list = EventList::new(
            &mut buffer,
            protocol,
            packets.iter().map(|(time, ump)| {
                // The stamp 0 is "now".
                (time.map_or(0, |time| base.ticks(time)), ump.words())
            }),
        )?;
        // SAFETY: `self.port` is the live port `new` created, `destination.reference()`
        // an endpoint reference (a removed endpoint returns an error status), and
        // `list` points to an initialised event list that outlives the call.
        let status =
            unsafe { MIDISendEventList(self.port, destination.reference(), list.as_ptr()) };
        OsStatus::check(status).map_err(|status| Error::call(Operation::SendEventList, status))
    }
}

impl Drop for OutputPort {
    fn drop(&mut self) {
        // SAFETY: the port is ours, created in `new` and disposed once.
        unsafe {
            MIDIPortDispose(self.port);
        }
    }
}
