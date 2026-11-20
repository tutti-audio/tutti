//! Linux loopback over the kernel's `Midi Through` port.
//!
//! Sends UMP out through one endpoint and reads it back through another,
//! checking the message survives the round trip.
//!
//! **`Midi Through`, not VirMIDI.** A virmidi port does not echo: writing to
//! it feeds the raw-MIDI device, not that port's own subscribers, so a
//! send/receive pair on one virmidi port receives nothing. `Midi Through` is
//! the kernel's loopback.
//!
//! The suite needs the `snd-seq` module and a `Midi Through` client
//! (`sudo modprobe snd-seq-dummy`) and fails without them.

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use super::listener::Listener;

use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::{EventCapacity, Offset};
use tutti_midi_device::DeviceInfo;
use tutti_midi_hardware::{MidiHardwareConfig, MidiSession, OpenOutput};
use tutti_midi_runtime::InputInlets;
use tutti_ump::{MidiChannel, NoteKeyEvent, Ump};

/// The loopback endpoint.
fn pick(list: &[DeviceInfo]) -> Option<&DeviceInfo> {
    list.iter().find(|e| e.name.contains("Midi Through"))
}

/// A session with `Midi Through` connected both ways, and the graph that
/// listens to it. The fields drop in order: the output, the graph, then the
/// session.
struct Loopback {
    sink: OpenOutput,
    graph: Listener,
    _io: MidiSession,
}

fn open() -> Loopback {
    // 256 events, each placed 20 ms after it arrived.
    let inlets = InputInlets::new(EventCapacity::of(256), Duration::from_millis(20));
    let mut graph = Listener::over(&inlets);
    let executor = tutti_platform::Executor::new();
    let mut io = MidiSession::open(&executor, Arc::clone(&inlets), MidiHardwareConfig::new())
        .expect("open MIDI");
    let inputs = io.inputs();
    let outputs = io.outputs();
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        panic!("no `Midi Through` port: `sudo modprobe snd-seq-dummy`");
    };
    let tutti_midi_hardware::Connected::Opened(inlet) =
        io.connect_input(&inp.id).expect("connect input")
    else {
        panic!("a new input opens");
    };
    graph.attach(inlet);
    let mut sink = io.connect_output(&out.id).expect("connect output");
    // Whatever was buffered before the test is drained with the probes.
    graph.ready(
        || {
            let _ = sink.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 59, 0x8100))]);
        },
        Duration::from_secs(30),
    );
    Loopback {
        sink,
        graph,
        _io: io,
    }
}

/// What only this process sends: a channel and a run of ten notes derived from
/// the process id, so that a note another process puts into the kernel-wide
/// loopback is told from this test's. Ids are mapped to 96 distinct
/// (channel, run) pairs, so any 96 consecutive process ids never share one. The
/// notes sit above the probe's 59.
struct Marker {
    channel: MidiChannel,
    base: u8,
}

impl Marker {
    fn of_this_process() -> Self {
        let slot = (std::process::id() % 96) as u8;
        Self {
            channel: MidiChannel::masked(slot % 16),
            base: 60 + 10 * (slot / 16),
        }
    }

    /// The notes of `events` that this process sent, as (is_on, note).
    fn mine(&self, events: &[Ump]) -> Vec<(bool, u8)> {
        events
            .iter()
            .filter_map(|ev| match ev.note_key_event()? {
                NoteKeyEvent::On(k) => Some((true, k)),
                NoteKeyEvent::Off(k) => Some((false, k)),
                NoteKeyEvent::Sustain { .. } => None,
            })
            .filter(|(_, k)| {
                let note = k.note().get();
                k.address().channel == self.channel && (self.base..self.base + 10).contains(&note)
            })
            .map(|(on, k)| (on, k.note().get()))
            .collect()
    }
}

/// A hang guard: the waits below return when the events arrive.
const PATIENCE: Duration = Duration::from_secs(30);

/// After the expected events, the loopback is drained this long for extras.
const QUIET: Duration = Duration::from_millis(100);

