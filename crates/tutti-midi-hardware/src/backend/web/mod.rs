//! The web backend: Web MIDI (W3C Web MIDI API), the MIDI 1.0 API a browser gives a page.
//!
//! This is the one module that names the browser's bindings (`web-sys`, `js-sys`,
//! `wasm-bindgen`); what the backend decides is [`web_port`](super::web_port), which runs on
//! every platform.
//!
//! - **Start** is the permission request: `navigator.requestMIDIAccess({ sysex: true })`
//!   (SysEx is always requested, as every session uses it for MIDI-CI and the Identity
//!   Request). A denial, a browser with no Web MIDI and any other failure resolve the
//!   [`Start`] with a named [`MidiHardwareError`]. One request serves the page: the first
//!   [`Start`] makes it, and a [`Start`] after a failed one asks again.
//! - **The list** is the `MIDIAccess`'s input and output maps, each connected port a MIDI
//!   1.0 endpoint named by `MIDIPort.id`. Before the access is granted the list is empty,
//!   and the watch is seeded when it is.
//! - **The watch** looks at the maps again on each `statechange` of the `MIDIAccess` and
//!   reports the difference.
//! - **Opening a port** is [`MidiHardwareError::Unsupported`]: the translation between MIDI
//!   1.0 bytes and UMP words is not here yet.
//! - **Threads.** The `MIDIAccess` and the callbacks the browser calls back live in the
//!   thread-local table of the thread that asked for access ([`page`]); a call on another
//!   thread finds none, and [`MidiHardwareError::WebMidiOffPage`] says so.

mod page;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tutti_midi_device::{EndpointId, OsDevice};
use tutti_rt::Mutex;

use super::hub::{Registry, StartGate};
use super::legacy_port::ports::Side;
use super::web_port::ports::{devices, endpoints};
use crate::InputProducerHandle;
use crate::capability::OsEndpoint;
use crate::devices::ChangeSink;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{MidiHardwareError, Result};
use crate::start::{Start, Started};

/// Why a port cannot be opened yet.
const OPEN_REASON: &str = "opening a Web MIDI port is not supported yet";

/// What the page shares: where the request stands, and the watches registered with it.
/// One per page, as the `MIDIAccess` is.
struct Hub {
    gate: Arc<StartGate>,
    registry: Arc<Registry>,
    /// Whether the `MIDIAccess` is granted.
    ready: AtomicBool,
    /// The generation last requested, and the request under way, which keeps its callbacks.
    asked: Mutex<(Option<u64>, Option<page::Request>)>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub {
        gate: Arc::new(StartGate::new()),
        registry: Arc::new(Registry::new()),
        ready: AtomicBool::new(false),
        asked: Mutex::new((None, None)),
    })
}

/// The page's callbacks into the hub.
const SETTLED: page::Settled = page::Settled {
    ready,
    changed,
    failed,
};

fn ready() {
    let hub = hub();
    hub.ready.store(true, Ordering::Release);
    hub.registry.look_all();
    hub.gate.publish_ready().wake();
}

fn changed() {
    hub().registry.look_all();
}

fn failed(error: MidiHardwareError) {
    hub().gate.publish_failed(Arc::new(error), None).wake();
}

impl Hub {
    /// Makes the request the gate wants and has not had made.
    fn ensure_requested(&self) {
        let mut asked = self.asked.lock();
        let Some((generation, _)) = self.gate.wanted(asked.0) else {
            return;
        };
        asked.0 = Some(generation);
        match page::request(SETTLED) {
            Ok(request) => asked.1 = Some(request),
            Err(error) => {
                drop(asked);
                failed(error);
            }
        }
    }
}

/// The page's devices now: empty on a thread with no `MIDIAccess`.
fn snapshot() -> Vec<OsDevice> {
    page::list()
        .map(|(inputs, outputs)| devices(&inputs, &outputs))
        .unwrap_or_default()
}

/// The `side` endpoints now. Empty until the access is granted; where it is granted and this
/// thread does not hold it, [`MidiHardwareError::WebMidiOffPage`].
fn listing(side: Side) -> Result<Vec<OsEndpoint>> {
    match page::list() {
        Some((inputs, outputs)) => Ok(endpoints(
            side,
            match side {
                Side::Input => &inputs,
                Side::Output => &outputs,
            },
        )),
        None if hub().ready.load(Ordering::Acquire) => Err(MidiHardwareError::WebMidiOffPage),
        None => Ok(Vec::new()),
    }
}

/// The Web MIDI [`MidiEndpoints`] implementation: a handle to the page's one hub.
#[derive(Debug)]
pub(super) struct WebEndpoints;

impl MidiEndpoints for WebEndpoints {
    fn start(&self) -> Start {
        let hub = hub();
        let (round, wakers) = hub.gate.begin(hub.ready.load(Ordering::Acquire));
        wakers.wake();
        hub.ensure_requested();
        hub.gate.start(round)
    }

    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        let hub = hub();
        if hub.ready.load(Ordering::Acquire) && !page::on_page() {
            return Err(MidiHardwareError::WebMidiOffPage);
        }
        Ok(hub
            .registry
            .register(sink, snapshot, || hub.ready.load(Ordering::Acquire)))
    }

    fn inputs(&self) -> Result<Vec<OsEndpoint>> {
        listing(Side::Input)
    }

    fn outputs(&self) -> Result<Vec<OsEndpoint>> {
        listing(Side::Output)
    }

    fn open_input(
        &self,
        _started: Started,
        _id: EndpointId,
        _producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(MidiHardwareError::Unsupported(OPEN_REASON))
    }

    fn open_output(&self, _started: Started, _id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        Err(MidiHardwareError::Unsupported(OPEN_REASON))
    }
}
