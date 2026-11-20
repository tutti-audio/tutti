//! Virtual endpoints on the sequencer: a client of this process with one port
//! other clients subscribe to.
//!
//! The client is a UMP client speaking the protocol the spec asks for, or MIDI
//! 1.0 where the kernel's sequencer has no MIDI 2.0 client mode; the protocol it
//! got is what the endpoint reports. It publishes a UMP Endpoint record (the
//! protocol, the spec's Device Identity and Product Instance Id, and its Function
//! Blocks), so an application enumerating it reads the protocol it carries
//! instead of taking it for a legacy port.

use tutti_alsa_ffi::{
    Blocking, Client, Operation, PortCaps, PortId, PortKind, UmpBlock, UmpEndpoint, UmpVersion,
};
use tutti_midi_device::{FunctionBlock, UMP_VERSION};
use tutti_rt::LockExt;
use tutti_ump::{EndpointInfoNotification, FunctionBlockCount, FunctionBlocks, Protocol, Ump};

use super::input::{self, AlsaInput};
use crate::InputProducerHandle;
use crate::error::{MidiHardwareError, Result};
use crate::virtual_endpoint::{SenderBackend, SourceBackend, VirtualProtocol, VirtualSpec};
use std::sync::{Arc, Weak};

/// The sequencer's version for a protocol.
fn version_of(protocol: VirtualProtocol) -> UmpVersion {
    match protocol {
        VirtualProtocol::Midi1 => UmpVersion::Midi1,
        VirtualProtocol::Midi2 => UmpVersion::Midi2,
    }
}

/// Opens the endpoint's client in the protocol the spec asks for, or in MIDI 1.0
/// when the kernel refuses a MIDI 2.0 client; the protocol it got goes with it.
fn open_client(spec: &VirtualSpec, blocking: Blocking) -> Result<(Client, VirtualProtocol)> {
    let name = spec.name().as_str();
    match super::client::open(name, blocking, version_of(spec.protocol)) {
        Ok(client) => Ok((client, spec.protocol)),
        Err(MidiHardwareError::Os(tutti_alsa_ffi::Error::Call {
            operation: Operation::SetClientMidiVersion,
            source,
        })) if spec.protocol == VirtualProtocol::Midi2 => {
            // The call that fails when the kernel has no MIDI 2.0 client mode.
            tracing::debug!("virtual MIDI endpoint falls back to MIDI 1.0 ({source})");
            Ok((
                super::client::open(name, blocking, UmpVersion::Midi1)?,
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

/// A virtual source: the client's readable, subscribable port.
pub(crate) struct AlsaVirtualSource {
    // `Client` is `Send` and not `Sync`; sends are rare and short. The senders
    // hold it weakly, so the endpoint goes with the source.
    inner: Arc<std::sync::Mutex<(Client, PortId)>>,
}

/// A send on a virtual source's client, which it does not keep alive.
struct AlsaSender(Weak<std::sync::Mutex<(Client, PortId)>>);

impl SenderBackend for AlsaSender {
    fn send(&self, ump: &Ump) -> Result<()> {
        let inner = self.0.upgrade().ok_or(MidiHardwareError::EndpointGone)?;
        // A poisoned lock guards a client handle and a port number, which a
        // panic cannot leave half-written.
        let inner = inner.lock_recover();
        inner.0.send_to_subscribers(inner.1, ump)?;
        Ok(())
    }
}

impl SourceBackend for AlsaVirtualSource {
    fn sender(&self) -> Arc<dyn SenderBackend> {
        Arc::new(AlsaSender(Arc::downgrade(&self.inner)))
    }
}

/// Publishes a virtual source for `spec`.
pub(crate) fn source(spec: &VirtualSpec) -> Result<(AlsaVirtualSource, VirtualProtocol)> {
    let (client, protocol) = open_client(spec, Blocking::Block)?;
    let port = client.open_port(
        spec.name().as_str(),
        PortCaps::READ | PortCaps::SUBS_READ,
        PortKind::MIDI_GENERIC | PortKind::APPLICATION,
    )?;
    publish(&client, spec, protocol)?;
    Ok((
        AlsaVirtualSource {
            inner: Arc::new(std::sync::Mutex::new((client, port))),
        },
        protocol,
    ))
}

/// Publishes a virtual destination for `spec`: the client's writable,
/// subscribable port, read by a pump into `producer`, each event stamped by the
/// kernel as it arrives.
pub(crate) fn destination(
    spec: &VirtualSpec,
    producer: InputProducerHandle,
) -> Result<(AlsaInput, VirtualProtocol)> {
    let (client, protocol) = open_client(spec, Blocking::NonBlock)?;
    let port = client.open_port(
        spec.name().as_str(),
        PortCaps::WRITE | PortCaps::SUBS_WRITE,
        PortKind::MIDI_GENERIC | PortKind::APPLICATION,
    )?;
    publish(&client, spec, protocol)?;
    let clock = client.stamp_port(port)?;
    Ok((input::start(client, clock, producer)?, protocol))
}
