//! Events a port cannot hold: a note-off is taken before a note-on is, every
//! loss is counted exactly and charged to the output port that had it, and
//! the parallel executor reports the same losses as the serial one.
//!
//! All figures are counted by hand from the nodes below, never from another
//! render.

use tutti_graph::testing as common;

use std::sync::{Arc, Mutex};
use tutti_buffers::MaxBlock;

use common::test_pool;

use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventKind, EventLoss, EventOut, Executor, Io, Node,
    Prepare, Shape, Status, TransportPosition, note_off_room,
};
use tutti_ump::{MidiChannel, U7, Ump};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 64;

/// MIDI 1.0, group 0, channel 0, note-on, velocity 100.
fn note_on(key: u32) -> [u32; 4] {
    midi::m1_note_on(MidiChannel::FIRST, key as u8, U7::of(100)).0
}

fn note_off(key: u32) -> [u32; 4] {
    midi::m1_note_off(MidiChannel::FIRST, key as u8, U7::of(0)).0
}

/// One port's script for a block: `ons` note-ons then `offs` note-offs, each
/// on its own frame in order (frame = index, so offsets never decrease).
#[derive(Clone, Copy)]
struct Script {
    ons: u32,
    offs: u32,
}

/// Writes one script per output port every block, declaring `cap`.
struct Writer {
    cap: u32,
    scripts: Vec<Script>,
}

