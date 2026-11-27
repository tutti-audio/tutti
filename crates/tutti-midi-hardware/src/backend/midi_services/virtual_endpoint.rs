//! A virtual endpoint on Windows MIDI Services: a virtual device the service lists as one
//! UMP Endpoint with the spec's name, identity, protocol and Function Blocks.
//!
//! This process holds the device's side of the endpoint. What a peer sends to the endpoint
//! reaches the handler, which delivers it into the input's producer as a device input does,
//! and what the app sends goes to the peers. The service lists each block under the number
//! the spec gave it, so [`block_numbers`](EndpointBackend::block_numbers) has nothing to
//! add. The endpoint is published on a session of its own, which closes with it.

use std::sync::{Arc, Weak};

use super::input::handler;
use super::{new_session, os};
use crate::InputFault;
use crate::InputProducerHandle;
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{
    Delivery, EndpointBackend, Live, SenderBackend, VirtualProtocol, VirtualSpec,
};
use tutti_rt::FaultLatch;
use tutti_ump::Ump;
use tutti_winmidi2_ffi::{Block, Error, Session, VirtualDevice, VirtualDeviceSpec};

/// The manufacturer the service keeps with the device.
const MANUFACTURER: &str = "tutti";

/// The description the service keeps with the device.
const DESCRIPTION: &str = "A virtual MIDI endpoint published by tutti";

/// A published virtual device. Dropping it removes the endpoint from the service.
pub(crate) struct MidiServicesVirtualEndpoint {
    /// Raised when the handler panics.
    died: Arc<FaultLatch<InputFault>>,
    live: Live,
    // Closing the connection removes the endpoint, so it goes before the session it was
    // made on; senders hold the device only weakly.
    device: Arc<VirtualDevice>,
    _session: Session,
}

impl Drop for MidiServicesVirtualEndpoint {
    fn drop(&mut self) {
        // After this no send is in flight or begins, so the device is dropped unshared.
        self.live.end();
    }
}

/// A send on a virtual device, which it does not keep alive.
struct DeviceSender {
    device: Weak<VirtualDevice>,
    live: Live,
}

impl SenderBackend for DeviceSender {
    /// # Errors
    ///
    /// [`MidiHardwareError::EndpointGone`] once the endpoint is dropped or the service
    /// closed its connection, and [`MidiHardwareError::Os`] for any other refusal. A full
    /// buffer is [`Delivery::Full`].
    fn send(&self, ump: &Ump) -> Result<Delivery> {
        let _live = self.live.hold()?;
        let device = self
            .device
            .upgrade()
            .ok_or(MidiHardwareError::EndpointGone)?;
        match device.send(ump) {
            Ok(()) => Ok(Delivery::Sent),
            Err(Error::BufferFull) => Ok(Delivery::Full),
            Err(Error::Closed) => Err(MidiHardwareError::EndpointGone),
            Err(other) => Err(os(other)),
        }
    }
}

impl EndpointBackend for MidiServicesVirtualEndpoint {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        Arc::new(DeviceSender {
            device: Arc::downgrade(&self.device),
            live: self.live.clone(),
        })
    }

    fn is_alive(&self) -> bool {
        self.died.cause().is_none()
    }
}

/// Publishes the virtual device `spec` describes, delivering what peers send into
/// `producer`. The service carries the protocol the spec asks for.
///
/// # Errors
///
/// The named cause of the API or the service not being there, or the platform's own error
/// from the session, the declaration or the device.
pub(crate) fn endpoint(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(MidiServicesVirtualEndpoint, VirtualProtocol)> {
    let session = new_session()?;
    let died = Arc::new(FaultLatch::<InputFault>::new());
    let blocks: Vec<Block> = spec
        .function_blocks()
        .iter()
        .map(|block| Block {
            info: block.info(),
            name: block.name().map(|name| name.as_str().to_owned()),
        })
        .collect();
    let info = spec.endpoint_info(spec.protocol);
    let device = session
        .publish(
            &VirtualDeviceSpec {
                name: spec.name(),
                manufacturer: MANUFACTURER,
                description: DESCRIPTION,
                identity: spec.device(),
                instance: spec.instance(),
                info: &info,
                blocks: &blocks,
            },
            handler(producer, Arc::clone(&died)),
        )
        .map_err(os)?;
    let endpoint = MidiServicesVirtualEndpoint {
        died,
        live: Live::new(),
        device: Arc::new(device),
        _session: session,
    };
    Ok((endpoint, spec.protocol))
}
