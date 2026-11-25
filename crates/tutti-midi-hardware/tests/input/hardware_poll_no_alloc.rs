//! Regression gate for the **hardware** MIDI input on the audio thread.
//!
//! `tutti-midi-runtime`'s own gate drives `MidiInputNode` through a scripted
//! inlet, which cannot show what the production input does. That is a
//! `HardwareMidiInputs` port: a driver thread stamps each arrival with the
//! frame the engine's block clock puts it on and pushes it into an event
//! inlet; the graph's inlet node holds it until its block and the input node
//! translates it. Reaching it needs this crate, because `tutti-midi-runtime`
//! cannot depend on `tutti-midi-hardware` (the dependency runs the other way).
//!
//! Both halves run inside `assert_no_alloc`: the audio thread (the inlet node,
//! the input node, the block clock's write) **and** the driver's `push`,
//! which runs on a thread the OS owns and must not allocate either. A waiting
//! list that grew, a stamp that boxed, or an event held and dropped on the
//! wrong side would `realloc` or free inside a callback.
//!
//! Each gate runs the nodes in a graph, channel 0's port wired to an event tap
//! whose ring the test drains, so what was carried is counted.

use std::sync::Arc;
use std::time::Duration;
use tutti_buffers::MaxBlock;
use tutti_fixtures::BlockClock;
use tutti_platform::Instant;

use tutti_fixtures::midi;
use tutti_graph::EventInletNode;
use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, EventEdge, EventInputPort, EventOutputPort, EventTapId, EventTapOptions,
    EventTapReader, Executor, Prepare, TapAlign, TapEvent, TransportPosition,
};
use tutti_midi_hardware::{HardwareMidiInputs, InputProducerHandle};
use tutti_midi_runtime::{InputInlets, InputWriters, MidiInputNode};
use tutti_ump::{JrTicks, Message, MidiChannel, Ump, Utility};
use tutti_units::{Frame, SampleRate, Samples};

const RATE: f64 = 48_000.0;
const INPUT: NodeKey = NodeKey(1);

/// A graph holding an input node over the inlets of `inputs`, its channel-0
/// port under an event tap, and the engine's part of the block clock.
struct Rig {
    _editor: Editor,
    exec: Executor,
    clock: BlockClock,
    origin: Instant,
    frames: u64,
    out: EventTapReader,
    drain: Vec<TapEvent>,
    /// Events that reached the tap.
    carried: usize,
}

impl Rig {
    /// `ports` are the inlets of the ports of `inputs` that were opened: they
    /// are added here, each feeding an input node of its own.
    fn new(inputs: &HardwareMidiInputs, ports: Vec<EventInletNode>) -> Self {
        let inlets = inputs.inlets();
        let (mut editor, exec) = Editor::new(Prepare::new(
            SampleRate::of(RATE),
            MaxBlock::of(Samples::new(1024)),
        ));
        // An input node for each port, the first one's channel 0 under the tap.
        for (input, node) in ports.into_iter().enumerate() {
            let input_node = NodeKey(INPUT.0 + 1000 * input as u64);
            editor.insert(input_node, "in", MidiInputNode::new());
            let key = NodeKey(100 + input as u64);
            editor.insert(key, "inlet", node);
            editor
                .connect_events(
                    EventInputPort::new(input_node, 0),
                    EventEdge::Direct(EventOutputPort::new(key, 0)),
                )
                .unwrap();
        }
        let out = editor
            .open_event_tap(
                EventTapId::of(1),
                EventOutputPort::new(INPUT, 0),
                EventTapOptions::new(tutti_graph::EventCapacity::of(1024)).align(TapAlign::Raw),
            )
            .expect("the input node is in the graph");
        editor.commit().expect("commits");
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        Self {
            _editor: editor,
            exec,
            clock,
            origin: Instant::now(),
            frames: 0,
            out,
            drain: vec![
                TapEvent {
                    frame: Frame::ZERO,
                    ump: Ump::from(Message::Utility(Utility::Noop))
                };
                2048
            ],
            carried: 0,
        }
    }

    /// When graph frame `frame` begins.
    fn time_of(&self, frame: u64) -> Instant {
        self.origin + Duration::from_secs_f64(frame as f64 / RATE)
    }

    /// Where the next block begins, in OS time.
    fn next_block(&self) -> Instant {
        self.time_of(self.frames)
    }

    /// One block of `frames`, preceded by the engine's clock reading, and
    /// drain what reached the tap.
    fn run(&mut self, frames: usize) {
        self.clock.write(
            self.next_block(),
            Frame::new(self.frames),
            SampleRate::of(RATE),
        );
        self.exec
            .process(frames, &TransportPosition::default(), &[], &mut []);
        self.frames += frames as u64;
        self.carried += self.out.pop_into(&mut self.drain);
    }
}

fn inputs(capacity: u32) -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(InputInlets::new(
        tutti_graph::EventCapacity::of(capacity),
        Duration::ZERO,
    )))
}

fn note() -> Ump {
    midi::note_on(MidiChannel::FIRST, 60, 0x8000)
}

/// Feed `count` events through a port's producer, as a driver callback
/// thread would, each arriving at `at`.
fn feed(port: &mut InputProducerHandle, count: usize, at: Instant) {
    for _ in 0..count {
        let _ = port.push(note(), at);
    }
}

/// The steady state: events arriving every block, well under the capacity,
/// each placed inside the block that follows.
#[test]
fn hardware_input_is_allocation_free() {
    let inputs = inputs(256);
    let (mut port, inlet) = inputs.open_input_port("Test Input");
    let mut rig = Rig::new(&inputs, vec![inlet]);

    // Warm up: the first block publishes the clock the stamp needs.
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..500 {
            let at = rig.time_of(rig.frames + 100);
            feed(&mut port, 16, at);
            rig.run(512);
        }
    });

    assert!(
        rig.carried > 0,
        "the input must have carried events: an empty one would prove nothing"
    );
}