impl Node for Writer {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, self.scripts.len() as u16)
            .with_event_capacity(tutti_graph::testing::cap_of(self.cap))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (p, s) in self.scripts.iter().enumerate() {
            let out = io.event_out(p);
            for i in 0..s.ons + s.offs {
                let words = if i < s.ons {
                    note_on(i % 100)
                } else {
                    note_off(i % 100)
                };
                let at = cx.env.offset(i as usize).expect("inside the block");
                let _ = out.push(Event::midi(at, Ump(words)));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Counts what arrives on each input port: `(note-ons, note-offs)`.
struct Counter {
    ports: u16,
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl Node for Counter {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(self.ports, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.seen.lock().unwrap();
        for p in 0..io.event_input_count() {
            for e in io.events(p) {
                if e.is_note_off() {
                    seen[p].1 += 1;
                } else if matches!(e.kind, EventKind::Midi(Ump(_))) {
                    seen[p].0 += 1;
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// `writers` (each a key and its scripts) all feeding one counter, port `i`
/// of the counter taking writer `i`'s port 0 ... in the order listed.
fn rig(
    workers: usize,
    cap: u32,
    writers: &[Vec<Script>],
) -> (Editor, Executor, Arc<Mutex<Vec<(u32, u32)>>>) {
    let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
    let (mut ed, mut exec) = Editor::new(prep);
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    let ports: u16 = writers.iter().map(|w| w.len() as u16).sum();
    let seen = Arc::new(Mutex::new(vec![(0, 0); ports as usize]));
    ed.insert(
        NodeKey(100),
        "counter",
        tutti_graph::Unforkable(Counter {
            ports,
            seen: Arc::clone(&seen),
        }),
    );
    let mut next = 0;
    for (i, scripts) in writers.iter().enumerate() {
        let key = NodeKey(1 + i as u64);
        ed.insert(
            key,
            "writer",
            tutti_graph::Unforkable(Writer {
                cap,
                scripts: scripts.clone(),
            }),
        );
        for p in 0..scripts.len() {
            ed.spec_mut().connect_events(
                EventIn {
                    node: NodeKey(100),
                    port: next,
                },
                EventEdge::Direct(EventOut {
                    node: key,
                    port: p as u16,
                }),
            );
            next += 1;
        }
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, seen)
}

fn run(ed: &mut Editor, exec: &mut Executor, blocks: usize) -> Vec<EventLoss> {
    for _ in 0..blocks {
        exec.process(BLOCK, &TransportPosition::default(), &[], &mut []);
    }
    ed.take_event_losses().collect()
}

/// A port at its capacity takes note-offs, not note-ons, up to the room past
/// it. Capacity 16 is room for 16 + 2 + 8 = 26 events. A script of 20
/// note-ons and 20 note-offs writes the 16 first note-ons (4 refused), and
/// then 10 of its note-offs (10 refused): 16 + 10 = 26 land.
#[test]
fn a_full_port_takes_note_offs_before_note_ons() {
    assert_eq!(note_off_room(16), 26);
    let (mut ed, mut exec, seen) = rig(1, 16, &[vec![Script { ons: 20, offs: 20 }]]);
    let losses = run(&mut ed, &mut exec, 1);
    assert_eq!(*seen.lock().unwrap(), vec![(16, 10)]);
    assert_eq!(exec.dropped_event_count(), 14);
    assert_eq!(
        losses,
        vec![EventLoss {
            node: NodeKey(1),
            port: Some(0),
            count: 14,
            recovered: 0
        }]
    );
}

/// A node that keeps to its declaration loses nothing, note-offs included,
/// although the declaration was all note-ons-worth: 16 note-ons and 10
/// note-offs fit capacity 16 plus its note-off room.
#[test]
fn a_node_within_its_room_loses_nothing() {
    let (mut ed, mut exec, seen) = rig(1, 16, &[vec![Script { ons: 16, offs: 10 }]]);
    let losses = run(&mut ed, &mut exec, 3);
    assert_eq!(*seen.lock().unwrap(), vec![(48, 30)]);
    assert_eq!(exec.dropped_event_count(), 0);
    assert!(losses.is_empty());
}

/// The loss is charged to the port that had it, in the block that had it,
/// and a block with none reports none. Port 1 of the first writer is 5 over
/// (21 note-ons against 16) each of two blocks; its port 0 and the second
/// writer lose nothing.
#[test]
fn a_loss_is_charged_to_its_port() {
    let ok = Script { ons: 4, offs: 4 };
    let over = Script { ons: 21, offs: 0 };
    let (mut ed, mut exec, _) = rig(1, 16, &[vec![ok, over], vec![ok]]);
    let losses = run(&mut ed, &mut exec, 2);
    let each = EventLoss {
        node: NodeKey(1),
        port: Some(1),
        count: 5,
        recovered: 0,
    };
    assert_eq!(losses, vec![each, each]);
    assert_eq!(exec.dropped_event_count(), 10);
}

/// The parallel executor reports the same totals, and here the same set of
/// losses, as the serial one, whatever the worker count: each worker's log is
/// added to the block's. Four writers over capacity on different ports, so
/// the tasks spread. The entries are compared **sorted**: in a parallel block
/// they come in the order the workers finished, and a block that lost at more
/// places than a log holds (not this one) would leave out different ones. The
/// totals and the render are exact and identical; the order and truncation of
/// entries are not promised to match.
#[test]
fn the_parallel_losses_are_the_serial_ones() {
    let scripts = |n: u32| {
        vec![
            Script {
                ons: 20 + n,
                offs: n,
            },
            Script { ons: 3, offs: 3 },
        ]
    };
    let writers: Vec<Vec<Script>> = (0..4).map(scripts).collect();
    let (mut ed, mut exec, seen) = rig(1, 16, &writers);
    let serial = run(&mut ed, &mut exec, 5);
    let (serial_total, serial_seen) = (exec.dropped_event_count(), seen.lock().unwrap().clone());
    assert!(!serial.is_empty());
    let sorted = |mut v: Vec<EventLoss>| {
        v.sort_by_key(|l| (l.node, l.port, l.count));
        v
    };
    for workers in common::WORKERS {
        let (mut ed, mut exec, seen) = rig(workers, 16, &writers);
        assert!(exec.is_parallel(), "{workers} workers");
        let parallel = run(&mut ed, &mut exec, 5);
        assert_eq!(exec.dropped_event_count(), serial_total, "{workers}");
        assert_eq!(*seen.lock().unwrap(), serial_seen, "{workers}");
        assert_eq!(sorted(parallel), sorted(serial.clone()), "{workers}");
    }
}

/// Records the frame of every event its event inputs are sent, block by
/// block, declaring an input bound of `bound` events per block (all ports
/// together).
struct Recorder {
    bound: Option<u32>,
    blocks: Arc<Mutex<Vec<Vec<u32>>>>,
}

impl Node for Recorder {
    fn shape(&self) -> Shape {
        let shape = Shape::EMPTY.with_events(1, 0);
        match self.bound {
            Some(n) => shape.with_event_input_capacity(tutti_graph::testing::cap_of(n)),
            None => shape,
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let frames = io.events(0).iter().map(|e| e.offset.index() as u32);
        self.blocks.lock().unwrap().push(frames.collect());
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A bounded-input rig: a [`Recorder`] (key 100) bounded to `bound`, and a
/// writer (key `1 + i`) of `ons[i]` note-ons per block on frames
/// `0..ons[i]` for each `i`, all into its port 0.
struct Bounded {
    ed: Editor,
    exec: Executor,
    blocks: Arc<Mutex<Vec<Vec<u32>>>>,
    writers: u64,
}

impl Bounded {
    fn new(workers: usize, bound: Option<u32>, ons: &[u32]) -> Self {
        let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
        let (mut ed, mut exec) = Editor::new(prep);
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        let blocks = Arc::default();
        ed.insert(
            NodeKey(100),
            "recorder",
            tutti_graph::Unforkable(Recorder {
                bound,
                blocks: Arc::clone(&blocks),
            }),
        );
        let mut rig = Self {
            ed,
            exec,
            blocks,
            writers: 0,
        };
        for &n in ons {
            rig.wire(n);
        }
        rig.commit();
        rig
    }

    /// A new writer of `ons` note-ons into the recorder, not yet committed.
    fn wire(&mut self, ons: u32) {
        self.writers += 1;
        let key = NodeKey(self.writers);
        self.ed.insert(
            key,
            "writer",
            tutti_graph::Unforkable(Writer {
                cap: 64,
                scripts: vec![Script { ons, offs: 0 }],
            }),
        );
        let input = self.input();
        self.ed
            .spec_mut()
            .connect_events(input, EventEdge::Direct(EventOut { node: key, port: 0 }));
    }

    fn input(&self) -> EventIn {
        EventIn {
            node: NodeKey(100),
            port: 0,
        }
    }

    fn commit(&mut self) {
        self.ed.commit().expect("commits");
        self.exec.apply_pending();
        self.ed.collect();
    }

    /// The frames the recorder was sent in each of `n` more blocks.
    fn run(&mut self, n: usize) -> Vec<Vec<u32>> {
        self.blocks.lock().unwrap().clear();
        for _ in 0..n {
            self.exec
                .process(BLOCK, &TransportPosition::default(), &[], &mut []);
        }
        self.blocks.lock().unwrap().clone()
    }
}

/// Two writers send 6 and 5 note-ons a block on frames `0..6` and `0..5`,
/// 11 events past a bound of 8. The merge orders by frame, ties to the lower
/// source: frames 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5. The node is sent the first
/// 8 of them, and the 3 past them are counted once each block, the same on the
/// serial executor and on every worker count.
#[test]
fn a_declared_input_bound_keeps_the_earliest_and_counts_the_rest() {
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let mut rig = Bounded::new(workers, Some(8), &[6, 5]);
        let got = rig.run(2);
        assert_eq!(got, vec![vec![0, 0, 1, 1, 2, 2, 3, 3]; 2], "{workers}");
        assert_eq!(rig.exec.dropped_event_count(), 6, "{workers}");
        assert_eq!(rig.exec.lost_event_count(), 6, "{workers}");
        // Every cut is the node's: charged to it, at no port.
        let losses: Vec<EventLoss> = rig.ed.take_event_losses().collect();
        let each = EventLoss {
            node: NodeKey(100),
            port: None,
            count: 3,
            recovered: 0,
        };
        assert_eq!(losses, vec![each, each], "{workers}");
        assert_eq!(rig.ed.take_unreported_event_losses(), 0, "{workers}");
    }
}

/// Events within the bound are all delivered and nothing is counted: the same
/// 11 events under a bound of 11.
#[test]
fn events_within_the_input_bound_are_all_delivered() {
    let mut rig = Bounded::new(1, Some(11), &[6, 5]);
    let got = rig.run(2);
    assert_eq!(got, vec![vec![0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5]; 2]);
    assert_eq!(rig.exec.dropped_event_count(), 0);
}

/// A bound holds a node whose input has a single writer: 9 events past a
/// bound of 4 deliver frames 0..4 and count 5.
#[test]
fn a_declared_input_bound_holds_a_single_writer() {
    let mut rig = Bounded::new(1, Some(4), &[9]);
    assert_eq!(rig.run(1), vec![vec![0, 1, 2, 3]]);
    assert_eq!(rig.exec.dropped_event_count(), 5);
}

/// The bound is the node's, not the edges': a writer wired in by a commit,
/// with no re-prepare of the node, is held to it. One writer of 5 is within 8;
/// a second (5 more) is cut to 8 and counts 2; a third (5 more) counts 7.
#[test]
fn an_input_bound_holds_when_a_writer_is_wired_in_without_a_reprepare() {
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let mut rig = Bounded::new(workers, Some(8), &[5]);
        assert_eq!(rig.run(1), vec![vec![0, 1, 2, 3, 4]], "{workers}");
        assert_eq!(rig.exec.dropped_event_count(), 0);
        rig.wire(5);
        rig.commit();
        assert_eq!(rig.run(1), vec![vec![0, 0, 1, 1, 2, 2, 3, 3]], "{workers}");
        assert_eq!(rig.exec.dropped_event_count(), 2, "{workers}");
        rig.wire(5);
        rig.commit();
        assert_eq!(rig.run(1), vec![vec![0, 0, 0, 1, 1, 1, 2, 2]], "{workers}");
        assert_eq!(rig.exec.dropped_event_count(), 2 + 7, "{workers}");
    }
}

/// A node declaring no input bound is sent everything its edges send, and its
/// merge slot is sized from the writers (the existing sizing, pinned by
/// `declared_capacities_size_merges_and_delays_so_nothing_is_lost`): 11
/// events with no bound all arrive.
#[test]
fn an_undeclared_input_is_not_bounded() {
    let mut rig = Bounded::new(1, None, &[6, 5]);
    assert_eq!(rig.run(1), vec![vec![0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5]]);
    assert_eq!(rig.exec.dropped_event_count(), 0);
}

/// The compiled merge slot holds `min(bound, what the writers can send)`,
/// sized on the control side: two writers of declared capacity 64 hold
/// `2 × note_off_room(64)` events, and a bound of 8 holds 8 and a bound
/// above that, the sum.
#[test]
fn the_merge_slot_is_sized_to_the_smaller_of_the_bound_and_the_writers() {
    let sized = |bound: u32| {
        let rig = Bounded::new(1, Some(bound), &[1, 1]);
        let plan = rig.exec.plan().expect("applied");
        plan.ops()
            .iter()
            .filter_map(|op| match *op {
                tutti_graph::Op::EventMerge { dst, bound, .. } => {
                    Some((plan.event_slot_capacity()[dst as usize], bound))
                }
                _ => None,
            })
            .next()
            .expect("a merge")
    };
    let sum = 2 * note_off_room(64) as u32;
    let (cap, bound) = sized(8);
    assert_eq!(
        (cap.declared, cap.defaults, bound.map(|b| b.capacity.get())),
        (8, 0, Some(8))
    );
    let (cap, bound) = sized(10_000);
    assert_eq!(
        (cap.declared, cap.defaults, bound.map(|b| b.capacity.get())),
        (sum, 0, Some(10_000))
    );
}

/// A [`Writer`] declaring `latency` frames of processing latency, so a node
/// without any, wired to the same input, is the one delayed.
struct Latent {
    inner: Writer,
    latency: usize,
}

impl Node for Latent {
    fn shape(&self) -> Shape {
        self.inner
            .shape()
            .with_latency(tutti_units::Latency::new(Samples::new(self.latency)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.inner.process(cx, io)
    }
    fn reset(&mut self) {}
}

/// Writes one note-on on the first frame of its first block.
struct Once {
    written: bool,
}

impl Node for Once {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if !std::mem::replace(&mut self.written, true) {
            let at = cx.env.offset(0).expect("inside the block");
            let _ = io.event_out(0).push(Event::midi(at, Ump(note_on(60))));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// The bound covers everything the node is sent on its inputs in a block, not
/// only the edges. A node bounded to 4 has 4 events from its writer on
/// frames 0 to 3, which fill it. In block 1 a flush (the event a removed,
/// delayed writer still held) and a scheduled command, both on frame 0, join
/// them: 6 events. The node is sent 4, in merge order (flush, writer's frame 0,
/// command, writer's frame 1: frames 0, 0, 0, 1), and the 2 cut are counted.
#[test]
fn a_declared_input_bound_covers_flushed_events_and_scheduled_commands() {
    use tutti_units::{At, Frame};
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
        let (mut ed, mut exec) = Editor::new(prep);
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        let blocks = Arc::default();
        let input = EventIn {
            node: NodeKey(100),
            port: 0,
        };
        ed.insert(
            NodeKey(100),
            "recorder",
            tutti_graph::Unforkable(Recorder {
                bound: Some(4),
                blocks: Arc::clone(&blocks),
            }),
        );
        // The writer's latency makes the other writer's edge a delayed one.
        ed.insert(
            NodeKey(1),
            "writer",
            tutti_graph::Unforkable(Latent {
                inner: Writer {
                    cap: 64,
                    scripts: vec![Script { ons: 4, offs: 0 }],
                },
                latency: 256,
            }),
        );
        ed.insert(
            NodeKey(2),
            "once",
            tutti_graph::Unforkable(Once { written: false }),
        );
        for node in [1, 2] {
            ed.spec_mut().connect_events(
                input,
                EventEdge::Direct(EventOut {
                    node: NodeKey(node),
                    port: 0,
                }),
            );
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let transport = TransportPosition::default();
        exec.process(BLOCK, &transport, &[], &mut []);
        assert_eq!(blocks.lock().unwrap().last().unwrap(), &[0, 1, 2, 3]);
        assert_eq!(exec.dropped_event_count(), 0, "{workers}");

        ed.schedule(
            At::Frame(Frame::new(BLOCK as u64)),
            input,
            EventKind::Midi(midi::m1_note_on(MidiChannel::FIRST, 61, U7::of(100))),
        )
        .expect("room");
        ed.remove(NodeKey(2));
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        exec.process(BLOCK, &transport, &[], &mut []);
        assert_eq!(
            blocks.lock().unwrap().last().unwrap(),
            &[0, 0, 0, 1],
            "{workers}"
        );
        assert_eq!(exec.dropped_event_count(), 2, "{workers}");

        exec.process(BLOCK, &transport, &[], &mut []);
        assert_eq!(blocks.lock().unwrap().last().unwrap(), &[0, 1, 2, 3]);
        assert_eq!(exec.dropped_event_count(), 2, "{workers}");
    }
}

/// A lone writer whose port cannot hold more than the bound needs no merge:
/// the node reads the writer's own slot. A writer declaring 4 fills at most
/// `note_off_room(4)` = 12 events; a bound of 12 leaves it be, a bound of 11
/// puts a merge in front of the node, and so does a writer that declared
/// nothing.
#[test]
fn a_lone_writer_within_the_bound_gets_no_merge() {
    let merges = |bound: u32, cap: u32| {
        let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
        let (mut ed, mut exec) = Editor::new(prep);
        ed.insert(
            NodeKey(100),
            "recorder",
            tutti_graph::Unforkable(Recorder {
                bound: Some(bound),
                blocks: Arc::default(),
            }),
        );
        ed.insert(
            NodeKey(1),
            "writer",
            tutti_graph::Unforkable(Writer {
                cap,
                scripts: vec![Script { ons: 1, offs: 0 }],
            }),
        );
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(100),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        ed.commit().expect("commits");
        exec.apply_pending();
        let plan = exec.plan().expect("applied");
        plan.ops()
            .iter()
            .filter(|op| matches!(op, tutti_graph::Op::EventMerge { .. }))
            .count()
    };
    assert_eq!(note_off_room(4), 12);
    assert_eq!(merges(12, 4), 0);
    assert_eq!(merges(11, 4), 1);
    assert_eq!(merges(1_000, 4), 0);
}

/// A replacement whose only difference is its input bound swaps in whole,
/// not through a crossfade the plan cannot hold, and the new bound holds in
/// the next block.
#[test]
fn replace_or_swap_to_a_node_with_another_bound_commits_and_holds_it() {
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let mut rig = Bounded::new(workers, Some(8), &[6, 5]);
        assert_eq!(rig.run(1)[0].len(), 8);
        let fade = tutti_graph::Fade::new(Samples::new(100));
        rig.ed
            .replace_or_swap(
                NodeKey(100),
                tutti_graph::Unforkable(Recorder {
                    bound: Some(3),
                    blocks: Arc::clone(&rig.blocks),
                }),
                fade,
            )
            .expect("not poisoned, not re-preparing");
        rig.commit();
        assert_eq!(rig.ed.fades_in_flight(), 0, "a changed bound is a swap");
        assert_eq!(rig.run(1), vec![vec![0, 0, 1]], "{workers}");
    }
}

/// Holds what it is sent, counting each block's events and how many are
/// note-offs; declares `bound` and an event output so it can be bypassed.
struct Holder {
    bound: u32,
    blocks: Arc<Mutex<Vec<(usize, usize)>>>,
}

impl Node for Holder {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, 1)
            .with_event_input_capacity(tutti_graph::testing::cap_of(self.bound))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let evs = io.events(0);
        let offs = evs.iter().filter(|e| e.is_note_off()).count();
        self.blocks.lock().unwrap().push((evs.len(), offs));
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Writes `per_block[i]` note-ons of fresh keys in block `i`, then nothing.
struct Keys {
    per_block: Vec<u32>,
    block: usize,
    next: u32,
}

impl Node for Keys {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(tutti_graph::testing::cap_of(16))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = self.per_block.get(self.block).copied().unwrap_or(0);
        self.block += 1;
        for i in 0..n {
            let at = cx.env.offset(i as usize).expect("inside the block");
            let _ = io
                .event_out(0)
                .push(Event::midi(at, Ump(note_on(self.next))));
            self.next += 1;
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A bypassed node is sent the closes of what it holds, and a bound of 4
/// holds those too: 10 held keys close 4, 4 and 2 in three blocks, the rest
/// staying held rather than cut, so no loss is counted.
#[test]
fn a_bypassed_node_is_sent_at_most_its_bound_of_closes_a_block() {
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
        let (mut ed, mut exec) = Editor::new(prep);
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        let blocks = Arc::default();
        ed.insert(
            NodeKey(100),
            "holder",
            tutti_graph::Unforkable(Holder {
                bound: 4,
                blocks: Arc::clone(&blocks),
            }),
        );
        ed.insert(
            NodeKey(1),
            "keys",
            tutti_graph::Unforkable(Keys {
                per_block: vec![4, 4, 2],
                block: 0,
                next: 40,
            }),
        );
        ed.spec_mut().connect_events(
            EventIn {
                node: NodeKey(100),
                port: 0,
            },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let transport = TransportPosition::default();
        for _ in 0..3 {
            exec.process(BLOCK, &transport, &[], &mut []);
        }
        assert_eq!(*blocks.lock().unwrap(), vec![(4, 0), (4, 0), (2, 0)]);
        blocks.lock().unwrap().clear();
        ed.set_mode(NodeKey(100), tutti_graph::NodeMode::Bypassed)
            .expect("a node");
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        for _ in 0..5 {
            exec.process(BLOCK, &transport, &[], &mut []);
        }
        let got: Vec<(usize, usize)> = blocks
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|&(n, _)| n > 0)
            .collect();
        assert_eq!(got, vec![(4, 4), (4, 4), (2, 2)], "{workers}");
        assert_eq!(exec.dropped_event_count(), 0, "{workers}");
    }
}

/// Writes the note-ons on frames 0 to 3 and the note-offs on frames 20 and 21
/// of every block.
struct EarlyOnsLateOffs;

impl Node for EarlyOnsLateOffs {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(tutti_graph::testing::cap_of(16))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (frame, off) in [
            (0, false),
            (1, false),
            (2, false),
            (3, false),
            (20, true),
            (21, true),
        ] {
            let at = cx.env.offset(frame).expect("inside the block");
            let words = if off {
                note_off(60 + frame as u32)
            } else {
                note_on(60 + frame as u32)
            };
            let _ = io.event_out(0).push(Event::midi(at, Ump(words)));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Records `(frame, is note-off)` for each event it is sent, block by block,
/// declaring a bound of 4.
struct Trace {
    blocks: Arc<Mutex<Vec<Vec<(u32, bool)>>>>,
}

impl Node for Trace {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, 0)
            .with_event_input_capacity(tutti_graph::testing::cap_of(4))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        let seen = io
            .events(0)
            .iter()
            .map(|e| (e.offset.index() as u32, e.is_note_off()));
        self.blocks.lock().unwrap().push(seen.collect());
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Note-offs decide which events survive the cut, not their order: what the
/// node is sent stays in frame order. A bound of 4 meets note-ons on frames 0
/// to 3 and note-offs on frames 20 and 21. The late note-offs survive, the
/// two latest note-ons do not, and the node is sent frames 0, 1, 20, 21. In
/// the next block a note-off scheduled onto frame 30 joins them in the joint
/// cut, which keeps it too and cuts one more note-on: frames 0, 20, 21, 30.
/// Both are non-decreasing in frame, serial and parallel.
#[test]
fn a_cut_keeps_late_note_offs_and_delivers_in_frame_order() {
    use tutti_units::{At, Frame};
    for workers in std::iter::once(1).chain(common::WORKERS) {
        let prep = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(BLOCK)));
        let (mut ed, mut exec) = Editor::new(prep);
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        let blocks = Arc::default();
        let input = EventIn {
            node: NodeKey(100),
            port: 0,
        };
        ed.insert(
            NodeKey(100),
            "trace",
            tutti_graph::Unforkable(Trace {
                blocks: Arc::clone(&blocks),
            }),
        );
        ed.insert(
            NodeKey(1),
            "writer",
            tutti_graph::Unforkable(EarlyOnsLateOffs),
        );
        ed.spec_mut().connect_events(
            input,
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        ed.schedule(
            At::Frame(Frame::new(BLOCK as u64 + 30)),
            input,
            EventKind::Midi(midi::m1_note_off(MidiChannel::FIRST, 90, U7::of(0))),
        )
        .expect("room");
        let transport = TransportPosition::default();
        exec.process(BLOCK, &transport, &[], &mut []);
        exec.process(BLOCK, &transport, &[], &mut []);
        let got = blocks.lock().unwrap().clone();
        let early = vec![(0, false), (1, false), (20, true), (21, true)];
        let joint = vec![(0, false), (20, true), (21, true), (30, true)];
        assert_eq!(got, vec![early, joint], "{workers}");
        for block in &got {
            assert!(block.windows(2).all(|w| w[0].0 <= w[1].0), "{block:?}");
        }
        // Block 0 cuts 2 at the merge; block 1 cuts the same 2 there and 1 more at the node.
        assert_eq!(exec.dropped_event_count(), 2 + 3, "{workers}");
    }
}
