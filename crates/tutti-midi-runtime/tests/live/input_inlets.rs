//! `InputInlets` and `DeviceInput`: a device's thread places
//! each arrival on a frame with a `ClockMap` fed the engine's block clock, and
//! pushes it into its own event inlet. Figures are analytic: a reading says OS
//! time `T` is frame `F`, so an arrival `d` after `T` is frame `F + d * rate`,
//! plus the lead.

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_fixtures::midi;
use tutti_fixtures::{BlockClock, EventRig};
use tutti_graph::NodeKey;
use tutti_midi_runtime::{DeviceInput, InputInlets, InputWriters};
use tutti_rt::{Counter, Gauge};
use tutti_ump::{MidiChannel, Ump};
use tutti_units::Losses;
use tutti_units::{Frame, Resyncs, SampleRate, Seconds};

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
            assert!(rig.input.push(note(n), after(t0, d)).is_ok());
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

/// **A lead set after the inlets were built is the lead each arrival gets**: an
/// arrival 100 frames after `t0` sounds on frame 1100 under a zero lead and,
/// once the lead is set to 10 ms (480 frames), on frame 1580.
#[test]
fn a_lead_set_later_applies_to_the_next_arrival() {
    let inlets = inlets(Duration::ZERO);
    let mut rig = Rig::new(&inlets);
    let t0 = Instant::now();
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    clock.write(t0, Frame::new(1000), SampleRate::of(RATE));
    assert!(rig.input.push(note(60), after(t0, 100)).is_ok());
    inlets.set_lead(Duration::from_secs_f64(480.0 / RATE));
    assert!(rig.input.push(note(61), after(t0, 100)).is_ok());
    rig.run(6);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.0).collect();
    assert_eq!(frames, vec![1100, 1580]);
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
    assert!(rig.input.push(note(60), after(t0, 100)).is_ok());
    clock.write(after(t0, 480), Frame::new(1480), SampleRate::of(RATE));
    assert!(rig.input.push(note(61), after(t0, 580)).is_ok());
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
    assert!(rig.input.push(note(60), Instant::now()).is_ok());
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
    assert!(rig.input.push(note(60), early).is_ok());
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
    assert!(rig.input.push(note(60), after(t0, 480)).is_ok());

    // The engine restarts a second later; its frames begin at 0 again.
    let t1 = t0 + Duration::from_secs(1);
    let mut second = BlockClock::new();
    inlets.set_clock(Some(second.reader()));
    second.write(t1, Frame::new(100), SampleRate::of(RATE));
    assert!(rig.input.push(note(61), after(t1, 480)).is_ok());
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
    assert!(
        rig.input
            .push(note(60), t0 + Duration::from_millis(10))
            .is_ok()
    );
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
    assert!(a.input.push(note(60), after(t0, 100)).is_ok());
    assert!(b.input.push(note(61), after(t0, 300)).is_ok());
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
        .map(|i| device.push(note(60 + i), Instant::now()).is_ok())
        .collect();
    assert_eq!(pushed, vec![true, true, false, false]);
    assert_eq!(counts.losses().lost, 2);
}

/// **An input's lost count is its own cell and carries across a reopened
/// input**: a push the ring refuses raises it, the count survives the device
/// dropping, and the next connection on the same cell keeps adding.
#[test]
fn the_lost_count_carries_across_a_reopened_input() {
    let inlets = InputInlets::new(tutti_graph::EventCapacity::of(2), Duration::ZERO);
    let (writers, readers) = InputWriters::new();
    let (mut device, _node) = inlets.open_with(writers.split().0);
    assert_eq!(readers.lost.get(), Losses::NONE);
    for i in 0..4 {
        let _ = device.push(note(60 + i), Instant::now());
    }
    assert_eq!(
        readers.lost.get(),
        Losses::lost(2),
        "the two pushes the ring refused"
    );
    drop(device);
    assert_eq!(
        readers.lost.get(),
        Losses::lost(2),
        "the count outlives the input"
    );
    let again = InputWriters::reopen(&readers).expect("the input dropped");
    let (mut device, _node) = inlets.open_with(again.split().0);
    for i in 0..4 {
        let _ = device.push(note(60 + i), Instant::now());
    }
    assert_eq!(
        readers.lost.get(),
        Losses::lost(4),
        "the reopened input keeps counting"
    );
}

