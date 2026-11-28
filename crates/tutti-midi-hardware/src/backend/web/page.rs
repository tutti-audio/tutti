//! The page thread's side of Web MIDI: the one file that names the browser's bindings.
//!
//! A `MIDIAccess` and the closures the browser calls back are JavaScript values, which are
//! neither `Send` nor `Sync`, so none sits in a type that crosses threads. They sit in a
//! [`Table`] in a `thread_local!` of the thread that asked for access (the page's), under
//! the ids the table minted, and the rest of the crate holds ids. A call that finds no
//! entry (another thread's table has none) gets `None`, never a panic. An id released
//! from any thread goes to [`ORPHANS`], and the page thread drops its entry at the next
//! callback or request.
//!
//! The `requestMIDIAccess` promise is bridged by `then` callbacks, not a spawned
//! future: the fulfilment callback stores the `MIDIAccess`, listens for `statechange` and
//! tells its owner it is [ready](Settled::ready); the rejection callback names the
//! failure. Each reports through plain functions of the owner, so what the owner holds
//! stays `Send`.

use std::cell::RefCell;

use js_sys::Reflect;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MidiAccess, MidiOptions, MidiPort, MidiPortDeviceState, Navigator};

use crate::backend::web_port::failure::from_dom_exception;
use crate::backend::web_port::ports::PortSnapshot;
use crate::backend::web_port::table::{Orphans, Table};
use crate::error::MidiHardwareError;

/// What the page thread keeps for a JavaScript value.
enum Entry {
    Access(MidiAccess),
    /// A closure the browser calls back, held for as long as the browser may.
    Callback {
        _closure: Closure<dyn FnMut(JsValue)>,
    },
}

#[derive(Default)]
struct Page {
    table: Table<Entry>,
    /// The id of this thread's `MIDIAccess`, once it is granted.
    access: Option<u32>,
}

thread_local! {
    static PAGE: RefCell<Page> = RefCell::new(Page::default());
}

/// The ids released, for the thread of their table to sweep.
static ORPHANS: Orphans = Orphans::new();

/// Drops the entries released since the last sweep. Runs at the start of every callback
/// and request, never inside a callback whose own closure it could drop: a released id
/// is a closure that has already run.
fn sweep() {
    // Dropping a `Closure` or a `MIDIAccess` handle calls nothing back into Rust, so the
    // entries drop under the borrow. `try_with`: a thread that is ending has no table.
    let _ = PAGE.try_with(|page| page.borrow_mut().table.sweep(&ORPHANS));
}

/// What the page tells its owner, as plain functions.
#[derive(Clone, Copy)]
pub(super) struct Settled {
    /// The `MIDIAccess` is granted and in this thread's table.
    pub ready: fn(),
    /// A `statechange` fired: a port came, went or changed.
    pub changed: fn(),
    /// The request failed.
    pub failed: fn(MidiHardwareError),
}

/// A request under way: the ids of its two callbacks. Dropping it releases them for the
/// next sweep, so a callback is never dropped while it runs.
#[derive(Debug)]
pub(super) struct Request {
    ids: [u32; 2],
}

impl Drop for Request {
    fn drop(&mut self) {
        for id in self.ids {
            ORPHANS.push(id);
        }
    }
}

/// The failure a rejected promise or a thrown call is: the DOMException's name.
fn failure_of(error: &JsValue) -> MidiHardwareError {
    let name = Reflect::get(error, &JsValue::from_str("name"))
        .ok()
        .and_then(|name| name.as_string())
        .unwrap_or_else(|| "unnamed exception".to_owned());
    from_dom_exception(&name)
}

/// The page's `navigator`, when it has `requestMIDIAccess`.
fn navigator() -> Result<Navigator, MidiHardwareError> {
    let navigator = Reflect::get(&js_sys::global(), &JsValue::from_str("navigator"))
        .ok()
        .filter(JsValue::is_object)
        .ok_or(MidiHardwareError::WebMidiUnavailable)?;
    let has_request = Reflect::get(&navigator, &JsValue::from_str("requestMIDIAccess"))
        .is_ok_and(|request| request.is_function());
    if has_request {
        Ok(navigator.unchecked_into())
    } else {
        Err(MidiHardwareError::WebMidiUnavailable)
    }
}

/// Asks the browser for MIDI access, with SysEx (every session uses it for MIDI-CI), and
/// reports the outcome through `settled` from a browser callback on this thread.
///
/// # Errors
///
/// [`MidiHardwareError::WebMidiUnavailable`] when the page has no `requestMIDIAccess`, or
/// the failure the call itself threw.
pub(super) fn request(settled: Settled) -> Result<Request, MidiHardwareError> {
    sweep();
    let navigator = navigator()?;
    let options = MidiOptions::new();
    options.set_sysex(true);
    let promise = navigator
        .request_midi_access_with_options(&options)
        .map_err(|error| failure_of(&error))?;

    let granted = Closure::once(move |access: JsValue| {
        sweep();
        grant(access.unchecked_into(), settled);
    });
    let refused = Closure::once(move |error: JsValue| {
        sweep();
        (settled.failed)(failure_of(&error));
    });
    let _ = promise.then2(&granted, &refused);
    let ids = PAGE.with(|page| {
        let mut page = page.borrow_mut();
        [
            page.table.insert(Entry::Callback { _closure: granted }),
            page.table.insert(Entry::Callback { _closure: refused }),
        ]
    });
    Ok(Request { ids })
}

/// Keeps `access` in this thread's table, listens for its `statechange`, and says it is ready.
fn grant(access: MidiAccess, settled: Settled) {
    let changed = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        sweep();
        (settled.changed)();
    });
    access.set_onstatechange(Some(changed.as_ref().unchecked_ref()));
    PAGE.with(|page| {
        let mut page = page.borrow_mut();
        let id = page.table.insert(Entry::Access(access));
        page.access = Some(id);
        page.table.insert(Entry::Callback { _closure: changed });
    });
    (settled.ready)();
}

/// This thread's `MIDIAccess`, if it holds one.
fn access() -> Option<MidiAccess> {
    PAGE.with(|page| {
        let page = page.borrow();
        match page.table.get(page.access?) {
            Some(Entry::Access(access)) => Some(access.clone()),
            _ => None,
        }
    })
}

/// Whether this thread holds the `MIDIAccess`: the page thread, once the request resolved.
pub(super) fn on_page() -> bool {
    access().is_some()
}

/// The ports of a `MIDIInputMap` or `MIDIOutputMap`, as the page reads them.
fn snapshots(values: &js_sys::Iterator) -> Vec<PortSnapshot> {
    let Ok(Some(ports)) = js_sys::try_iter(values) else {
        return Vec::new();
    };
    ports
        .filter_map(Result::ok)
        .map(|port| {
            let port: MidiPort = port.unchecked_into();
            PortSnapshot {
                id: port.id(),
                name: port.name(),
                connected: port.state() == MidiPortDeviceState::Connected,
            }
        })
        .collect()
}

/// The input and output ports the page lists now; `None` on a thread with no `MIDIAccess`.
pub(super) fn list() -> Option<(Vec<PortSnapshot>, Vec<PortSnapshot>)> {
    let access = access()?;
    Some((
        snapshots(&access.inputs().values()),
        snapshots(&access.outputs().values()),
    ))
}
