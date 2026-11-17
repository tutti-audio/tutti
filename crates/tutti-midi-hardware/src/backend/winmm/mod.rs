//! The Windows backend: Windows Multimedia (winmm), the MIDI 1.0 API every
//! Windows has.
//!
//! winmm lists ports by position, names them by a product string, and moves
//! MIDI 1.0 bytes: no UMP, no Endpoint discovery, no virtual ports, and no
//! notification when a device comes or goes. This file and its siblings are
//! the calls into it; what the backend decides (identity, packing, timestamps)
//! is [`legacy_port`](super::legacy_port), which runs on every platform.
//!
//! - **Enumeration** names each port by the device interface path the driver
//!   reports (`DRV_QUERYDEVICEINTERFACE`), which survives a replug, and by its
//!   product name and ids when it reports none. An
//!   [`EndpointId`](tutti_midi_device::EndpointId) carries the direction, a
//!   digest of that name and a count among ports with the same digest, never
//!   the list position (it shifts when an earlier device is unplugged):
//!   opening finds the device wherever the list has put it, and fails when it
//!   is gone instead of opening the device that took its place. The name shown is
//!   the device's friendly name (from its property store) where it has one and
//!   winmm's product name (31 characters at most) otherwise; identity is the
//!   interface path or the product name and ids, never the friendly name. A
//!   listed port owns its list position, and the open checks it again once the
//!   port is open, failing when the device there is not the one listed.
//! - **Input** is a winmm callback on the driver's thread. A short message is
//!   one MIDI 1.0 UMP (type 2 or type 1) and System Exclusive arrives as
//!   SysEx7 packets; the ingest node widens the first as it does for any
//!   MIDI 1.0 port. Each carries winmm's millisecond stamp since the port
//!   started, placed on the `Instant` clock.
//! - **Output** is a thread of its own: it holds each event until its time
//!   (a high-resolution waitable timer; one already due goes at once) and
//!   sends them in order, a SysEx message whole, waiting for the driver to
//!   take it.
//! - **The device watch** lists again once a second.
//! - **Virtual ports** do not exist in winmm: they are
//!   [`Error::Unsupported`](crate::Error::Unsupported).

mod input;
mod output;

use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{NotFound, Result};
use tutti_midi_device::{EndpointId, OsDevice, WinMmDevice};

use super::legacy_port::poll;
use super::legacy_port::ports::{PortInfo, Side, endpoints, locate, slot_of};
use tutti_winmm_ffi::{Input, InputPort, Output, OutputPort, Port, PortKind, ports};

/// The Windows [`MidiEndpoints`] implementation. Holds nothing: winmm is
/// asked afresh at each call.
#[derive(Debug, Default)]
pub struct WinMmEndpoints;

impl WinMmEndpoints {
    /// The winmm backend.
    pub fn new() -> Self {
        Self
    }
}

/// What names a port: the interface path where the driver gives one, else its
/// product name and ids; and the name shown, which is the friendly name where
/// the device has one.
fn info<D: PortKind>(port: &Port<D>) -> PortInfo {
    let caps = port.caps();
    let device = match port.interface() {
        Some(path) => WinMmDevice::Interface(path.to_owned()),
        None => WinMmDevice::Named {
            name: caps.name.clone(),
            manufacturer: caps.manufacturer,
            product: caps.product,
        },
    };
    PortInfo {
        name: port.name().to_owned(),
        device,
    }
}

/// Every port of direction `D` now, with what names it.
fn described<D: PortKind>() -> Vec<(Port<D>, PortInfo)> {
    ports::<D>()
        .into_iter()
        .map(|port| {
            let described = info(&port);
            (port, described)
        })
        .collect()
}

/// Every device now.
fn devices() -> Vec<OsDevice> {
    os_devices(&list(Side::Input), &list(Side::Output))
}

/// The endpoints of `side` now.
fn list(side: Side) -> Vec<OsEndpoint> {
    match side {
        Side::Input => endpoints(side, described::<Input>().into_iter().map(|(_, i)| i)),
        Side::Output => endpoints(side, described::<Output>().into_iter().map(|(_, i)| i)),
    }
}

/// The input port `id` names now, wherever the list has moved it;
/// [`NotFound::Endpoint`] when the id is not an input's, or the device is gone.
fn resolve_input(id: EndpointId) -> Result<InputPort> {
    let slot = slot_of(id);
    if slot.side != Side::Input {
        return Err(NotFound::Endpoint(id).into());
    }
    Ok(locate(slot, described::<Input>()).ok_or(NotFound::Endpoint(id))?)
}

/// The output port `id` names now; see [`resolve_input`].
fn resolve_output(id: EndpointId) -> Result<OutputPort> {
    let slot = slot_of(id);
    if slot.side != Side::Output {
        return Err(NotFound::Endpoint(id).into());
    }
    Ok(locate(slot, described::<Output>()).ok_or(NotFound::Endpoint(id))?)
}

impl MidiEndpoints for WinMmEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(poll::start(sink, devices, poll::INTERVAL)?))
    }

    fn inputs(&self) -> Vec<OsEndpoint> {
        list(Side::Input)
    }

    fn outputs(&self) -> Vec<OsEndpoint> {
        list(Side::Output)
    }

    fn open_input(
        &self,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let port = resolve_input(id)?;
        Ok(Box::new(input::open(&port, producer)?))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let port = resolve_output(id)?;
        Ok(Box::new(output::open(&port)?))
    }
}
