//! A virtual device: an endpoint this process publishes for other applications to connect to.

use tutti_platform::Instant;
use tutti_ump::{DeviceIdentity, EndpointInfoNotification, EndpointName, ProductInstanceId, Ump};
use windows_collections::IVector;
use windows_core::{HSTRING, Interface};

use super::call::{abi, call, statics};
use super::{Connection, Error, Session};
use crate::bindings::Windows::Devices::Midi2::Enumeration::{
    MidiDeclaredDeviceIdentity, MidiDeclaredEndpointInfo, MidiFunctionBlock,
    MidiFunctionBlockDirection, MidiFunctionBlockRepresentsMidi10Connection,
    MidiFunctionBlockUIHint,
};
use crate::bindings::Windows::Devices::Midi2::Transports::Virtual::{
    IMidiVirtualDeviceManagerStatics, MidiVirtualDevice, MidiVirtualDeviceCreationConfig,
    MidiVirtualDeviceManager,
};
use crate::bindings::Windows::Devices::Midi2::{IMidiEndpointMessageProcessingPlugin, MidiGroup};
use crate::checks::declared::{Block, PlatformBlock, PlatformInfo, platform};

/// What a virtual device declares: who it is, what it speaks, and its Function Blocks.
#[derive(Clone, Copy, Debug)]
pub struct VirtualDeviceSpec<'a> {
    /// The endpoint's name, which the service lists it under.
    pub name: &'a EndpointName,
    /// The manufacturer's name, which the service keeps with the device.
    pub manufacturer: &'a str,
    /// A description of the device, which the service keeps with it.
    pub description: &'a str,
    /// The Device Identity the endpoint declares.
    pub identity: &'a DeviceIdentity,
    /// The Product Instance Id the endpoint declares.
    pub instance: Option<&'a ProductInstanceId>,
    /// The Endpoint Info the endpoint declares: the protocols it speaks, and a block count
    /// that is `blocks.len()`.
    pub info: &'a EndpointInfoNotification,
    /// The Function Blocks the endpoint declares. The service lists each under its own
    /// number.
    pub blocks: &'a [Block],
}

/// A published virtual device and the app's connection to it.
///
/// The connection is the device's side of the endpoint: what it sends, other applications
/// receive, and what they send, its handler receives. Dropping it closes the connection,
/// which removes the endpoint from the service.
#[derive(Debug)]
pub struct VirtualDevice {
    // Dropped first: closing the connection is what removes the endpoint.
    connection: Connection,
    _device: MidiVirtualDevice,
}

impl Session {
    /// Publishes the virtual device `spec` describes and connects to it. `on_message`
    /// receives each message another application sends to the endpoint, as
    /// [`connect`](Self::connect)'s does: on a thread of the service, one at a time, behind a
    /// panic guard.
    ///
    /// # Errors
    ///
    /// `Error::Declaration` for a declaration the service cannot carry, `Error::ApiMissing`
    /// when the API is not present, `Error::Call` when a call fails and `Error::Refused`
    /// when the service creates no device, or does not open the connection.
    pub fn publish<F>(
        &self,
        spec: &VirtualDeviceSpec<'_>,
        on_message: F,
    ) -> Result<VirtualDevice, Error>
    where
        F: FnMut(Result<(Option<Instant>, Ump), Error>) + Send + 'static,
    {
        let (info, blocks) = platform(spec.info, spec.blocks)?;
        let config = configuration(spec, &info, &blocks)?;
        let manager = statics::<MidiVirtualDeviceManager, IMidiVirtualDeviceManagerStatics>()?;
        let device = call!(
            "CreateVirtualDevice",
            manager,
            CreateVirtualDevice(config.as_raw()) -> Option<MidiVirtualDevice>
        )?
        .ok_or(Error::Refused {
            call: "CreateVirtualDevice",
        })?;
        let endpoint =
            call!("DeviceEndpointDeviceId", device, DeviceEndpointDeviceId() -> HSTRING)?
                .to_string();
        let plugin: IMidiEndpointMessageProcessingPlugin = device
            .cast()
            .map_err(super::error::call("QueryInterface"))?;
        let connection = Connection::open(
            &self.session,
            self.clock,
            &endpoint,
            Some(&plugin),
            on_message,
        )?;
        Ok(VirtualDevice {
            connection,
            _device: device,
        })
    }
}