/// **A push into an inlet whose node is gone is not a loss**: it is refused as
/// `Disconnected` and the input's `lost` cell stays at zero, where a full ring
/// raises it.
#[test]
fn a_push_after_the_node_is_gone_is_disconnected_and_not_lost() {
    let inlets = inlets(Duration::ZERO);
    let (writers, readers) = InputWriters::new();
    let (mut device, node) = inlets.open_with(writers.split().0);
    drop(node);
    assert_eq!(
        device.push(note(60), Instant::now()),
        Err(tutti_rt::PushError::Disconnected)
    );
    assert_eq!(readers.lost.get(), Losses::NONE);
}

/// A device whose clock the test writes, built on the reading "frame 1000 at
/// `t0`": the first map.
struct Timed {
    device: DeviceInput,
    timing: Gauge<Seconds>,
    resyncs: Counter<Resyncs>,
    clock: BlockClock,
    t0: Instant,
}

impl Timed {
    fn new() -> Self {
        let inlets = inlets(Duration::ZERO);
        let (writers, readers) = InputWriters::new();
        let (timing, resyncs) = (readers.timing, readers.resyncs);
        let (device, _node) = inlets.open_with(writers.split().0);
        let clock = BlockClock::new();
        inlets.set_clock(Some(clock.reader()));
        let mut timed = Self {
            device,
            timing,
            resyncs,
            clock,
            t0: Instant::now() + Duration::from_secs(3600),
        };
        timed.read(Duration::ZERO, 1000);
        timed
    }

    /// The engine reads frame `frame` at `t0 + after`; a push makes the
    /// device's thread see it.
    fn read(&mut self, after: Duration, frame: u64) {
        let at = self.t0 + after;
        self.clock
            .write(at, Frame::new(frame), SampleRate::of(RATE));
        // Whether the event is placed is not under test here.
        let _ = self.device.push(note(60), at);
    }

    /// The reading `frame` makes on a map anchored at frame 1000 (`t0`):
    /// where the nominal rate puts it, `late` seconds off (negative: early).
    fn read_off(&mut self, frame: u64, late: f64) {
        let nominal = (frame - 1000) as f64 / RATE;
        self.read(Duration::from_secs_f64(nominal + late), frame);
    }

    fn correction(&self) -> f64 {
        self.timing.get().get()
    }
}

/// **The first map is not a resync, and nothing has been corrected**: after the
/// first reading the counter reads 0 and the gauge 0 s.
#[test]
fn the_first_map_built_counts_no_resync_and_reads_no_correction() {
    let timed = Timed::new();
    assert_eq!(timed.resyncs.get(), Resyncs::ZERO);
    assert_eq!(timed.timing.get(), Seconds::new(0.0));
}

/// **A late follow counts nothing and the gauge reads its step**: a reading
/// 8 ms late moves the map by an eighth of it, +1 ms.
#[test]
fn a_late_follow_counts_no_resync_and_the_gauge_reads_a_positive_eighth() {
    let mut timed = Timed::new();
    timed.read_off(1480, 0.008);
    assert_eq!(timed.resyncs.get(), Resyncs::ZERO);
    assert!((timed.correction() - 0.001).abs() < 1e-9);
}

/// **An early follow reads the step with the opposite sign**: -1 ms.
#[test]
fn an_early_follow_counts_no_resync_and_the_gauge_reads_a_negative_eighth() {
    let mut timed = Timed::new();
    timed.read_off(1480, -0.008);
    assert_eq!(timed.resyncs.get(), Resyncs::ZERO);
    assert!((timed.correction() + 0.001).abs() < 1e-9);
}

/// **A reading past `RESYNC` counts one resync, and the gauge reads the whole
/// error, signed**: 50 ms off is a restart or a suspend, not jitter.
#[test]
fn a_reading_far_off_counts_one_resync_and_the_gauge_reads_the_whole_error() {
    for late in [0.05, -0.05] {
        let mut timed = Timed::new();
        timed.read_off(49_000, late);
        assert_eq!(timed.resyncs.get(), Resyncs::new(1), "{late}");
        assert!((timed.correction() - late).abs() < 1e-9, "{late}");
    }
}

