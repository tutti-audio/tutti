//! The endpoints the suite runs against on Windows MIDI Services: the service's two
//! cross-wired diagnostic loopback endpoints, out on A and in on B.

use tutti_midi_device::{DeviceInfo, OsEndpointIdentity};
use tutti_midi_hardware::Devices;
use tutti_winmidi2_ffi::{Loopback, endpoints, ensure_service_available};

/// Loopback A, which what is sent to arrives from at B.
pub(crate) fn output(devices: &Devices) -> DeviceInfo {
    loopback(devices, Loopback::A)
}

/// Loopback B, which what is sent to A arrives at.
pub(crate) fn input(devices: &Devices) -> DeviceInfo {
    loopback(devices, Loopback::B)
}

/// The listing of diagnostic loopback `which`: the service names it by its device id, and
/// the listing carries that id as the device's identity. A missing one is a failure that
/// lists what the service has.
fn loopback(devices: &Devices, which: Loopback) -> DeviceInfo {
    ensure_service_available().expect("the MIDI service is available");
    let listed = endpoints().expect("the service lists its endpoints");
    let path = &listed
        .iter()
        .find(|e| e.loopback == Some(which))
        .unwrap_or_else(|| panic!("the service lists diagnostic loopback {which:?}: {listed:?}"))
        .id;
    devices
        .list()
        .into_iter()
        .find(|d| {
            matches!(
                d.id.identity(),
                OsEndpointIdentity::MidiServices(id) if id.eq_ignore_ascii_case(path)
            )
        })
        .unwrap_or_else(|| panic!("the device list has diagnostic loopback {which:?} ({path})"))
}
