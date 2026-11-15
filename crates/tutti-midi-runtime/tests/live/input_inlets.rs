//! `InputInlets` and `DeviceInput`: a device's thread places
//! each arrival on a frame with a `ClockMap` fed the engine's block clock, and
//! pushes it into its own event inlet. Figures are analytic: a reading says OS
//! time `T` is frame `F`, so an arrival `d` after `T` is frame `F + d * rate`,
//! plus the lead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_fixtures::midi;
use tutti_fixtures::{BlockClock, EventRig};
use tutti_graph::NodeKey;
use tutti_midi_runtime::{DeviceInput, InputInlets};
use tutti_types::{Frame, SampleRate};
use tutti_ump::{MidiChannel, Ump};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;

fn note(n: u8) -> Ump {
    midi::note_on(MidiChannel::FIRST, n, 0x8000)
}

/// One device's inlet in a graph that logs what it emits, played in blocks.
struct Rig {
    rig: EventRig,
    input: DeviceInput,
}

impl Rig {
    fn new(inlets: &InputInlets) -> Self {
        let (input, node) = inlets.open();
        let mut rig = EventRig::new(RATE, BLOCK, 1);
        rig.editor.insert(NodeKey(1), "inlet", node);
        rig.feed(NodeKey(1), 0, 0);
        rig.commit();
        Self { rig, input }
    }

    fn run(&mut self, blocks: usize) {
        for _ in 0..blocks {
            self.rig.block(BLOCK);
        }
    }

    fn take(&self) -> Vec<(u64, Ump)> {
        self.rig
            .take()
            .into_iter()
            .map(|(_, frame, ump)| (frame, ump))
            .collect()
    }
}

/// `d` after `t`, in frames at `RATE`.
fn after(t: Instant, frames: u64) -> Instant {
    t + Duration::from_secs_f64(frames as f64 / RATE)
}

fn inlets(lead: Duration) -> Arc<InputInlets> {
    InputInlets::new(tutti_graph::EventCapacity::of(64), lead)
}

/// **An arrival is the frame the reading puts it on, plus the lead**: the
/// engine says OS time `t0` is frame 1000; arrivals 100, 250 and 700 frames
/// after `t0` sound on frames 1100, 1250 and 1700, and with a lead of 480
/// frames (10 ms), 1580, 1730 and 2180: the same spacing, the same lead.
#[test]
fn an_arrival_sounds_on_the_frame_the_clock_puts_it_on_plus_the_lead() {
    for lead_frames in [0u64, 480] {
        let inlets = inlets(Duration::from_secs_f64(lead_frames as f64 / RATE));
        let mut rig = Rig::new(&inlets);
        let t0 = Instant::now();
        let mut clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        clock.write(t0, Frame::new(1000), SampleRate::of(RATE));
        for (n, d) in [(60u8, 100u64), (61, 250), (62, 700)] {
            assert!(rig.input.push(note(n), after(t0, d)));
        }
        rig.run(6);
        let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
        assert_eq!(
            frames,
            vec![1100 + lead_frames, 1250 + lead_frames, 1700 + lead_frames],
            "lead {lead_frames}"
        );
    }
}

/// **The map follows the clock across readings**: a second reading puts the
/// OS time `t0 + 10 ms` at frame 1480 (480 frames on, on the nominal rate), and
/// an arrival after it is placed from it.
#[test]
fn a_later_reading_places_later_arrivals() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame::new(1000), SampleRate::of(RATE));
    assert!(rig.input.push(note(60), after(t0, 100)));
    clock.write(after(t0, 480), Frame::new(1480), SampleRate::of(RATE));
    assert!(rig.input.push(note(61), after(t0, 580)));
    rig.run(6);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
    assert_eq!(frames, vec![1100, 1580]);
}

/// **Before any reading an event is pushed for the next block**: the first
/// frame of the block that finds it, as `Stamp::NextBlock` is.
#[test]
fn before_a_reading_an_event_is_for_the_next_block() {
    let inlets = inlets(Duration::from_millis(5));
    let mut rig = Rig::new(&inlets);
    assert!(rig.input.push(note(60), Instant::now()));
    rig.run(2);
    assert_eq!(rig.take(), vec![(0, note(60))]);
}