/// The overflow case: more events pushed than an inlet holds.
///
/// Four ports of 64 events take 256 a block against rings of 64: the driver's
/// push must refuse the rest (counted), and the inlet's waiting list must
/// never grow past its capacity.
#[test]
fn hardware_input_over_capacity_is_allocation_free() {
    let inputs = inputs(64);
    let mut readers = Vec::new();
    let (mut ports, inlets): (Vec<InputProducerHandle>, Vec<EventInletNode>) = (0..4)
        .map(|i| {
            let (writers, input) = InputWriters::new();
            readers.push(input);
            inputs.open_input_port_with(format!("Input {i}"), writers.split().0)
        })
        .unzip();
    let mut rig = Rig::new(&inputs, inlets);
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            let at = rig.time_of(rig.frames + 100);
            for port in &mut ports {
                feed(port, 256, at);
            }
            rig.run(512);
        }
    });
    assert!(
        readers.iter().all(|input| input.inlet.get().lost > 0),
        "the rings were full"
    );
}

/// A block size sweep, since the inlet resolves each event against the block's
/// length.
#[test]
fn hardware_input_across_block_sizes_is_allocation_free() {
    let inputs = inputs(256);
    let (mut port, inlet) = inputs.open_input_port("Test Input");
    let mut rig = Rig::new(&inputs, vec![inlet]);

    const SIZES: [usize; 5] = [64, 128, 256, 512, 1024];
    rig.run(64);
    for frames in SIZES {
        feed(&mut port, 8, rig.time_of(rig.frames + 10));
        rig.run(frames);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            for frames in SIZES {
                feed(&mut port, 8, rig.time_of(rig.frames + 10));
                rig.run(frames);
            }
        }
    });
}

/// Events whose arrival predates the block take the late path: the inlet finds
/// them already past and emits them at the block's first frame, counting them.
/// The steady-state test above never reaches that branch.
#[test]
fn hardware_input_with_stale_timestamps_is_allocation_free() {
    let inputs = inputs(256);
    let (mut port, inlet) = inputs.open_input_port("Test Input");
    let mut rig = Rig::new(&inputs, vec![inlet]);
    rig.run(512);

    // An arrival at frame 0, long before every block that follows.
    let stale = rig.time_of(0);
    feed(&mut port, 16, stale);
    rig.run(512);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            feed(&mut port, 16, stale);
            rig.run(512);
        }
    });
    assert!(rig.carried > 0);
}

/// Before the engine has published a reading there is nothing to place an
/// arrival with: the event is pushed for the next block, and that path
/// allocates nothing either.
#[test]
fn hardware_input_before_the_first_reading_is_allocation_free() {
    let inputs = inputs(256);
    let (mut port, inlet) = inputs.open_input_port("Test Input");
    let mut rig = Rig::new(&inputs, vec![inlet]);
    // Nothing has run, so the clock holds no reading.
    assert_no_alloc::assert_no_alloc(|| {
        feed(&mut port, 16, Instant::now());
    });
    rig.run(512);
    assert_eq!(rig.carried, 16, "pushed for the next block, all of them");
}

/// A port timed by the device's JR Timestamps takes every branch of the JR path
/// without allocating: a clock heard, a timestamp mapped, a timestamp before any
/// clock, a clock restarted by a jump, a clock lost to silence, a late clock, and
/// a timestamp after a silence of two seconds.
#[test]
fn hardware_input_timed_by_jr_is_allocation_free() {
    use tutti_midi_hardware::InputTiming;
    let inputs = inputs(256);
    let (mut port, inlet) = inputs.open_input_port("Test Input");
    let mut rig = Rig::new(&inputs, vec![inlet]);
    rig.run(512);
    inputs
        .set_input_timing(port.port(), InputTiming::JitterReduction)
        .expect("the port is open");
    let jr = |utility| Ump::from(Message::Utility(utility));

    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..100u64 {
            // The block's arrivals, a timestamp before any clock first.
            let at = rig.time_of(rig.frames + 100);
            let _ = port.push(jr(Utility::JrTimestamp(JrTicks::new(40))), at);
            let _ = port.push(note(), at);
            for k in 0..8u16 {
                let at = rig.time_of(rig.frames + 100 + u64::from(k));
                let _ = port.push(jr(Utility::JrClock(JrTicks::new(k * 3_125))), at);
                let _ = port.push(jr(Utility::JrTimestamp(JrTicks::new(k * 3_125 + 20))), at);
                let _ = port.push(note(), at);
            }
            // Late (a repeat) and restarted (a jump).
            let at = rig.time_of(rig.frames + 100);
            let _ = port.push(jr(Utility::JrClock(JrTicks::new(7 * 3_125))), at);
            let _ = port.push(
                jr(Utility::JrClock(JrTicks::new(30_000 + round as u16))),
                at,
            );
            let _ = port.push(jr(Utility::JrTimestamp(JrTicks::new(0))), at);
            let _ = port.push(note(), at);
            if round % 10 == 0 {
                // A timestamp two seconds after the last clock: it is lost.
                let later = at + Duration::from_secs(2);
                let _ = port.push(jr(Utility::JrTimestamp(JrTicks::new(9))), later);
                let _ = port.push(note(), later);
            }
            rig.run(512);
        }
    });
    assert!(rig.carried > 0);
    let info = inputs.port_info(port.port()).unwrap();
    assert!(
        info.jr_restarted.get() > 0 && info.losses.late > 0,
        "{info:?}"
    );
}
