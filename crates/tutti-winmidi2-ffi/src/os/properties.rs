//! What the service declares of an endpoint: its Endpoint Info and Function Blocks.

use windows_collections::IVectorView;
use windows_core::HSTRING;

use super::Error;
use super::call::call;
use crate::bindings::Windows::Devices::Midi2::Enumeration::{
    MidiDeclaredEndpointInfo, MidiEndpointDeviceInformation, MidiFunctionBlock,
    MidiFunctionBlockDirection, MidiFunctionBlockRepresentsMidi10Connection,
    MidiFunctionBlockUIHint,
};
use crate::bindings::Windows::Devices::Midi2::MidiGroup;
use crate::checks::declared::{PlatformBlock, PlatformInfo};
use crate::checks::read::{Declared, endpoint_info, function_block};

/// The properties `information` declares.
///
/// # Errors
///
/// `Error::Call` when a read fails and `Error::Refused` when the service answers with no
/// object.
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
    let blocks = list
        .into_iter()
        .map(|block| platform_block(&block).map(|block| function_block(&block)))
        .collect::<Result<_, _>>()?;
    Ok(Declared {
        info: endpoint_info(&info).ok_or(Error::Refused {
            call: "endpoint info",
        })?,
        blocks,
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
