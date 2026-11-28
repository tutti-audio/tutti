use super::watch::change_of;
use super::*;
use crate::capability::legacy;
use tutti_midi_device::{OsChange, PortDirection};
use tutti_ump::{EndpointInfoNotification, FunctionBlockCount, FunctionBlocks, UmpVersion};
use tutti_winmidi2_ffi::Change;

fn declared(midi1: bool, midi2: bool, protocol: Option<Protocol>) -> Declared {
    Declared {
        info: EndpointInfoNotification {
            version: UmpVersion::V1_1,
            static_function_blocks: false,
            function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(0)),
            midi2,
            midi1,
            rx_jr: false,
            tx_jr: false,
        },
        blocks: Vec::new(),
        protocol,
    }
}

fn endpoint(id: &str) -> Endpoint {
    Endpoint {
        id: id.to_owned(),
        name: "Loopback".to_owned(),
        loopback: None,
        declared: Some(declared(true, true, Some(Protocol::Midi2))),
    }
}

/// **The id is FNV-1a's 64-bit digest of the path, whatever its case**: its published
/// vector for "a", for "A" too.
#[test]
fn an_endpoint_id_is_the_fnv1a_digest_of_the_lowercased_path() {
    assert_eq!(endpoint_id("a").raw(), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(endpoint_id("A"), endpoint_id("a"));
}

/// **Two endpoints have two ids.**
#[test]
fn distinct_paths_have_distinct_ids() {
    let (a, b) = (r"\\?\swd#midisrv#a", r"\\?\swd#midisrv#b");
    assert_ne!(endpoint_id(a), endpoint_id(b));
}

/// **The API and the service not being there are their named causes**; any other failure is
/// the platform's own.
#[test]
fn availability_failures_are_named_and_others_are_the_platforms() {
    assert!(matches!(
        os(Error::ApiMissing { code: -2 }),
        MidiHardwareError::MidiServicesApiMissing { code: -2 }
    ));
    assert!(matches!(
        os(Error::ServiceUnavailable),
        MidiHardwareError::MidiServicesNotRunning
    ));
    assert!(matches!(
        os(Error::Closed),
        MidiHardwareError::Os(Error::Closed)
    ));
}

/// **An added or updated endpoint is present as a duplex device, and a removed one is gone
/// under the id it was added with, whatever the case of the path** the service reports it
/// by.
#[test]
fn a_change_is_present_or_gone_under_the_endpoints_id() {
    let path = r"\\?\SWD#MidiSrv#A";
    let OsChange::Present(device) = change_of(Change::Added(endpoint(path))) else {
        panic!("an added endpoint is present");
    };
    assert_eq!(device.direction, PortDirection::Duplex);
    assert_eq!(device.handle, endpoint_id(path));
    assert_eq!(
        device.identity,
        OsEndpointIdentity::MidiServices(path.to_owned())
    );
    assert_eq!(
        change_of(Change::Updated(endpoint(path))),
        OsChange::Present(device.clone())
    );
    assert_eq!(
        change_of(Change::Removed(path.to_lowercase())),
        OsChange::Gone(device.handle)
    );
}

/// **A change buffered after the list was read is applied after it**: an endpoint the list
/// has that a buffered `Removed` follows ends gone, one a buffered `Added` follows ends
/// present, and a change that only repeats the list changes nothing.
#[test]
fn buffered_changes_apply_in_order_after_the_list() {
    use tutti_midi_device::DeviceTable;
    let (x, y, z) = (r"\\?\a", r"\\?\b", r"\\?\c");
    let mut table = DeviceTable::default();
    for path in [x, y] {
        let OsChange::Present(device) = change_of(Change::Added(endpoint(path))) else {
            unreachable!("an added endpoint is present");
        };
        table.apply(OsChange::Present(device));
    }
    let buffered = vec![
        Ok(Change::Added(endpoint(x))),
        Ok(Change::Removed(y.to_owned())),
        Ok(Change::Added(endpoint(z))),
        Err(Error::WatcherStopped),
        Ok(Change::Removed(x.to_owned())),
    ];
    let (changes, error) = super::watch::replayed(buffered);
    assert_eq!(error, Some(Error::WatcherStopped));
    for change in changes {
        table.apply(change);
    }
    let left: Vec<_> = table.list().map(|d| d.name.clone()).collect();
    assert_eq!(left.len(), 2, "x and z remain, y is gone: {left:?}");
    assert!(
        table
            .list()
            .all(|d| d.id.to_string().contains(x) || d.id.to_string().contains(z))
    );
}

/// **An endpoint speaks the protocol the service and it agreed on, whatever it supports**,
/// and MIDI 1.0 while none is agreed: an endpoint that supports MIDI 2.0 is not in it until
/// they agree. Each supports what it declares.
/// Spec: m2-104/7.1.6.2#2
#[test]
fn an_endpoint_speaks_the_protocol_it_agreed() {
    let agreed2 = described(Some(&declared(true, true, Some(Protocol::Midi2)))).endpoint;
    assert_eq!(agreed2.active().protocol, Protocol::Midi2);
    assert!(agreed2.capabilities().midi1 && agreed2.capabilities().midi2);
    let agreed1 = described(Some(&declared(true, true, Some(Protocol::Midi1)))).endpoint;
    assert_eq!(agreed1.active().protocol, Protocol::Midi1);
    assert!(agreed1.capabilities().midi1 && agreed1.capabilities().midi2);
    let not_agreed = described(Some(&declared(true, true, None))).endpoint;
    assert_eq!(not_agreed.active().protocol, Protocol::Midi1);
    assert!(not_agreed.capabilities().midi1 && not_agreed.capabilities().midi2);
    let only1 = described(Some(&declared(true, false, Some(Protocol::Midi1)))).endpoint;
    assert_eq!(only1.active().protocol, Protocol::Midi1);
    assert!(only1.capabilities().midi1 && !only1.capabilities().midi2);
}

/// **An endpoint the service could not describe is a legacy MIDI 1.0 endpoint**: MIDI 1.0 in
/// use and supported, with no Function Blocks.
#[test]
fn an_endpoint_with_no_declaration_is_described_as_midi1() {
    let described = described(None);
    assert_eq!(described.endpoint, legacy());
    assert_eq!(described.endpoint.active().protocol, Protocol::Midi1);
    assert!(described.endpoint.capabilities().midi1 && !described.endpoint.capabilities().midi2);
    assert_eq!(described.blocks_left_out, 0);
}
