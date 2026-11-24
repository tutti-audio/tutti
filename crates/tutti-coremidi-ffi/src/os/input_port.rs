//! A **native-UMP** input port connected to a source: the receive side of a
//! device, the counterpart to [`UmpDestination`](crate::UmpDestination) for an
//! endpoint another process owns.
//!
//! The port is created with `MIDIInputPortCreateWithProtocol`, so CoreMIDI
//! delivers UMP in the protocol the port was created with (translating what the
//! source speaks into it), and hands each message to the caller with its time.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    MIDIInputPortCreateWithProtocol, MIDIPortConnectSource, MIDIPortDispose, MIDIPortRef,
};
use std::fmt;
use std::ptr;
use std::time::Duration;

use super::client::SharedClient;
use super::endpoint::{Endpoint, Source};
use super::error::{Error, Operation, OsStatus};
use super::receive::{Event, Receiver};
use super::source::protocol_id;
use tutti_ump::{Protocol, Ump};

/// A CoreMIDI input port connected to one source, in the UMP protocol it was
/// created with. Every UMP message the source sends is handed to the callback given
/// at creation, on CoreMIDI's delivery thread, as an [`UmpDestination`](crate::UmpDestination)
/// hands it: one call per message of each packet, in order, with its packet's time
/// since the host clock's origin, or `None` for a packet sent with no time. Each
/// delivered list first makes a state with `new_list`, which every message of the
/// list is handed, so what a list shares (a reading of a clock) is made once for it. A packet
/// that ends inside a message drops that tail and counts it
/// ([`truncated`](Self::truncated)). Owns its port, which is disposed on drop, and
/// that disconnects it.
///
/// A panic in the callback is caught as in [`UmpDestination`](crate::UmpDestination):
/// the callback is dropped and [`faults`](Self::faults) counts it. Dropping the port
/// waits for a call in progress, so it must not run inside the callback. The
/// port's block is never freed, so a call CoreMIDI still has in flight after the
/// drop finds no callback: each port keeps a few dozen bytes for the life of the
/// process.
pub struct InputPort {
    port: MIDIPortRef,
    // Dropped after `port` is disposed (see `Drop`).
    receiver: Receiver,
}

impl fmt::Debug for InputPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputPort")
            .field("port", &self.port)
            .field("faults", &self.faults())
            .finish_non_exhaustive()
    }
}

impl InputPort {
    /// Creates a port named `name` on `client` that receives UMP in `protocol` and
    /// connects it to `source`; `new_list` makes the state of each delivered list
    /// and `on_ump` gets it with each message of the list.
    ///
    /// # Errors
    ///
    /// [`Error::ReservedProtocol`] for a reserved `protocol`, and [`Error::Call`]
    /// when CoreMIDI refuses the port or the connection (the source went away).
    ///
    /// Only a source connects; a destination is another type:
    ///
    /// ```compile_fail,E0308
    /// use tutti_coremidi_ffi::{Destination, Endpoint, InputPort, SharedClient};
    /// use tutti_ump::Protocol;
    ///
    /// fn connect_to_a_destination(client: SharedClient, destination: &Endpoint<Destination>) {
    ///     let _ = InputPort::connect(
    ///         client,
    ///         "in",
    ///         Protocol::Midi2,
    ///         destination,
    ///         || (),
    ///         |(), _, _| {},
    ///     );
    /// }
    /// ```
    pub fn connect<S, N, F>(
        client: SharedClient,
        name: &str,
        protocol: Protocol,
        source: &Endpoint<Source>,
        mut new_list: N,
        mut on_ump: F,
    ) -> Result<Self, Error>
    where
        S: Send + 'static,
        N: FnMut() -> S + Send + 'static,
        F: FnMut(&mut S, Option<Duration>, Ump) + Send + 'static,
    {
        let protocol = protocol_id(protocol)?;
        let mut list: Option<S> = None;
        let receiver = Receiver::new(move |event| match event {
            Event::List => list = Some(new_list()),
            Event::Ump(time, ump) => {
                if let Some(list) = list.as_mut() {
                    on_ump(list, time, ump);
                }
            }
        });
        let name = CFString::new(name);
        let mut port: MIDIPortRef = 0;
        // SAFETY: `client` is a live client, `name` outlives the call, `port` is a
        // valid out-pointer, and the block is a valid Objective-C block CoreMIDI
        // copies (and so retains) before the call returns; the block never reads
        // its reference-constant argument.
        let status = unsafe {
            MIDIInputPortCreateWithProtocol(
                client.get(),
                name.as_concrete_TypeRef(),
                protocol,
                &mut port,
                receiver.block(),
            )
        };
        OsStatus::check(status)
            .map_err(|status| Error::call(Operation::CreateInputPort, status))?;
        // From here a failure drops the port, which disposes it.
        let port = Self { port, receiver };
        // SAFETY: `port.port` is the live port created above and `source.reference()`
        // an endpoint reference; a reference to a removed endpoint returns an
        // error status. The reference constant is null: the block never reads it.
        let status =
            unsafe { MIDIPortConnectSource(port.port, source.reference(), ptr::null_mut()) };
        OsStatus::check(status).map_err(|status| Error::call(Operation::ConnectSource, status))?;
        Ok(port)
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

impl Drop for InputPort {
    fn drop(&mut self) {
        // SAFETY: the port is ours, created in `connect` and disposed once, which
        // also disconnects it. The receiver, dropped after this, then empties the
        // callback.
        unsafe {
            MIDIPortDispose(self.port);
        }
    }
}
