//! Virtual endpoints on the sequencer: a client of this process with one port
//! that other clients subscribe to and write to.
//!
//! The client is a UMP client speaking the protocol the spec asks for, or MIDI
//! 1.0 where the kernel's sequencer has no MIDI 2.0 client mode; the protocol it
//! got is what the endpoint reports. It publishes a UMP Endpoint record (the
//! protocol, the spec's Device Identity and Product Instance Id, and its Function
//! Blocks), so an application enumerating it reads the protocol it carries
//! instead of taking it for a legacy port.

use tutti_alsa_ffi::{
    Client, ClientHandle, Operation, PortCaps, PortId, PortKind, UmpBlock, UmpEndpoint, UmpVersion,
};
use tutti_midi_device::{FunctionBlock, UMP_VERSION};
use tutti_platform::Executor;
use tutti_ump::{EndpointInfoNotification, FunctionBlockCount, FunctionBlocks, Protocol, Ump};

use super::input::{self, AlsaInput};
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{
    Delivery, EndpointBackend, SenderBackend, VirtualProtocol, VirtualSpec,
};
use std::sync::{Arc, PoisonError, RwLock};

/// The sequencer's version for a protocol.
fn version_of(protocol: VirtualProtocol) -> UmpVersion {
    match protocol {
        VirtualProtocol::Midi1 => UmpVersion::Midi1,
        VirtualProtocol::Midi2 => UmpVersion::Midi2,
    }
}

/// Opens the endpoint's client in the protocol the spec asks for, or in MIDI 1.0
/// when the kernel refuses a MIDI 2.0 client; the protocol it got goes with it.
fn open_client(spec: &VirtualSpec) -> Result<(Client, VirtualProtocol)> {
    let name = spec.name().as_str();
    match super::client::open(name, version_of(spec.protocol)) {
        Ok(client) => Ok((client, spec.protocol)),
        Err(MidiHardwareError::Os(tutti_alsa_ffi::Error::Call {
            operation: Operation::SetClientMidiVersion,
            source,
        })) if spec.protocol == VirtualProtocol::Midi2 => {
            // The call that fails when the kernel has no MIDI 2.0 client mode.
            tracing::debug!("virtual MIDI endpoint falls back to MIDI 1.0 ({source})");
            Ok((
                super::client::open(name, UmpVersion::Midi1)?,
                VirtualProtocol::Midi1,
            ))
        }
        Err(e) => Err(e),
    }
}

/// The sequencer's record of a Function Block.
fn ump_block(block: &FunctionBlock) -> UmpBlock {
    UmpBlock {
        info: block.info(),
        name: block.name().map(|name| name.as_str().to_owned()),
    }
}

/// Publishes the endpoint record on `client`: the protocol it carries, the spec's
/// Device Identity and Product Instance Id, and its blocks.
fn publish(client: &Client, spec: &VirtualSpec, protocol: VirtualProtocol) -> Result<()> {
    let (in_use, midi1, midi2) = match protocol {
        VirtualProtocol::Midi1 => (Protocol::Midi1, true, false),
        VirtualProtocol::Midi2 => (Protocol::Midi2, false, true),
    };
    let blocks: Vec<UmpBlock> = spec.function_blocks().iter().map(ump_block).collect();
    // The record names the MIDI protocol only: it offers no JR Timestamps in
    // either direction, so the endpoint declares that it sends and receives none
    // (the spec has no JR support).
    client.publish_ump_endpoint(&UmpEndpoint {
        protocol: in_use,
        offered: EndpointInfoNotification {
            version: UMP_VERSION,
            static_function_blocks: true,
            function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(blocks.len())),
            midi2,
            midi1,
            rx_jr: false,
            tx_jr: false,
        },
        name: Some(spec.name().as_str().to_owned()),
        device: Some(*spec.device()),
        instance: spec.instance().copied(),
        blocks,
    })?;
    Ok(())
}

/// A virtual endpoint: the client's one port, which peers subscribe to and
/// write to, read by a pump into the producer. Dropping it stops the pump and
/// closes the client, which removes the endpoint.
pub(crate) struct AlsaVirtualEndpoint {
    input: AlsaInput,
    port: PortId,
    /// Read-locked by a send for its whole call and cleared under the write lock
    /// by `Drop`, so a drop waits for a send in flight and no send follows it.
    live: Arc<RwLock<bool>>,
}

impl Drop for AlsaVirtualEndpoint {
    fn drop(&mut self) {
        *self.live.write().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// What a send's result says: a subscriber with no room is [`Delivery::Full`], the
/// non-blocking client's refusal, and any other failure is the OS's error.
fn delivery(sent: tutti_alsa_ffi::Result<()>) -> Result<Delivery> {
    match sent {
        Ok(()) => Ok(Delivery::Sent),
        Err(error) if error.would_block() => Ok(Delivery::Full),
        Err(error) => Err(error.into()),
    }
}

/// A send on a virtual endpoint's client, which it does not keep open.
struct AlsaSender {
    client: ClientHandle,
    port: PortId,
    live: Arc<RwLock<bool>>,
}

impl SenderBackend for AlsaSender {
    fn send(&self, ump: &Ump) -> Result<Delivery> {
        let live = self.live.read().unwrap_or_else(PoisonError::into_inner);
        if !*live {
            return Err(MidiHardwareError::EndpointGone);
        }
        let sent = self
            .client
            .with(|client| client.send_to_subscribers(self.port, ump))
            .ok_or(MidiHardwareError::EndpointGone)?;
        delivery(sent)
    }
}

impl EndpointBackend for AlsaVirtualEndpoint {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        Arc::new(AlsaSender {
            client: self.input.client(),
            port: self.port,
            live: Arc::clone(&self.live),
        })
    }

    fn is_alive(&self) -> bool {
        InputConnection::is_alive(&self.input)
    }
}

/// Publishes a virtual endpoint for `spec`: one client with one UMP Endpoint
/// record and one port that is readable, writable and subscribable. What peers
/// write to it is read by a pump into `producer`, each event stamped by the
/// kernel as it arrives; what the app sends goes to the port's subscribers.
pub(crate) fn endpoint(
    executor: &Executor,
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(AlsaVirtualEndpoint, VirtualProtocol)> {
    // Non-blocking: the pump reads until the queue is empty and then waits in
    // `poll`, and a send from another thread shares the client with it.
    let (client, protocol) = open_client(spec)?;
    let port = client.open_port(
        spec.name().as_str(),
        PortCaps::READ | PortCaps::SUBS_READ | PortCaps::WRITE | PortCaps::SUBS_WRITE,
        PortKind::MIDI_GENERIC | PortKind::APPLICATION,
    )?;
    publish(&client, spec, protocol)?;
    let clock = client.stamp_port(port)?;
    let input = input::start(executor, client, clock, producer)?;
    let live = Arc::new(RwLock::new(true));
    Ok((AlsaVirtualEndpoint { input, port, live }, protocol))
}

#[cfg(test)]
mod tests;
