use super::watch::change_of;
use super::*;
use tutti_midi_device::{OsChange, PortDirection};
use tutti_winmidi2_ffi::Change;

fn endpoint(id: &str) -> Endpoint {
    Endpoint {
        id: id.to_owned(),
        name: "Loopback".to_owned(),
        loopback: None,
    }
}

/// **The id is FNV-1a's 64-bit digest of the path**: its published vector for "a" (and for
/// the empty input, which is the offset), whatever the case of the path.
#[test]
fn an_endpoint_id_is_the_fnv1a_digest_of_the_lowercased_path() {
    assert_eq!(endpoint_id("a").raw(), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(endpoint_id("A"), endpoint_id("a"));
    assert_eq!(endpoint_id("").raw(), 0xcbf2_9ce4_8422_2325);
}

/// **Two endpoints have two ids, and an endpoint has the same id in every listing.**
#[test]
fn distinct_paths_have_distinct_ids() {
    let (a, b) = (r"\\?\swd#midisrv#a", r"\\?\swd#midisrv#b");
    assert_ne!(endpoint_id(a), endpoint_id(b));
    assert_eq!(os_endpoint(&endpoint(a)).id, os_endpoint(&endpoint(a)).id);
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
