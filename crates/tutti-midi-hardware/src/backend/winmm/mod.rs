//! The Windows backend: Windows Multimedia (winmm), the MIDI 1.0 API every
//! Windows has.
//!
//! winmm lists ports by position, names them by a product string, and moves
//! MIDI 1.0 bytes: no UMP, no Endpoint discovery, no virtual ports, and no
//! notification when a device comes or goes. This file and its siblings are
//! the calls into it; what the backend decides (identity, SysEx, timestamps)
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
//!   interface path or the product name and ids, never the friendly name. Ports
//!   that share a shown name (a multi-port device) are shown by their driver
//!   names where those differ, else numbered (`distinct_names`); an id never
//!   reads a name. A
//!   listed port owns its list position, and the open checks it again once the
//!   port is open, failing when the device there is not the one listed.
//! - **Input** is a winmm callback on the driver's thread. A short message is
//!   one MIDI 1.0 UMP (type 2 or type 1) and System Exclusive arrives as
//!   SysEx7 packets; the input node translates the first as it does for any
//!   MIDI 1.0 port. Each carries its time since the port started, placed on
//!   the `Instant` clock.
//! - **Output** is a future on a dedicated thread of the executor: it holds each event until its time
//!   (a high-resolution waitable timer; one already due goes at once) and
//!   sends them in order, a SysEx message whole, waiting for the driver to
//!   take it.
//! - **The device watch** lists again once a second.
//! - **Virtual ports** do not exist in winmm: they are
//!   [`MidiHardwareError::Unsupported`].

mod input;
mod output;

use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, NotFound, Result};
use crate::start::Started;
use tutti_midi_device::{EndpointId, OsDevice, WinMmDevice};
use tutti_platform::Executor;

use super::legacy_port::poll;
use super::legacy_port::ports::{PortInfo, Side, endpoints, locate, slot_of};
use tutti_winmm_ffi::{Input, InputPort, Output, OutputPort, Port, PortKind, ports};

/// The Windows [`MidiEndpoints`] implementation. Holds nothing: winmm is
/// asked afresh at each call.
#[derive(Debug)]
pub(super) struct WinMmEndpoints {
    executor: Executor,
}

impl WinMmEndpoints {
    /// The winmm backend, whose device poll is a task on `executor`.
    pub(super) fn new(executor: &Executor) -> Self {
        Self {
            executor: executor.clone(),
        }
    }
}

/// What names a port's device: the interface path where the driver gives one,
/// else its product name and ids. Never the friendly name.
fn device_of<D: PortKind>(port: &Port<D>) -> WinMmDevice {
    match port.interface() {
        Some(path) => WinMmDevice::Interface(path.to_owned()),
        None => {
            let caps = port.caps();
            WinMmDevice::Named {
                name: caps.name.clone(),
                manufacturer: caps.manufacturer,
                product: caps.product,
            }
        }
    }
}

/// Every port of direction `D` now, with what names its device: what a
/// resolve needs, and no display name.
fn identified<D: PortKind>() -> Vec<(Port<D>, WinMmDevice)> {
    ports::<D>()
        .into_iter()
        .map(|port| {
            let device = device_of(&port);
            (port, device)
        })
        .collect()
}

/// Every port of direction `D` now, as a listing shows it: the display name
/// (the friendly name where the device has one, read here and nowhere else;
/// `endpoints` makes shared names distinct by the driver names or numbers) and
/// what names the device.
fn shown<D: PortKind>() -> Vec<PortInfo> {
    ports::<D>()
        .iter()
        .map(|port| PortInfo {
            name: port.name().to_owned(),
            driver_name: port.caps().name.clone(),
            device: device_of(port),
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
        Side::Input => endpoints(side, shown::<Input>()),
        Side::Output => endpoints(side, shown::<Output>()),
    }
}

/// The input port `id` names now, wherever the list has moved it;
/// [`NotFound::Endpoint`] when the id is not an input's, or the device is gone.
fn resolve_input(id: EndpointId) -> Result<InputPort> {
    let slot = slot_of(id);
    if slot.side != Side::Input {
        return Err(NotFound::Endpoint(id).into());
    }
    Ok(locate(slot, identified::<Input>()).ok_or(NotFound::Endpoint(id))?)
}

/// The output port `id` names now; see [`resolve_input`].
fn resolve_output(id: EndpointId) -> Result<OutputPort> {
    let slot = slot_of(id);
    if slot.side != Side::Output {
        return Err(NotFound::Endpoint(id).into());
    }
    Ok(locate(slot, identified::<Output>()).ok_or(NotFound::Endpoint(id))?)
}

/// `error` as the open of `id` reports it: winmm's `MMSYSERR_ALLOCATED` (another
/// application holds the port) is [`MidiHardwareError::InUse`], and any other
/// failure stays the OS's own.
fn in_use_if_allocated(id: EndpointId, error: MidiHardwareError) -> MidiHardwareError {
    match error {
        MidiHardwareError::Os(tutti_winmm_ffi::Error::Call {
            source: tutti_winmm_ffi::MmResult::Allocated,
            ..
        }) => MidiHardwareError::InUse(id),
        other => other,
    }
}

impl MidiEndpoints for WinMmEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(poll::start(
            &self.executor,
            sink,
            devices,
            poll::INTERVAL,
        )?))
    }

    // winmm's device count and capability calls have no failure to report: a
    // device that left is not in the list.
    fn inputs(&self) -> Result<Vec<OsEndpoint>> {
        Ok(list(Side::Input))
    }

    fn outputs(&self) -> Result<Vec<OsEndpoint>> {
        Ok(list(Side::Output))
    }

    fn open_input(
        &self,
        _started: Started,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let port = resolve_input(id)?;
        Ok(Box::new(
            input::open(&self.executor, &port, producer).map_err(|e| in_use_if_allocated(id, e))?,
        ))
    }

    fn open_output(&self, _started: Started, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let port = resolve_output(id)?;
        Ok(Box::new(
            output::open(&self.executor, &port).map_err(|e| in_use_if_allocated(id, e))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_winmm_ffi::{Error, MmOperation, MmResult};

    fn open_failed(result: MmResult) -> MidiHardwareError {
        MidiHardwareError::Os(Error::Call {
            operation: MmOperation::MidiInOpen,
            source: result,
        })
    }

    /// **A port another application holds is `InUse`**, and any other winmm
    /// failure stays the OS's own.
    #[test]
    fn mmsyserr_allocated_is_in_use_and_other_results_are_not() {
        let id = EndpointId::from_raw(7);
        assert!(matches!(
            in_use_if_allocated(id, open_failed(MmResult::Allocated)),
            MidiHardwareError::InUse(held) if held == id
        ));
        assert!(matches!(
            in_use_if_allocated(id, open_failed(MmResult::BadDeviceId)),
            MidiHardwareError::Os(Error::Call {
                source: MmResult::BadDeviceId,
                ..
            })
        ));
    }
}
