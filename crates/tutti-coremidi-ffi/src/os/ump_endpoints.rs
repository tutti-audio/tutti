//! The UMP Endpoints and Function Blocks CoreMIDI reports (macOS 15).
//!
//! `MIDIUMPEndpointManager`, `MIDIUMPEndpoint` and `MIDIUMPFunctionBlock` are
//! Objective-C classes, so `coremidi-sys` (C functions) does not carry them;
//! `objc2-core-midi`'s generated bindings do, and this module is the only code
//! that names them. What it returns is `tutti-ump`'s values (a Function
//! Block Info and an Endpoint Info Notification, with a value the spec does not
//! define kept as the field's reserved raw); `tutti-midi-hardware` turns them
//! into `tutti-midi-device`'s Function Blocks and checks them against M2-104
//! there.
//!
//! The classes do not exist before macOS 15. [`ump_endpoints`] asks the
//! Objective-C runtime for the manager class first and returns
//! [`UmpEndpointsError::Unsupported`] where it is absent. Every message send goes
//! through [`catch`], so an Objective-C exception CoreMIDI raises (as seen when no
//! MIDI server is reachable) comes back as [`UmpEndpointsError::Exception`]
//! instead of unwinding into Rust, which aborts the process.

use super::objc_exception::{ObjcException, catch};
use crate::UniqueId;
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyClass;
use objc2_core_midi::{
    MIDI2DeviceInfo, MIDIUMPEndpoint, MIDIUMPEndpointManager, MIDIUMPFunctionBlock,
    MIDIUMPProtocolOptions,
};
use tutti_ump::{
    DeviceIdentity, EndpointInfoNotification, FunctionBlockCount, FunctionBlockInfo,
    FunctionBlocks, ManufacturerId, OsFunctionBlock, Protocol, SoftwareRevision, U7, UiHint,
    UmpVersion, read_function_block, read_protocol,
};

/// One Function Block as CoreMIDI reports it (`MIDIUMPFunctionBlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlockInfo {
    /// What the block says of itself, as a Function Block Info Notification
    /// carries it. A value CoreMIDI reports that the spec does not define is the
    /// field's reserved raw. `MIDIUMPFunctionBlock` has no MIDI-CI version, so
    /// that is `0` (none).
    pub info: FunctionBlockInfo,
    /// `name`, when it is not empty.
    pub name: Option<String>,
}

/// One UMP Endpoint as CoreMIDI reports it (`MIDIUMPEndpoint`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpointInfo {
    /// `MIDIProtocol`, the protocol in use: `kMIDIProtocol_1_0` is 1 and
    /// `kMIDIProtocol_2_0` is 2, anything else reserved.
    pub protocol: Protocol,
    /// What the endpoint offers, as its Endpoint Info Notification:
    /// `supportedMIDIProtocols` (the endpoint may negotiate MIDI 2.0 later
    /// even while `protocol` is MIDI 1.0), whether its blocks are static, how
    /// many it has and its JR Timestamp capabilities. CoreMIDI reports no UMP
    /// version, so that is UMP 1.1, which macOS 15 speaks.
    pub offered: EndpointInfoNotification,
    /// The unique id of the endpoint's source (what a device sends from), when
    /// it has one.
    pub source_unique_id: Option<UniqueId>,
    /// The unique id of its destination, when it has one.
    pub destination_unique_id: Option<UniqueId>,
    /// Its Device Identity (`deviceInfo`), when its bytes form one.
    pub identity: Option<DeviceIdentity>,
    /// Its Product Instance Id (`productInstanceID`), when it is not empty.
    pub instance: Option<String>,
    /// Its Function Blocks.
    pub blocks: Vec<UmpBlockInfo>,
}

/// Why [`ump_endpoints`] returned no endpoints.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UmpEndpointsError {
    /// The OS has no `MIDIUMPEndpointManager` (before macOS 15).
    #[error("this macOS has no MIDIUMPEndpointManager (before macOS 15)")]
    Unsupported,
    /// CoreMIDI raised an Objective-C exception (as seen when no MIDI server is
    /// reachable).
    #[error(transparent)]
    Exception(#[from] ObjcException),
}

/// Every UMP Endpoint now.
///
/// On macOS 15 CoreMIDI's listing reports every Function Block's direction as 0,
/// so each listed block reads `BlockDirection::Reserved(0)`; the app that created
/// the endpoint holds the direction it registered. From macOS 26 the listing
/// carries the registered direction.
///
/// # Errors
///
/// [`UmpEndpointsError::Unsupported`] before macOS 15, and
/// [`UmpEndpointsError::Exception`] when CoreMIDI raises an Objective-C
/// exception.
///
/// It takes no [`SharedClient`](crate::SharedClient): the listing is reached
/// where no client may be started, and the exception CoreMIDI raises here with
/// no MIDI server is one the crate's exception catch catches.
pub fn ump_endpoints() -> Result<Vec<UmpEndpointInfo>, UmpEndpointsError> {
    // `ClassType::class` panics on a missing class; the lookup by name is the
    // availability check.
    if AnyClass::get(c"MIDIUMPEndpointManager").is_none() {
        return Err(UmpEndpointsError::Unsupported);
    }
    autoreleasepool(|_| {
        catch(|| {
            // SAFETY: the class exists (checked above), `sharedInstance` and
            // `UMPEndpoints` take no arguments and are safe to call from any thread
            // (Apple documents the manager as a shared singleton), and the arrays
            // they return are owned (`Retained`), so they outlive the reads.
            let endpoints = unsafe { MIDIUMPEndpointManager::sharedInstance().UMPEndpoints() };
            endpoints.iter().map(|e| endpoint_info(&e)).collect()
        })
    })
    .map_err(UmpEndpointsError::Exception)
}

