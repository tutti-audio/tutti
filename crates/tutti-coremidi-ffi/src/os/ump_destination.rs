//! A **native-UMP** virtual MIDI destination, the inbound counterpart to
//! [`UmpSource`](crate::UmpSource).
//!
//! A virtual destination has no connection, so CoreMIDI passes its receive block a
//! null reference constant; [`UmpDestination`] creates the endpoint with
//! `MIDIDestinationCreateWithProtocol` and a block that never reads it, and hands
//! the caller each UMP message of each packet with its time. The block is shared
//! with [`InputPort`](crate::InputPort).

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{MIDIDestinationCreateWithProtocol, MIDIEndpointDispose, MIDIEndpointRef};
use std::fmt;
use std::time::Duration;

use super::client::SharedClient;
use super::error::{Error, Operation, OsStatus};
use super::receive::Receiver;
use super::source::protocol_id;
use tutti_ump::{Protocol, Ump};

/// A CoreMIDI virtual destination that speaks the **MIDI 2.0 (UMP) protocol**:
/// other apps see a UMP-capable destination, and every UMP message sent to it is
/// handed to the callback given at creation, on CoreMIDI's delivery thread, with
/// its time. A packet carries several messages back to back, each one's length
/// in its message type; the callback gets one call per message, in order, with
/// its packet's time. The time is the Mach host clock's reading (the one
/// [`host_ticks`](crate::host_ticks) reads) as the duration since that clock's
/// origin, or `None` when the sender gave no time (a host time of 0: delivery
/// now). A packet that ends inside a message drops that tail and counts it
/// ([`truncated`](Self::truncated)). Owns its endpoint, which is disposed on
/// drop.
///
/// # A panic in the callback
///
/// The receive block's invoke function is `extern "C-unwind"`, so a panic
/// leaving it would unwind through CoreMIDI's frames. The callback runs under
/// `catch_unwind` instead: a panic drops the callback, so every later packet
/// delivers nothing, and [`faults`](Self::faults) counts it. The crate logs
/// nothing, but a caught panic still runs the process's panic hook on CoreMIDI's
/// thread (the default one prints to stderr, which allocates and locks): an app
/// that cares installs a quiet hook, which a library never does.
///
/// # Drop
///
/// `MIDIServices.h` does not say whether a call already running on CoreMIDI's
/// thread has returned when `MIDIEndpointDispose` does, nor whether one can
/// start after it. The drop therefore disposes the endpoint, then takes the
/// callback out of the state it shares with the block under the callback's lock:
/// it waits for a call in progress, and afterwards the callback is neither
/// running nor callable, and is dropped on the dropping thread. CoreMIDI's own
/// copy of the block may outlive the drop and only finds the state empty.
///
/// Dropping a destination from inside its own callback deadlocks, since the
pub struct UmpDestination {
    endpoint: MIDIEndpointRef,
    // Dropped after `endpoint` is disposed (see `Drop`).
    receiver: Receiver,
}

impl fmt::Debug for UmpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UmpDestination")
            .field("endpoint", &self.endpoint)
            .field("faults", &self.faults())
            .finish_non_exhaustive()
    }
}

impl UmpDestination {
    /// Publishes a virtual destination named `name` carrying `protocol` on
    /// `client`; `on_ump` gets each message sent to it.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, and
    /// [`Error::Call`] when the endpoint cannot be created.
    pub fn new<F>(
        client: SharedClient,
        name: &str,
        protocol: Protocol,
        on_ump: F,
    ) -> Result<Self, Error>
    where
        F: FnMut(Option<Duration>, Ump) + Send + 'static,
    {
        let protocol = protocol_id(protocol)?;
        let client = client.get();
        let receiver = Receiver::ump(on_ump);
        let name = CFString::new(name);
        let mut endpoint: MIDIEndpointRef = 0;
        // SAFETY: `client` is a live client, `name` outlives the call, `endpoint`
        // is a valid out-pointer, and the block is a valid Objective-C block
        // CoreMIDI copies (and so retains) before the call returns; the block
        // never reads its reference-constant argument.
        let status = unsafe {
            MIDIDestinationCreateWithProtocol(
                client,
                name.as_concrete_TypeRef(),
                protocol,
                &mut endpoint,
                receiver.block(),
            )
        };
        OsStatus::check(status)
            .map_err(|status| Error::call(Operation::CreateDestination, status))?;
        Ok(Self { endpoint, receiver })
    }

    /// How many panics in the callback have been caught: 0, or 1 once the
    /// callback panicked and was dropped.
    #[must_use]
    pub fn faults(&self) -> u64 {
        self.receiver.faults()
    }

    /// How many packets ended inside a message, whose tail was dropped.
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.receiver.truncated()
    }
}

impl Drop for UmpDestination {
    fn drop(&mut self) {
        // SAFETY: the endpoint is ours, created above and disposed once. The
        // receiver, dropped after this, then empties the callback.
        unsafe {
            MIDIEndpointDispose(self.endpoint);
        }
    }
}