impl VirtualDevice {
    /// Sends `ump` to the applications connected to the endpoint, now.
    ///
    /// # Errors
    ///
    /// `Error::Closed` for a closed connection, `Error::BufferFull` when the service cannot
    /// take the message now and `Error::MessageRefused` for any other refusal.
    pub fn send(&self, ump: &Ump) -> Result<(), Error> {
        self.connection.send(None, ump)
    }
}

/// The creation config of the device `spec` describes, with its blocks.
fn configuration(
    spec: &VirtualDeviceSpec<'_>,
    info: &PlatformInfo,
    blocks: &[PlatformBlock],
) -> Result<MidiVirtualDeviceCreationConfig, Error> {
    let name = HSTRING::from(spec.name.as_str());
    let declared = MidiDeclaredEndpointInfo::CreateInstance(
        &name,
        &HSTRING::from(spec.instance.map_or("", ProductInstanceId::as_str)),
        info.midi1,
        info.midi2,
        info.rx_jr,
        info.tx_jr,
        info.static_blocks,
        info.block_count,
        info.version_major,
        info.version_minor,
    )
    .map_err(super::error::call(
        "MidiDeclaredEndpointInfo.CreateInstance",
    ))?;
    let [m1, m2, m3] = spec
        .identity
        .manufacturer
        .sysex_bytes()
        .map(tutti_ump::U7::get);
    let [f1, f2] = spec.identity.family.map(tutti_ump::U7::get);
    let [d1, d2] = spec.identity.model.map(tutti_ump::U7::get);
    let [r1, r2, r3, r4] = spec.identity.revision.get().map(tutti_ump::U7::get);
    let identity =
        MidiDeclaredDeviceIdentity::CreateInstance(m1, m2, m3, f1, f2, d1, d2, r1, r2, r3, r4)
            .map_err(super::error::call(
                "MidiDeclaredDeviceIdentity.CreateInstance",
            ))?;
    let config = MidiVirtualDeviceCreationConfig::CreateInstance2(
        &name,
        &HSTRING::from(spec.description),
        &HSTRING::from(spec.manufacturer),
        &declared,
        &identity,
    )
    .map_err(super::error::call(
        "MidiVirtualDeviceCreationConfig.CreateInstance",
    ))?;
    // The endpoint is the UMP one: no MIDI 1.0 port endpoints beside it.
    call!(
        "SetCreateOnlyUmpEndpoints",
        config,
        SetCreateOnlyUmpEndpoints(true)
    )?;
    let list = call!(
        "FunctionBlocks",
        config,
        FunctionBlocks() -> Option<IVector<MidiFunctionBlock>>
    )?
    .ok_or(Error::Refused {
        call: "FunctionBlocks",
    })?;
    for block in blocks {
        list.Append(&function_block(block)?)
            .map_err(super::error::call("FunctionBlocks.Append"))?;
    }
    Ok(config)
}

/// The service's Function Block for `block`.
fn function_block(block: &PlatformBlock) -> Result<MidiFunctionBlock, Error> {
    let made = MidiFunctionBlock::new().map_err(super::error::call("MidiFunctionBlock"))?;
    let first_group = MidiGroup::CreateInstance(block.first_group)
        .map_err(super::error::call("MidiGroup.CreateInstance"))?;
    let name = HSTRING::from(block.name.as_str());
    call!("SetNumber", made, SetNumber(block.number))?;
    call!("SetName", made, SetName(abi(&name)))?;
    call!("SetIsActive", made, SetIsActive(block.active))?;
    call!(
        "SetDirection",
        made,
        SetDirection(MidiFunctionBlockDirection(block.direction))
    )?;
    call!(
        "SetUIHint",
        made,
        SetUIHint(MidiFunctionBlockUIHint(block.ui_hint))
    )?;
    call!(
        "SetRepresentsMidi10Connection",
        made,
        SetRepresentsMidi10Connection(MidiFunctionBlockRepresentsMidi10Connection(block.midi1))
    )?;
    call!("SetFirstGroup", made, SetFirstGroup(first_group.as_raw()))?;
    call!("SetGroupCount", made, SetGroupCount(block.groups))?;
    call!(
        "SetMidiCIMessageVersionFormat",
        made,
        SetMidiCIMessageVersionFormat(block.ci_version)
    )?;
    call!(
        "SetMaxSystemExclusive8Streams",
        made,
        SetMaxSystemExclusive8Streams(block.max_sysex8_streams)
    )?;
    Ok(made)
}
