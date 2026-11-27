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
