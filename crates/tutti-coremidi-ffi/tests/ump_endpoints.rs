//! `ump_endpoints` reads what a UMP Endpoint with Function Blocks declares.
//!
//! The test registers a virtual UMP Endpoint of its own (macOS 15's
//! `MIDIUMPMutableEndpoint`), so the read has a real endpoint to find. The class
//! exists from macOS 15: the test asserts it is there on 15 or later, and that
//! `ump_endpoints` says `Unsupported` before it.
#![cfg(target_os = "macos")]
// SAFETY arguments are on each block; the calls are Objective-C initialisers
// with no preconditions beyond valid arguments.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::runtime::AnyClass;
use objc2_core_midi::{
    MIDI2DeviceInfo, MIDI2DeviceManufacturer, MIDI2DeviceRevisionLevel, MIDIEventList,
    MIDIProtocolID, MIDIUMPFunctionBlockDirection, MIDIUMPFunctionBlockMIDI1Info,
    MIDIUMPFunctionBlockUIHint, MIDIUMPMutableEndpoint, MIDIUMPMutableFunctionBlock,
};
use objc2_foundation::{NSArray, NSString};
use tutti_coremidi_ffi::{UmpEndpointInfo, UmpEndpointsError, ump_endpoints};
use tutti_ump::{
    BlockDirection, FirstGroup, FunctionBlockCount, FunctionBlockInfo, FunctionBlocks, GroupSpan,
    Midi1Port, Protocol, UiHint,
};

const KEYS: &str = "tutti-ffi-test Keys";
const PORT: &str = "tutti-ffi-test Port";

/// A block as CoreMIDI reports it is the block that was registered: the Groups
/// it spans, its direction, UI hint and MIDI 1.0 port field are the spec's
/// numbers (M2-104 §7.1.8), and the endpoint's Protocol is the one it was
/// created in (§7.1.6.2).
#[test]
fn an_endpoint_reads_back_its_protocol_and_blocks() {
    let has_class = AnyClass::get(c"MIDIUMPMutableEndpoint").is_some();
    let major = tutti_fixtures::macos_major().expect("sw_vers names the macOS version");
    assert_eq!(has_class, major >= 15, "the class exists from macOS 15");
    if !has_class {
        assert_eq!(ump_endpoints().err(), Some(UmpEndpointsError::Unsupported));
        return;
    }
    // Nothing reads the manager before the endpoint exists: with no run loop in
    // this process the manager's list is the one it first took, and the backend
    // (whose device watch runs one) is what sees later changes.

    // Held until the end of the test: the endpoint calls it for as long as it lives.
    let callback = RcBlock::new(|_: NonNull<MIDIEventList>, _: *mut c_void| {});
    // SAFETY: every initialiser takes valid arguments, and `callback` outlives
    // the endpoint it is registered with.
    let endpoint = unsafe {
        let info = MIDI2DeviceInfo::initWithManufacturerID_family_modelNumber_revisionLevel(
            MIDI2DeviceInfo::alloc(),
            MIDI2DeviceManufacturer {
                sysExIDByte: [0x7D, 0, 0],
            },
            1,
            2,
            MIDI2DeviceRevisionLevel {
                revisionLevel: [0, 0, 0, 1],
            },
        );
        let endpoint =
            MIDIUMPMutableEndpoint::initWithName_deviceInfo_productInstanceID_MIDIProtocol_destinationCallback(
                MIDIUMPMutableEndpoint::alloc(),
                &NSString::from_str("tutti-ffi-test"),
                &info,
                &NSString::from_str("tutti-ffi-test-1"),
                MIDIProtocolID::Protocol_2_0,
                &*callback as *const _ as *mut _,
            )
            .expect("CoreMIDI creates the virtual endpoint");
        let block = |name: &str, direction, first, span, midi1, hint| {
            MIDIUMPMutableFunctionBlock::initWithName_direction_firstGroup_totalGroupsSpanned_maxSysEx8Streams_MIDI1Info_UIHint_isEnabled(
                MIDIUMPMutableFunctionBlock::alloc(),
                &NSString::from_str(name),
                direction,
                first,
                span,
                3,
                midi1,
                hint,
                true,
            )
        };
        let (keys, port) = (
            block(
                KEYS,
                MIDIUMPFunctionBlockDirection::Bidirectional,
                0,
                2,
                MIDIUMPFunctionBlockMIDI1Info::NotMIDI1,
                MIDIUMPFunctionBlockUIHint::SenderReceiver,
            ),
            block(
                PORT,
                MIDIUMPFunctionBlockDirection::Input,
                5,
                1,
                MIDIUMPFunctionBlockMIDI1Info::RestrictedBandwidth,
                MIDIUMPFunctionBlockUIHint::Sender,
            ),
        );
        let (keys, port) = (
            keys.expect("a block is created"),
            port.expect("a block is created"),
        );
        let blocks = NSArray::from_retained_slice(&[keys, port]);
        endpoint
            .registerFunctionBlocks_markAsStatic_error(&blocks, true)
            .expect("the blocks register");
        endpoint
            .setEnabled_error(true)
            .expect("the endpoint enables");
        endpoint
    };

    let ours = ump_endpoints()
        .expect("the manager class exists and CoreMIDI raised no exception")
        .into_iter()
        .find(is_ours)
        .expect("the registered endpoint is listed");

    assert_eq!(
        ours.protocol,
        Protocol::Midi2,
        "created in the MIDI 2.0 Protocol"
    );
    assert!(
        ours.offered.midi2,
        "an endpoint created in the MIDI 2.0 Protocol supports it"
    );
    assert!(
        ours.offered.static_function_blocks,
        "the blocks were registered as static"
    );
    assert_eq!(
        ours.offered.function_blocks,
        FunctionBlocks::Count(FunctionBlockCount::of(2))
    );
    assert!(ours.source_unique_id.is_some() && ours.destination_unique_id.is_some());
    let named = |name: &str| {
        ours.blocks
            .iter()
            .find(|b| b.name.as_deref() == Some(name))
            .unwrap()
            .info
    };
    let groups = |info: FunctionBlockInfo| {
        (
            info.first_group.value().map(FirstGroup::get),
            info.groups_spanned.value().map(GroupSpan::get),
        )
    };
    let keys = named(KEYS);
    assert_eq!(keys.direction, BlockDirection::Bidirectional);
    assert_eq!(groups(keys), (Some(0), Some(2)));
    assert_eq!(keys.midi1, Midi1Port::No);
    assert_eq!(
        (keys.ui_hint, keys.max_sysex8_streams, keys.active),
        (UiHint::Both, 3, true)
    );
    let port = named(PORT);
    assert_eq!(port.direction, BlockDirection::Input);
    assert_eq!(groups(port), (Some(5), Some(1)));
    assert_eq!(port.midi1, Midi1Port::Restricted);
    assert_eq!(port.ui_hint, UiHint::Sender);
    assert_ne!(keys.id, port.id, "block numbers are distinct");

    // SAFETY: a plain setter on the live endpoint, so it leaves the cache.
    unsafe { endpoint.setEnabled_error(false).ok() };
    drop(callback);
}

fn is_ours(endpoint: &UmpEndpointInfo) -> bool {
    endpoint
        .blocks
        .iter()
        .any(|b| b.name.as_deref() == Some(KEYS))
}
