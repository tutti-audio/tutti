use std::sync::Arc;

use tutti_midi_device::{DeviceEvent, OsEndpointIdentity};

use super::failure::from_dom_exception;
use super::ports::{PortSnapshot, devices, endpoint_id, endpoints};
use super::table::{Orphans, Table};
use crate::backend::hub::Registry;
use crate::backend::side::Side;
use crate::error::MidiHardwareError;

fn port(id: &str, name: Option<&str>, connected: bool) -> PortSnapshot {
    PortSnapshot {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        connected,
    }
}

/// **An id names an entry only in the table that minted it**: two tables never share an
/// id, so a call on another thread, whose table is another, finds nothing.
#[test]
fn an_id_is_found_only_in_the_table_that_holds_it() {
    let mut page = Table::default();
    let mut other = Table::default();
    let id = page.insert("access");
    let theirs = other.insert("elsewhere");
    assert_ne!(id, theirs);
    assert_eq!(page.get(id), Some(&"access"));
    assert_eq!(other.get(id), None);
    assert_eq!(page.get(theirs), None);
}

/// **Two ports with one name are told apart by a number**, as two ALSA or winmm ports are,
/// and the ids stay each port's own.
#[test]
fn ports_with_one_name_are_shown_under_distinct_names() {
    let listed = endpoints(
        Side::Input,
        &[
            port("a", Some("Pads"), true),
            port("b", Some("Pads"), true),
            port("c", Some("Keys"), true),
        ],
    );
    let names: Vec<_> = listed.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Pads", "Pads 2", "Keys"]);
    assert_ne!(listed[0].id, listed[1].id);
}

/// **An id released off the page thread is dropped by the page thread's sweep**, and an id
/// that belongs to another table stays queued for it.
#[test]
fn a_released_id_is_swept_by_the_table_that_holds_it_and_no_other() {
    let mut page = Table::default();
    let mut other = Table::default();
    let kept = page.insert(1_u8);
    let released = page.insert(2_u8);
    let theirs = other.insert(3_u8);
    let orphans = Arc::new(Orphans::new());
    let releaser = Arc::clone(&orphans);
    std::thread::spawn(move || {
        releaser.push(released);
        releaser.push(theirs);
    })
    .join()
    .unwrap();

    assert_eq!(page.sweep(&orphans), 1);
    assert_eq!(page.get(released), None);
    assert_eq!(page.get(kept), Some(&1));

    assert_eq!(
        other.get(theirs),
        Some(&3),
        "the other table's id waits for its sweep"
    );
    assert_eq!(other.sweep(&orphans), 1);
    assert_eq!(other.get(theirs), None);
    assert_eq!(page.sweep(&orphans), 0, "nothing is left queued");
}

/// **A `requestMIDIAccess` rejection is named by its DOMException**: the Web MIDI
/// API's NotAllowedError (and the Permissions API's SecurityError) is a denial,
/// NotSupportedError is no Web MIDI, and InvalidStateError, AbortError or any
/// other name is a failure that keeps its name.
#[test]
fn a_dom_exception_name_maps_to_its_web_midi_error() {
    assert!(matches!(
        from_dom_exception("NotAllowedError"),
        MidiHardwareError::WebMidiDenied
    ));
    assert!(matches!(
        from_dom_exception("SecurityError"),
        MidiHardwareError::WebMidiDenied
    ));
    assert!(matches!(
        from_dom_exception("NotSupportedError"),
        MidiHardwareError::WebMidiUnavailable
    ));
    for name in ["InvalidStateError", "AbortError", "SomethingNew"] {
        let error = from_dom_exception(name);
        assert!(
            matches!(&error, MidiHardwareError::WebMidiFailed { name: n } if n == name),
            "{error:?}"
        );
        assert!(error.to_string().ends_with(name), "{error}");
    }
}

/// **A port is listed only while its state is `connected`** (W3C Web MIDI API,
/// `MIDIPortDeviceState`: a disconnected device should not appear in the maps), as a
/// MIDI 1.0 port named by `MIDIPort.id`, shown by its name or else its id.
#[test]
fn only_connected_ports_are_listed_as_midi_1_ports_named_by_their_id() {
    let listed = endpoints(
        Side::Input,
        &[
            port("a", Some("Keystep"), true),
            port("b", Some("Gone"), false),
            port("c", None, true),
            port("d", Some("  "), true),
            port("a", Some("Twin"), true),
        ],
    );
    let names: Vec<_> = listed.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["Keystep", "c", "d"],
        "the repeated id is listed once"
    );
    assert_eq!(
        listed[0].identity,
        OsEndpointIdentity::WebMidi("a".to_owned())
    );
    assert_eq!(listed[0].capability, crate::capability::legacy());
    assert_eq!(listed[0].blocks_left_out, 0);
}

/// **An endpoint id differs between ports, and between an input and an output of one
/// id string.**
#[test]
fn an_endpoint_id_is_distinct_per_port_and_per_direction() {
    assert_ne!(endpoint_id(Side::Input, "x"), endpoint_id(Side::Input, "y"));
    assert_ne!(
        endpoint_id(Side::Input, "x"),
        endpoint_id(Side::Output, "x")
    );
    let listed = devices(&[port("x", Some("A"), true)], &[port("x", Some("A"), true)]);
    assert_eq!(listed.len(), 2, "an input and an output are two devices");
}

/// The page's ports, which a test changes as a browser's `MIDIAccess` does.
static PORTS: tutti_rt::Mutex<Vec<PortSnapshot>> = tutti_rt::Mutex::new(Vec::new());

fn present() -> Vec<tutti_midi_device::OsDevice> {
    devices(&PORTS.lock(), &[])
}

/// **A `statechange` that disconnects a port is a departure, and one that connects a port
/// is an arrival**: the watch looks again at the maps, and only the difference reaches
/// the device list's subscribers.
#[test]
fn a_statechange_looks_again_and_reports_each_departure_and_arrival() {
    let registry = Arc::new(Registry::new());
    *PORTS.lock() = vec![port("k", Some("Keys"), true), port("p", Some("Pads"), true)];
    let watching = Arc::clone(&registry);
    let devices = crate::Devices::open_over(
        &tutti_platform::Executor::new(),
        Box::new(crate::testing::Stub::new(move |sink| {
            Ok(watching.register(sink, present, || true))
        })),
    )
    .unwrap();
    let events = devices.subscribe();
    assert_eq!(events.initial().len(), 2);

    PORTS.lock()[1].connected = false;
    registry.look_all();
    let seen: Vec<_> = events.pending().collect();
    assert!(
        matches!(&seen[..], [DeviceEvent::Removed(info)] if info.name == "Pads"),
        "{seen:?}"
    );

    PORTS.lock()[1].connected = true;
    registry.look_all();
    let seen: Vec<_> = events.pending().collect();
    assert!(
        matches!(&seen[..], [DeviceEvent::Added(info)] if info.name == "Pads"),
        "{seen:?}"
    );
}