/// A CoreMIDI enum value as the byte the spec's field holds: one that is no
/// byte is `u8::MAX`, which no field of an endpoint or block defines.
fn byte(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// The UI hint CoreMIDI's `UIHint` names (`kMIDIUMPFunctionBlockUIHint*`: 0
/// unknown, 1 receiver, 2 sender, 3 both). It is the two bits the wire carries,
/// so the bits above them are not part of it.
fn ui_hint_of(raw: i32) -> UiHint {
    match raw & 0x3 {
        0 => UiHint::Unknown,
        1 => UiHint::Receiver,
        2 => UiHint::Sender,
        _ => UiHint::Both,
    }
}

fn endpoint_info(endpoint: &MIDIUMPEndpoint) -> UmpEndpointInfo {
    // SAFETY: plain getters on a live endpoint object.
    let (
        protocol,
        supported,
        source,
        destination,
        blocks,
        is_static,
        rx_jr,
        tx_jr,
        device,
        instance,
    ) = unsafe {
        (
            endpoint.MIDIProtocol().0,
            endpoint.supportedMIDIProtocols(),
            endpoint.MIDISource(),
            endpoint.MIDIDestination(),
            endpoint.functionBlocks(),
            endpoint.hasStaticFunctionBlocks(),
            endpoint.hasJRTSReceiveCapability(),
            endpoint.hasJRTSTransmitCapability(),
            endpoint.deviceInfo(),
            endpoint.productInstanceID().to_string(),
        )
    };
    let blocks: Vec<UmpBlockInfo> = blocks.iter().map(|b| block_info(&b)).collect();
    UmpEndpointInfo {
        protocol: read_protocol(byte(protocol)),
        offered: EndpointInfoNotification {
            version: UmpVersion::V1_1,
            static_function_blocks: is_static,
            function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(blocks.len())),
            midi2: supported.contains(MIDIUMPProtocolOptions::MIDI2),
            midi1: supported.contains(MIDIUMPProtocolOptions::MIDI1),
            rx_jr,
            tx_jr,
        },
        source_unique_id: UniqueId::of(source),
        destination_unique_id: UniqueId::of(destination),
        identity: identity_of(&device),
        instance: (!instance.is_empty()).then_some(instance),
        blocks,
    }
}

/// The Device Identity `info` holds, or `None` when a byte is not 7 bits or the
/// manufacturer bytes are no id.
fn identity_of(info: &MIDI2DeviceInfo) -> Option<DeviceIdentity> {
    let pair = |value: u16| {
        Some([
            U7::new(u8::try_from(value & 0x7F).ok()?)?,
            U7::new(u8::try_from(value >> 7).ok()?)?,
        ])
    };
    let bytes = |raw: [u8; 4]| -> Option<[U7; 4]> {
        Some([
            U7::new(raw[0])?,
            U7::new(raw[1])?,
            U7::new(raw[2])?,
            U7::new(raw[3])?,
        ])
    };
    // SAFETY: plain getters on a live device info object.
    let (manufacturer, family, model, revision) = unsafe {
        (
            info.manufacturerID().sysExIDByte,
            info.family(),
            info.modelNumber(),
            info.revisionLevel().revisionLevel,
        )
    };
    let [a, b, c] = manufacturer;
    Some(DeviceIdentity {
        manufacturer: ManufacturerId::try_from([U7::new(a)?, U7::new(b)?, U7::new(c)?]).ok()?,
        family: pair(family)?,
        model: pair(model)?,
        revision: SoftwareRevision::from(bytes(revision)?),
    })
}

fn block_info(block: &MIDIUMPFunctionBlock) -> UmpBlockInfo {
    // SAFETY: plain getters on a live function block object.
    let (id, active, direction, first_group, groups, midi1, ui_hint, sysex8_streams, name) = unsafe {
        (
            block.functionBlockID(),
            block.isEnabled(),
            block.direction().0,
            block.firstGroup(),
            block.totalGroupsSpanned(),
            block.MIDI1Info().0,
            block.UIHint().0,
            block.maxSysEx8Streams(),
            block.name().to_string(),
        )
    };
    UmpBlockInfo {
        info: read_function_block(OsFunctionBlock {
            active,
            id,
            direction: byte(direction),
            ui_hint: ui_hint_of(ui_hint),
            midi1: byte(midi1),
            first_group,
            groups_spanned: groups,
            midi_ci_version: 0,
            max_sysex8_streams: sysex8_streams,
        }),
        name: (!name.is_empty()).then_some(name),
    }
}
