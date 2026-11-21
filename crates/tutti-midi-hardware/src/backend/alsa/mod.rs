//! The Linux backend: ALSA's UMP sequencer.
//!
//! Present only when `build.rs` found alsa-lib ≥ 1.2.10 (`cfg(alsa_ump)`).
//!
//! # UMP over a sequencer that predates it
//!
//! `snd_seq_set_client_midi_version` turns an ordinary seq client into a **UMP**
//! client: `snd_seq_ump_event_input` / `_output_direct` then carry four-word UMP
//! messages instead of the legacy byte-oriented events.
//!
//! Paired with `snd_seq_set_client_ump_conversion(1)`, the kernel (≥ 6.5)
//! translates at the port boundary, so a UMP client can talk to legacy hardware
//! and a legacy app can talk to this one. That is what makes this backend useful
//! when almost no Linux machine has a native-UMP device. But **tutti converts
//! MIDI 1.0 itself**: an input on a source that can only speak
//! MIDI 1.0 (a legacy client, or an Endpoint offering no MIDI 2.0 Protocol) is a
//! MIDI 1.0 UMP client, so the kernel hands it type 2 packets and the input
//! node's `Midi2Translator` translates them (RPN and NRPN runs, Bank Select, velocity-0
//! Note On, the M2-115 scaling). An input on an Endpoint that offers MIDI 2.0 is
//! a MIDI 2.0 client, since the session negotiates that Protocol. An output is
//! always a MIDI 2.0 client; the wire translates for a MIDI 1.0 endpoint.
//!
//! # Threading
//!
//! `snd_seq_ump_event_input` blocks, so each open input runs a pump thread that
//! reads and pushes into the connection's [`InputProducerHandle`] — the same
//! ring CoreMIDI's callback feeds. The thread exits when its connection drops.
//!
//! One more thread watches the `System:Announce` port for devices that come and
//! go ([`watch`]); it exists while a `Devices` does. Both are dedicated threads of
//! the [`Executor`] the backend was made over.

mod client;
mod enumerate;
mod input;
mod output;
mod poll;
pub(super) mod virtual_endpoint;
mod watch;

use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::Result;
use crate::start::Started;
use std::sync::Mutex;
use tutti_alsa_ffi::{Address, Client, UmpVersion};
use tutti_midi_device::{EndpointId, OsDevice, PortDirection};
use tutti_platform::Executor;
use tutti_rt::LockExt;

/// The handle a port is listed under.
pub(super) fn handle_of(at: Address) -> EndpointId {
    EndpointId::from_raw(at.into())
}

/// The port a handle names; `None` for a handle no port here made.
pub(super) fn address_of(handle: EndpointId) -> Option<Address> {
    Address::try_from(handle.raw()).ok()
}

/// The Linux [`MidiEndpoints`] implementation.
///
/// Holds one client for enumerating, opened on first use and kept: a client
/// opened per call would announce its own start, port and exit each time, and
/// every watch on the machine would wake for it. Each connection owns the client
/// it pumps or sends through, and the watch enumerates on its own.
pub struct AlsaEndpoints {
    executor: Executor,
    enumerator: Mutex<Option<Client>>,
}

impl core::fmt::Debug for AlsaEndpoints {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlsaEndpoints").finish_non_exhaustive()
    }
}

impl AlsaEndpoints {
    /// The ALSA backend, whose pump and watch threads are `executor`'s. No client
    /// is open until one is needed.
    pub fn new(executor: &Executor) -> Self {
        Self {
            executor: executor.clone(),
            enumerator: Mutex::new(None),
        }
    }

    /// A fresh snapshot in `direction`.
    ///
    /// An unopenable sequencer is the platform error, tried again at the next
    /// call.
    fn list(&self, direction: PortDirection) -> Result<Vec<OsEndpoint>> {
        let mut held = self.enumerator.lock_recover();
        let seq = match held.take() {
            Some(seq) => seq,
            None => client::open("tutti-enum", UmpVersion::Midi2)?,
        };
        let listed = enumerate::endpoints(&seq, direction);
        *held = Some(seq);
        Ok(listed)
    }
}

/// Every device now, enumerated on `seq`; `None` when the enumeration was cut
/// short by an error.
fn devices(seq: &Client) -> Option<Vec<OsDevice>> {
    let (inputs, outputs) = enumerate::both(seq)?;
    Some(os_devices(&inputs, &outputs))
}

impl MidiEndpoints for AlsaEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        watch::start(&self.executor, sink, devices)
    }

    fn inputs(&self) -> Result<Vec<OsEndpoint>> {
        self.list(PortDirection::Input)
    }

    fn outputs(&self) -> Result<Vec<OsEndpoint>> {
        self.list(PortDirection::Output)
    }

    fn open_input(
        &self,
        _started: Started,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        input::open(&self.executor, id, producer)
    }

    fn open_output(&self, _started: Started, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        output::open(id)
    }
}

#[cfg(all(test, feature = "sequencer-tests"))]
mod tests;