/// **Each re-anchor counts**: two far readings in a row are two resyncs.
#[test]
fn every_far_reading_counts_its_own_resync() {
    let mut timed = Timed::new();
    timed.read(Duration::from_millis(100), 1480);
    timed.read(Duration::from_millis(300), 1960);
    assert_eq!(timed.resyncs.get(), Resyncs::new(2));
}

/// **A map rebuilt after a rejected reading counts one resync**: an engine that
/// restarts begins its frames again, the map refuses the reading, and the
/// rebuild is a re-anchor. The gauge keeps the last correction: a rejection
/// made none.
#[test]
fn a_map_rebuilt_after_a_rejected_reading_counts_one_resync() {
    let mut timed = Timed::new();
    timed.read_off(1480, 0.008);
    // The frame goes back to 100, a second on.
    timed.read(Duration::from_secs(1), 100);
    assert_eq!(timed.resyncs.get(), Resyncs::new(1));
    assert!((timed.correction() - 0.001).abs() < 1e-9);
}

/// **A rate change is not a rebuild**: the map is rescaled to the same instant,
/// so a reading consistent with the new rate is followed, and counts no resync;
/// one far off after it is a resync as at any rate.
#[test]
fn a_rate_change_counts_a_resync_only_when_its_reading_is_far_off() {
    let mut timed = Timed::new();
    // Frame 1000 at 48 kHz is frame 2000 at 96 kHz; 10 ms later is 960 frames on.
    timed.clock.write(
        timed.t0 + Duration::from_millis(10),
        Frame::new(2960),
        SampleRate::SR_96K,
    );
    let _ = timed.device.push(note(60), timed.t0);
    assert_eq!(timed.resyncs.get(), Resyncs::ZERO);
    assert!(timed.correction().abs() < 1e-6);
    // 1 s on at 96 kHz, 100 ms late.
    timed.clock.write(
        timed.t0 + Duration::from_millis(1110),
        Frame::new(2960 + 96_000),
        SampleRate::SR_96K,
    );
    let _ = timed.device.push(note(61), timed.t0);
    assert_eq!(timed.resyncs.get(), Resyncs::new(1));
}

/// **The readers outlive a connection**: the count is cumulative across a
/// device that drops and an input reopened on the same cells, and the gauge
/// holds its last reading until the new map writes one.
#[test]
fn the_cells_carry_across_a_reopened_input() {
    let inlets = inlets(Duration::ZERO);
    let (writers, readers) = InputWriters::new();
    let (timing, resyncs) = (readers.timing.clone(), readers.resyncs.clone());
    let (mut device, _node) = inlets.open_with(writers.split().0);
    let mut clock = BlockClock::new();
    inlets.set_clock(Some(clock.reader()));
    let t0 = Instant::now() + Duration::from_secs(3600);
    clock.write(t0, Frame::new(1000), SampleRate::of(RATE));
    let _ = device.push(note(60), t0);
    clock.write(
        t0 + Duration::from_millis(1050),
        Frame::new(1000 + 48_000),
        SampleRate::of(RATE),
    );
    let _ = device.push(note(60), t0);
    assert_eq!(resyncs.get(), Resyncs::new(1));
    assert!(
        InputWriters::reopen(&readers).is_none(),
        "the input holds them"
    );
    drop(device);
    assert!(timing.is_closed() && resyncs.is_closed());
    let held = timing.get();
    let again = InputWriters::reopen(&readers).expect("the input dropped");
    assert!(!timing.is_closed() && !resyncs.is_closed());
    assert_eq!(timing.get(), held);
    let (mut device, _node) = inlets.open_with(again.split().0);
    // The new map's first reading is not a resync; a far one after it is the second.
    let t1 = t0 + Duration::from_secs(10);
    clock.write(t1, Frame::new(5000), SampleRate::of(RATE));
    let _ = device.push(note(60), t1);
    assert_eq!(resyncs.get(), Resyncs::new(1));
    clock.write(
        t1 + Duration::from_millis(1100),
        Frame::new(5000 + 48_000),
        SampleRate::of(RATE),
    );
    let _ = device.push(note(60), t1);
    assert_eq!(resyncs.get(), Resyncs::new(2));
}