impl Marker {
    /// Plays until `expected` is what `mine` holds of the arrivals, failing
    /// with what was heard after [`PATIENCE`], then drains [`QUIET`] more so a
    /// duplicate is heard too. Returns everything heard.
    fn hear(&self, graph: &mut Listener, expected: &[(bool, u8)]) -> Vec<Ump> {
        let end = Instant::now() + PATIENCE;
        let mut heard = Vec::new();
        while self.mine(&heard).len() < expected.len() {
            assert!(
                Instant::now() < end,
                "{expected:?} never came back; heard {:?}",
                self.mine(&heard)
            );
            heard.extend(graph.listen(Duration::from_millis(5)));
        }
        heard.extend(graph.listen(QUIET));
        heard
    }
}

/// `Midi Through` is a kernel-wide loopback, so a test running in parallel in
/// another process puts its notes into the stream too: the test asserts on the
/// events of its own [`Marker`] only.
#[test]
fn midi_through_returns_a_note_on_a_note_off_and_a_burst_of_ten_note_ons() {
    let mut lb = open();
    let m = Marker::of_this_process();
    let base = m.base;

    let on = note_on(m.channel, base, 0x8000);
    assert_eq!(lb.sink.queue(&[(Offset::ZERO, on)]).packets(), 1);
    let heard = m.hear(&mut lb.graph, &[(true, base)]);
    assert_eq!(m.mine(&heard), [(true, base)], "the note on came back once");

    let off = note_off(m.channel, base, 0x4000);
    assert_eq!(lb.sink.queue(&[(Offset::ZERO, off)]).packets(), 1);
    let heard = m.hear(&mut lb.graph, &[(false, base)]);
    assert_eq!(
        m.mine(&heard),
        [(false, base)],
        "the note off came back once"
    );

    for n in base..base + 10 {
        let _ = lb
            .sink
            .queue(&[(Offset::ZERO, note_on(m.channel, n, 0xA000))]);
    }
    let burst: Vec<(bool, u8)> = (base..base + 10).map(|n| (true, n)).collect();
    let heard = m.hear(&mut lb.graph, &burst);
    assert_eq!(
        m.mine(&heard),
        burst,
        "all ten of the burst came back, in order"
    );
}

/// A note sent after an input is closed and the same device reopened at once is
/// heard once by the reopened port: closing returns before the old pump has
/// left the sequencer, and the reopen neither fails nor doubles the delivery.
#[test]
fn an_input_reopened_at_once_hears_a_note_once() {
    let inlets = InputInlets::new(EventCapacity::of(256), Duration::from_millis(20));
    let executor = tutti_platform::Executor::new();
    let mut io = MidiSession::open(&executor, Arc::clone(&inlets), MidiHardwareConfig::new())
        .expect("open MIDI");
    let (inputs, outputs) = (io.inputs(), io.outputs());
    let (Some(inp), Some(out)) = (pick(&inputs), pick(&outputs)) else {
        panic!("no `Midi Through` port: `sudo modprobe snd-seq-dummy`");
    };
    let (input_id, output_id) = (inp.id.clone(), out.id.clone());
    let mut sink = io.connect_output(&output_id).expect("connect output");

    let first = io.connect_input(&input_id).expect("connect input");
    assert!(matches!(first, tutti_midi_hardware::Connected::Opened(_)));
    drop(first);
    assert!(io.disconnect_input(&input_id), "the input was open");
    let tutti_midi_hardware::Connected::Opened(inlet) =
        io.connect_input(&input_id).expect("reconnects at once")
    else {
        panic!("a closed input opens anew");
    };
    let mut graph = Listener::over(&inlets);
    graph.attach(inlet);
    graph.ready(
        || {
            let _ = sink.queue(&[(Offset::ZERO, note_on(MidiChannel::of(1), 59, 0x8100))]);
        },
        PATIENCE,
    );

    let m = Marker::of_this_process();
    let on = note_on(m.channel, m.base, 0x8000);
    assert_eq!(sink.queue(&[(Offset::ZERO, on)]).packets(), 1);
    let heard = m.hear(&mut graph, &[(true, m.base)]);
    assert_eq!(m.mine(&heard), [(true, m.base)], "heard once");
}
