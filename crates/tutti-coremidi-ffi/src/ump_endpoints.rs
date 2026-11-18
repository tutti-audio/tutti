//! The UMP Endpoints and Function Blocks CoreMIDI reports (macOS 15).
//!
//! `MIDIUMPEndpointManager`, `MIDIUMPEndpoint` and `MIDIUMPFunctionBlock` are
//! Objective-C classes, so `coremidi-sys` (C functions) does not carry them;
//! `objc2-core-midi`'s generated bindings do, and this module is the only code
//! that names them. What it returns is plain values holding the OS's own
//! numbers; `tutti-midi-hardware` turns them into `tutti-midi-device`'s
//! Function Blocks and checks them against M2-104 there.
//!
//! The classes do not exist before macOS 15. [`ump_endpoints`] asks the
//! Objective-C runtime for the manager class first and returns
//! [`UmpEndpointsError::Unsupported`] where it is absent. Every message send goes
//! through [`catch`], so an Objective-C exception CoreMIDI raises (as seen when no
//! MIDI server is reachable) comes back as [`UmpEndpointsError::Exception`]
//! instead of unwinding into Rust, which aborts the process.

use crate::objc_exception::{ObjcException, catch};
use crate::unique_id::UniqueId;
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyClass;
use objc2_core_midi::{
    MIDIUMPEndpoint, MIDIUMPEndpointManager, MIDIUMPFunctionBlock, MIDIUMPProtocolOptions,
};

/// One Function Block as CoreMIDI reports it (`MIDIUMPFunctionBlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpBlockInfo {
    /// `functionBlockID`: the block's number.
    pub id: u8,
    /// `isEnabled`.
    pub active: bool,
    /// `direction`: `kMIDIUMPFunctionBlockDirection*`, 0 unknown, 1 input, 2
    /// output, 3 bidirectional.
    pub direction: i32,
    /// `firstGroup`, from 0.
    pub first_group: u8,
    /// `totalGroupsSpanned`.
    pub groups: u8,
    /// `MIDI1Info`: 0 not a MIDI 1.0 port, 1 unrestricted, 2 restricted.
    pub midi1: i32,
    /// `UIHint`: 0 unknown, 1 receiver, 2 sender, 3 both.
    pub ui_hint: i32,
    /// `maxSysEx8Streams`.
    pub sysex8_streams: u8,
    /// `name`.
    pub name: String,
}

/// One UMP Endpoint as CoreMIDI reports it (`MIDIUMPEndpoint`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UmpEndpointInfo {
    /// `MIDIProtocol`, the protocol in use: `kMIDIProtocol_1_0` is 1,
    /// `kMIDIProtocol_2_0` is 2.
    pub protocol: i32,
    /// Whether `supportedMIDIProtocols` includes the MIDI 2.0 Protocol: the
    /// endpoint may negotiate it later even while `protocol` is MIDI 1.0.
    pub supports_midi2: bool,
    /// The unique id of the endpoint's source (what a device sends from), when
    /// it has one.
    pub source_unique_id: Option<UniqueId>,
    /// The unique id of its destination, when it has one.
    pub destination_unique_id: Option<UniqueId>,
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
/// # Errors
///
/// [`UmpEndpointsError::Unsupported`] before macOS 15, and
/// [`UmpEndpointsError::Exception`] when CoreMIDI raises an Objective-C
/// exception.
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

fn endpoint_info(endpoint: &MIDIUMPEndpoint) -> UmpEndpointInfo {
    // SAFETY: plain getters on a live endpoint object.
    let (protocol, supported, source, destination, blocks) = unsafe {
        (
            endpoint.MIDIProtocol().0,
            endpoint.supportedMIDIProtocols(),
            endpoint.MIDISource(),
            endpoint.MIDIDestination(),
            endpoint.functionBlocks(),
        )
    };
    UmpEndpointInfo {
        protocol,
        supports_midi2: supported.contains(MIDIUMPProtocolOptions::MIDI2),
        source_unique_id: UniqueId::of(source),
        destination_unique_id: UniqueId::of(destination),
        blocks: blocks.iter().map(|b| block_info(&b)).collect(),
    }
}

fn block_info(block: &MIDIUMPFunctionBlock) -> UmpBlockInfo {
    // SAFETY: plain getters on a live function block object.
    unsafe {
        UmpBlockInfo {
            id: block.functionBlockID(),
            active: block.isEnabled(),
            direction: block.direction().0,
            first_group: block.firstGroup(),
            groups: block.totalGroupsSpanned(),
            midi1: block.MIDI1Info().0,
            ui_hint: block.UIHint().0,
            sysex8_streams: block.maxSysEx8Streams(),
            name: block.name().to_string(),
        }
    }
}
