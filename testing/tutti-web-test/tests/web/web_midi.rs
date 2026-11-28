//! The Web MIDI backend over a `navigator.requestMIDIAccess` that a test installs: the
//! three answers a page can get (no Web MIDI, a denial, a grant) and a `statechange` that
//! takes a listed port away. The mock is a plain JavaScript object with the shape the W3C
//! Web MIDI API gives `MIDIAccess` (`inputs`, `outputs`, `onstatechange`) and its ports
//! (`id`, `name`, `state`).

use js_sys::wasm_bindgen::{JsCast, JsValue};
use js_sys::{Array, Function, Reflect};
use tutti_midi_device::DeviceEvent;
use tutti_midi_hardware::{Devices, MidiHardwareError};
use wasm_bindgen_test::wasm_bindgen_test;

/// Replaces the global `navigator` with the object `source` returns.
fn install_navigator(source: &str) -> JsValue {
    let body = format!(
        "const navigator = (function () {{ {source} }})();\n\
         Object.defineProperty(globalThis, 'navigator', {{ value: navigator, configurable: true, writable: true }});\n\
         return navigator;"
    );
    Function::new_no_args(&body)
        .call0(&JsValue::NULL)
        .expect("the mock installs")
}

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).expect("the mock has the key")
}

/// The error a [`Devices::start`] failed with.
async fn start_error(devices: &Devices) -> MidiHardwareError {
    match devices.start().await {
        Ok(_) => panic!("the start resolved"),
        Err(failed) => match failed.cause() {
            MidiHardwareError::WebMidiDenied => MidiHardwareError::WebMidiDenied,
            MidiHardwareError::WebMidiUnavailable => MidiHardwareError::WebMidiUnavailable,
            other => panic!("unexpected cause {other:?}"),
        },
    }
}

/// **A page's three answers reach `Start`, and a `statechange` that disconnects a port is a
/// departure.** A navigator with no `requestMIDIAccess` is `WebMidiUnavailable`; one that
/// rejects with a NotAllowedError is `WebMidiDenied`; one that grants lists its connected
/// input, was asked for SysEx, and a `statechange` after the port's state becomes
/// `"disconnected"` removes it from the list and reports `Removed`.
#[wasm_bindgen_test]
async fn start_resolves_to_the_pages_answer_and_a_statechange_removes_a_port() {
    let executor = tutti_platform::Executor::new();
    let devices = Devices::open(&executor).expect("the watch registers");
    let events = devices.subscribe();
    assert!(devices.list().is_empty(), "nothing is listed before access");

    install_navigator("return {};");
    assert!(matches!(
        start_error(&devices).await,
        MidiHardwareError::WebMidiUnavailable
    ));

    install_navigator(
        "return { requestMIDIAccess() { return Promise.reject({ name: 'NotAllowedError' }); } };",
    );
    assert!(matches!(
        start_error(&devices).await,
        MidiHardwareError::WebMidiDenied
    ));

    let navigator = install_navigator(
        "const port = { id: 'in-1', name: 'Mock Keys', manufacturer: '', state: 'connected', \
                        connection: 'closed', type: 'input' };\n\
         const access = { inputs: new Map([['in-1', port]]), outputs: new Map(), \
                          sysexEnabled: true, onstatechange: null, \
                          fire() { this.onstatechange({ type: 'statechange', port }); } };\n\
         return { requests: [], access, port, \
                  requestMIDIAccess(options) { this.requests.push(options); \
                                               return Promise.resolve(this.access); } };",
    );
    devices.start().await.expect("the grant resolves the start");
    let asked = Array::from(&get(&navigator, "requests"));
    assert_eq!(asked.length(), 1);
    assert_eq!(
        get(&asked.get(0), "sysex"),
        JsValue::TRUE,
        "SysEx is requested"
    );
    let names: Vec<_> = devices.list().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["Mock Keys"]);

    let port = get(&navigator, "port");
    Reflect::set(
        &port,
        &JsValue::from_str("state"),
        &JsValue::from_str("disconnected"),
    )
    .unwrap();
    let fire: Function = get(&get(&navigator, "access"), "fire").unchecked_into();
    fire.call0(&get(&navigator, "access"))
        .expect("the page fires statechange");
    assert!(devices.list().is_empty());
    let seen: Vec<_> = events.pending().collect();
    assert!(
        matches!(&seen[..], [DeviceEvent::Removed(info)] if info.name == "Mock Keys"),
        "{seen:?}"
    );
}