/// **An arrival the clock puts before frame 0 is pushed for the next block**,
/// not wrapped to a huge frame.
#[test]
fn an_arrival_before_frame_zero_is_for_the_next_block() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame::new(100), SampleRate::of(RATE));
    // 5,000 frames before the reading: frame -4,900.
    let early = t0 - Duration::from_secs_f64(5000.0 / RATE);
    assert!(rig.input.push(note(60), early));
    rig.run(2);
    assert_eq!(rig.take(), vec![(0, note(60))]);
}

/// **A restarted clock re-anchors the map**: an engine that restarts begins its
/// frames at 0 again, and the map, which refuses a reading that goes back, is
/// built afresh from it. Arrivals after the restart are placed from the new
/// clock, not from the old anchor.
#[test]
fn a_restarted_clock_places_arrivals_from_the_new_anchor() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut first = BlockClock::new();
    inlets.set_clock(Some(first.reader()));
    first.write(t0, Frame::new(1000), SampleRate::of(RATE));
    assert!(rig.input.push(note(60), after(t0, 480)));

    // The engine restarts a second later; its frames begin at 0 again.
    let t1 = t0 + Duration::from_secs(1);
    let mut second = BlockClock::new();
    inlets.set_clock(Some(second.reader()));
    second.write(t1, Frame::new(100), SampleRate::of(RATE));
    assert!(rig.input.push(note(61), after(t1, 480)));
    rig.run(6);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
    assert_eq!(frames, vec![580, 1480]);
}

/// **An arrival is a moment at the reading's rate**: a clock that counts
/// frames at 96 kHz puts an arrival 10 ms after its reading 960 frames of its
/// own on, and the graph (48 kHz) plays it 480 frames on from the reading's
/// instant.
#[test]
fn a_reading_at_another_rate_is_rescaled_by_the_inlet() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame::new(2000), SampleRate::SR_96K);
    assert!(rig.input.push(note(60), t0 + Duration::from_millis(10)));
    rig.run(6);
    // 2,000 frames at 96 kHz is 1,000 at 48 kHz; 10 ms is 480 more.
    assert_eq!(rig.take(), vec![(1480, note(60))]);
}

/// **Each open is an input of its own, and ids are never reused**: ids count
/// up from 1 in the order inputs opened, and an input that dropped leaves no
/// id for the next, so a stale id names nothing; there is no cap.
#[test]
fn every_open_is_a_new_id_and_none_is_reused() {
    let inlets = inlets(Duration::ZERO);
    let ids: Vec<u64> = (0..100)
        .map(|_| {
            let (input, _node) = inlets.open();
            input.id().get()
        })
        .collect();
    assert_eq!(ids, (1..=100).collect::<Vec<u64>>());
}

/// **Two inputs share the clock and keep their own maps**: both read the one
/// block clock, and one input's arrivals do not move the other's.
#[test]
fn two_inputs_share_the_clock_and_keep_their_own_events() {
    let inlets = inlets(Duration::ZERO);
    let mut a = Rig::new(&inlets);
    let mut b = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame::new(1000), SampleRate::of(RATE));
    assert!(a.input.push(note(60), after(t0, 100)));
    assert!(b.input.push(note(61), after(t0, 300)));
    a.run(6);
    b.run(6);
    assert_eq!(a.take(), vec![(1100, note(60))]);
    assert_eq!(b.take(), vec![(1300, note(61))]);
}

/// **The counts are readable from the control side while the thread pushes**:
/// a push into a full inlet is refused and counted as lost.
#[test]
fn a_full_inlet_refuses_and_counts() {
    let inlets = InputInlets::new(tutti_graph::EventCapacity::of(2), Duration::ZERO);
    let (mut device, _node) = inlets.open();
    let counts = device.counts();
    let pushed: Vec<bool> = (0..4)
        .map(|i| device.push(note(60 + i), Instant::now()))
        .collect();
    assert_eq!(pushed, vec![true, true, false, false]);
    assert_eq!(counts.losses().lost, 2);
}
