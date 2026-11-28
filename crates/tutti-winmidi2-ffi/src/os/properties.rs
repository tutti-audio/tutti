//! What the service declares of an endpoint: its Endpoint Info and Function Blocks.

use windows_collections::IVectorView;
use windows_core::HSTRING;

use super::Error;
use super::call::call;
use super::error::call as call_error;
use crate::bindings::Windows::Devices::Midi2::Enumeration::{
    MidiDeclaredEndpointInfo, MidiDeclaredStreamConfiguration, MidiEndpointDeviceInformation,
    MidiFunctionBlock, MidiFunctionBlockDirection, MidiFunctionBlockRepresentsMidi10Connection,
    MidiFunctionBlockUIHint, MidiProtocol,
};
use crate::bindings::Windows::Devices::Midi2::MidiGroup;
use crate::checks::declared::{PlatformBlock, PlatformInfo};
use crate::checks::read::{Declared, agreed_protocol, endpoint_info, function_block};

/// The properties `information` declares.
///
/// # Errors
///
/// `Error::Call` when a read fails, the block list's reads (its size and each block) and
/// the stream configuration's included, and `Error::Refused` when the service answers with
/// no Endpoint Info, block list or block group. No stream configuration is no agreed
/// protocol.
pub(super) fn read(information: &MidiEndpointDeviceInformation) -> Result<Declared, Error> {
    let declared = call!(
        "GetDeclaredEndpointInfo",
        *information,
        GetDeclaredEndpointInfo() -> Option<MidiDeclaredEndpointInfo>
    )?
    .ok_or(Error::Refused {
        call: "GetDeclaredEndpointInfo",
    })?;
    let info = PlatformInfo {
        midi1: call!("SupportsMidi10Protocol", declared, SupportsMidi10Protocol() -> bool)?,
        midi2: call!("SupportsMidi20Protocol", declared, SupportsMidi20Protocol() -> bool)?,
        rx_jr: call!(
            "SupportsReceivingJitterReductionTimestamps",
            declared,
            SupportsReceivingJitterReductionTimestamps() -> bool
        )?,
        tx_jr: call!(
            "SupportsSendingJitterReductionTimestamps",
            declared,
            SupportsSendingJitterReductionTimestamps() -> bool
        )?,
        static_blocks: call!("HasStaticFunctionBlocks", declared, HasStaticFunctionBlocks() -> bool)?,
        block_count: call!(
            "DeclaredFunctionBlockCount",
            declared,
            DeclaredFunctionBlockCount() -> u8
        )?,
        version_major: call!(
            "SpecificationVersionMajor",
            declared,
            SpecificationVersionMajor() -> u8
        )?,
        version_minor: call!(
            "SpecificationVersionMinor",
            declared,
            SpecificationVersionMinor() -> u8
        )?,
    };
    let list = call!(
        "GetDeclaredFunctionBlocks",
        *information,
        GetDeclaredFunctionBlocks() -> Option<IVectorView<MidiFunctionBlock>>
    )?
    .ok_or(Error::Refused {
        call: "GetDeclaredFunctionBlocks",
    })?;
    let count = list.Size().map_err(call_error("IVectorView::Size"))?;
    let blocks = (0..count)
        .map(|index| {
            let block = list
                .GetAt(index)
                .map_err(call_error("IVectorView::GetAt"))?;
            platform_block(&block).map(|block| function_block(&block))
        })
        .collect::<Result<_, _>>()?;
    let stream = call!(
        "GetDeclaredStreamConfiguration",
        *information,
        GetDeclaredStreamConfiguration() -> Option<MidiDeclaredStreamConfiguration>
    )?;
    // An endpoint with no stream configuration has agreed no protocol.
    let protocol = match stream {
        Some(stream) => agreed_protocol(call!("Protocol", stream, Protocol() -> MidiProtocol)?.0),
        None => None,
    };
    Ok(Declared {
        info: endpoint_info(&info),
        blocks,
        protocol,
    })
}

/// The service's numbers of `block`.
fn platform_block(block: &MidiFunctionBlock) -> Result<PlatformBlock, Error> {
    let first_group = call!("FirstGroup", *block, FirstGroup() -> Option<MidiGroup>)?
        .ok_or(Error::Refused { call: "FirstGroup" })?;
    Ok(PlatformBlock {
        number: call!("Number", *block, Number() -> u8)?,
        name: call!("Name", *block, Name() -> HSTRING)?.to_string(),
        active: call!("IsActive", *block, IsActive() -> bool)?,
        direction: call!("Direction", *block, Direction() -> MidiFunctionBlockDirection)?.0,
        ui_hint: call!("UIHint", *block, UIHint() -> MidiFunctionBlockUIHint)?.0,
        midi1: call!(
            "RepresentsMidi10Connection",
            *block,
            RepresentsMidi10Connection() -> MidiFunctionBlockRepresentsMidi10Connection
        )?
        .0,
        first_group: call!("Index", first_group, Index() -> u8)?,
        groups: call!("GroupCount", *block, GroupCount() -> u8)?,
        ci_version: call!(
            "MidiCIMessageVersionFormat",
            *block,
            MidiCIMessageVersionFormat() -> u8
        )?,
        max_sysex8_streams: call!(
            "MaxSystemExclusive8Streams",
            *block,
            MaxSystemExclusive8Streams() -> u8
        )?,
    })
}
