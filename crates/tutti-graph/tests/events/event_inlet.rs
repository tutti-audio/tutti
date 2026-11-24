//! The event inlet: MIDI a thread of your own pushes, each event on its frame.
//!
//! Driven two ways: by hand through `contract::Direct` (the node's own
//! `process` on blocks of chosen length and frame, which is what pins offsets,
//! the late rule, the counts and the rate), and through an `Editor` and an
//! `Executor` (the silent fork, and the one-participant-against-pooled render with the
//! ring pre-filled, every participant inside `RtScope`).
//!
//! Every figure is analytic (a frame, an offset, a capacity), none read from
//! another render.

use tutti_graph::testing as common;

use common::{Kind, TestNode};

use tutti_buffers::MaxBlock;
use tutti_fixtures::{cycle_blocks, midi};
use tutti_graph::contract::{Direct, HAND_EVENT_CAPACITY};
use tutti_graph::{
    Edge, Editor, Env, EventCapacity, EventEdge, EventInletNode, EventInletWriter, EventInputPort,
    EventKind, EventOutputPort, EventTapId, EventTapOptions, EventTapReader, Executor, ForkCause,
    ForkSource, ForkTarget, Forked, InputPort, IntoNode, Node, NodeKey, NodeParts, OutputPort,
    PlayClock, PortSource, Prepare, RenderMode, Stamp, TapAlign, TapEvent, TransportChanges,
    TransportPosition,
};
use tutti_rt::PushError;
use tutti_ump::{CCNumber, MidiChannel, U7, Ump};
use tutti_units::{Frame, Losses, Moment, SampleRate, Samples};

const R48: SampleRate = SampleRate::SR_48K;
const R441: SampleRate = SampleRate::SR_44K1;

/// MIDI 1.0, channel 0, note-on, velocity 100.
fn note(n: u32) -> Ump {
    midi::m1_note_on(MidiChannel::FIRST, n as u8, U7::of(100))
}

fn off(n: u32) -> Ump {
    midi::m1_note_off(MidiChannel::FIRST, n as u8, U7::of(0))
}

/// A MIDI 1.0 Control Change of `controller`, value 0.
fn cc(controller: u32) -> Ump {
    midi::m1_cc(
        MidiChannel::FIRST,
        CCNumber::of(controller as u8),
        U7::of(0),
    )
}

type Rig = (Direct<EventInletNode>, EventInletWriter);

fn rig(capacity: u32) -> Rig {
    let (node, tx) = EventInletNode::pair(EventCapacity::of(capacity));
    (Direct::new(node, R48, 64), tx)
}

/// `(offset, status nibble, data byte 1)` of what the last block emitted.
fn sent(d: &Direct<EventInletNode>) -> Vec<(u32, u32, u32)> {
    let port = Node::<f32>::shape(&*d.node)
        .event_output(0)
        .expect("the inlet's output");
    d.event_output(port)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(Ump(w)) => (e.offset.get(), (w[0] >> 20) & 0xf, (w[0] >> 8) & 0x7f),
            _ => unreachable!(),
        })
        .collect()
}

/// The keys of what the last block emitted.
fn keys(d: &Direct<EventInletNode>) -> Vec<u32> {
    sent(d).into_iter().map(|(_, _, k)| k).collect()
}

/// A block of `len` frames starting at `frame` of the clock at `rate`,
/// wherever the last one ended: what the executor hands a node after a
/// re-prepare or a stretch it did not run.
fn block_at(d: &mut Direct<EventInletNode>, frame: u64, rate: SampleRate, len: usize) {
    d.block_in(&Env {
        frame: Frame::new(frame),
        sample_rate: rate,
        block_len: Samples::new(len),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    });
}

fn reprepare(d: &mut Direct<EventInletNode>, rate: SampleRate) {
    Node::<f32>::prepare(
        &mut *d.node,
        &Prepare::new(rate, MaxBlock::of(Samples::new(64))),
    );
}

/// An event pushed for frame F is emitted in the block that holds F, on offset
/// F minus the block's first frame, once, for block sizes 1, 64, 100 and 512
/// and for frames on a block's first and last frame and either side of the
/// edge, pushed in reverse order (the node sorts).
#[test]
fn an_event_lands_on_its_frame_across_block_sizes() {
    for size in [1usize, 64, 100, 512] {
        let s = size as u64;
        let mut frames: Vec<u64> = vec![
            0,
            1,
            s - 1,
            s,
            s + 1,
            2 * s - 1,
            2 * s,
            2 * s + 1,
            700,
            1023,
        ];
        frames.sort_unstable();
        frames.dedup();
        frames.retain(|&f| f < 1_100);
        let (node, mut tx) = EventInletNode::pair(EventCapacity::of(64));
        let mut d = Direct::new(node, R48, size);
        for (k, &f) in frames.iter().enumerate().rev() {
            tx.push(Frame::new(f), note(k as u32)).unwrap();
        }
        let mut got = Vec::new();
        let (mut start, mut blocks) = (0u64, 0u64);
        while start < 1_100 {
            d.block();
            for (offset, _, key) in sent(&d) {
                got.push((start + u64::from(offset), key));
            }
            start += s;
            blocks += 1;
        }
        let want: Vec<(u64, u32)> = frames
            .iter()
            .enumerate()
            .map(|(k, &f)| (f, k as u32))
            .collect();
        assert_eq!(got, want, "block size {size} ({blocks} blocks)");
        assert_eq!(tx.counts().losses(), Losses::NONE);
    }
}

/// `NextBlock` is the next block's first frame and is not late; a frame in the
/// past lands at offset 0 and is counted late, not dropped; a frame ahead
/// waits across blocks, is not lost, and lands on its offset.
#[test]
fn next_block_late_and_not_yet() {
    let (mut d, mut tx) = rig(256);
    d.block();
    d.block();
    tx.push(Stamp::NextBlock, note(1)).unwrap();
    d.block(); // 128..192
    assert_eq!(sent(&d), [(0, 9, 1)]);
    assert_eq!(
        tx.counts().losses().late,
        0,
        "as soon as possible is not late"
    );
    tx.push(Frame::new(5), note(2)).unwrap();
    d.block(); // 192..256
    assert_eq!(sent(&d), [(0, 9, 2)], "a past frame lands at offset 0");
    assert_eq!(tx.counts().losses().late, 1);
    tx.push(Frame::new(64 * 8 + 20), note(3)).unwrap();
    for _ in 0..3 {
        d.block(); // 256..448
        assert_eq!(sent(&d), []);
    }
    d.block(); // 448..512: not yet
    assert_eq!(sent(&d), []);
    d.block(); // 512..576
    assert_eq!(sent(&d), [(20, 9, 3)]);
    d.block();
    assert_eq!(sent(&d), [], "and only once");
    assert_eq!(
        (
            tx.counts().losses().late,
            tx.counts().losses().lost,
            tx.counts().losses().evicted,
            tx.counts().losses().refused
        ),
        (1, 0, 0, 0)
    );
}

/// Events due together leave in the order they were pushed: a late one, a
/// `NextBlock` and the same frame, and a note-off pushed after its note-on at
/// one frame never overtakes it.
#[test]
fn equal_frames_leave_in_push_order() {
    let (mut d, mut tx) = rig(256);
    d.block();
    d.block();
    tx.push(Frame::new(128 + 40), note(61)).unwrap();
    tx.push(Frame::new(128 + 10), note(62)).unwrap();
    tx.push(Stamp::NextBlock, note(64)).unwrap();
    tx.push(Frame::new(5), note(63)).unwrap();
    tx.push(Frame::new(128 + 20), note(70)).unwrap();
    tx.push(Frame::new(128 + 20), off(70)).unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            (0, 9, 64),
            (0, 9, 63),
            (10, 9, 62),
            (20, 9, 70),
            (20, 8, 70),
            (40, 9, 61)
        ]
    );
}

/// A push to a dropped node answers `Disconnected` and counts nothing: the
/// event had nowhere to go, which is not a ring that was full.
#[test]
fn a_push_to_a_dropped_node_is_disconnected_and_uncounted() {
    let (d, mut tx) = rig(8);
    drop(d);
    assert_eq!(
        tx.push(Stamp::NextBlock, note(1)),
        Err(PushError::Disconnected)
    );
    assert_eq!(tx.counts().losses().lost, 0);
}

/// A producer faster than the graph: the ring of 8 takes 8, refuses the rest
/// with `Full`, never blocks, and `lost()` equals the refusals; what it held
/// is emitted, and room returns once the node has drained.
#[test]
fn a_full_ring_refuses_and_counts_lost() {
    let (mut d, mut tx) = rig(8);
    for k in 0..8 {
        tx.push(Stamp::NextBlock, note(k)).unwrap();
    }
    for _ in 0..4 {
        assert_eq!(tx.push(Stamp::NextBlock, note(99)), Err(PushError::Full));
    }
    assert_eq!(tx.counts().losses().lost, 4);
    d.block();
    assert_eq!(keys(&d), (0..8).collect::<Vec<_>>());
    assert_eq!(
        (
            tx.counts().losses().evicted,
            tx.counts().losses().refused,
            tx.counts().losses().late
        ),
        (0, 0, 0)
    );
    tx.push(Stamp::NextBlock, note(7))
        .expect("room after a drain");
}

/// `push_all` queues in order, answers how many the ring took, and counts each
/// refused one as lost: a ring of 4 given 6 takes 4 and loses 2.
#[test]
fn push_all_answers_how_many_were_taken_and_counts_the_rest_lost() {
    let (mut d, mut tx) = rig(4);
    let events = (0..6).map(|k| (Stamp::NextBlock, note(k)));
    assert_eq!(tx.push_all(events), 4);
    assert_eq!(tx.counts().losses(), Losses::lost(2));
    d.block();
    assert_eq!(keys(&d), vec![0, 1, 2, 3]);
}

/// Eight events wait (seven note-ons and, latest of all, `last`), the list is
/// full, and a due event arrives: it costs the latest note-on, never `last`.
/// A jump far ahead then shows what survived, in order.
fn full_list_keeps(last: Ump, survivor: Vec<u32>) {
    let (mut d, mut tx) = rig(8);
    for k in 0..7 {
        tx.push(Frame::new(10_000 + u64::from(k)), note(10 + k))
            .unwrap();
    }
    tx.push(Frame::new(20_000), last).unwrap();
    d.block(); // the list is full
    assert_eq!(tx.counts().losses().evicted, 0);
    tx.push(Stamp::NextBlock, note(72)).unwrap();
    d.block();
    assert_eq!(keys(&d), [72], "a due event is not held up by later ones");
    assert_eq!(
        tx.counts().losses().evicted,
        1,
        "it took the room of the latest note-on"
    );
    block_at(&mut d, 30_000, R48, 64);
    assert_eq!(keys(&d), survivor);
    assert_eq!(
        (tx.counts().losses().lost, tx.counts().losses().refused),
        (0, 0)
    );
}

/// A full waiting list evicts the latest event that may be lost and keeps a
/// note-off.
#[test]
fn a_full_list_keeps_the_note_off() {
    full_list_keeps(off(99), vec![10, 11, 12, 13, 14, 15, 99]);
}

/// And a panic controller (All Notes Off).
#[test]
fn a_full_list_keeps_a_panic_controller() {
    full_list_keeps(cc(123), vec![10, 11, 12, 13, 14, 15, 123]);
}

/// A list of nothing but note-offs and a panic that is full loses the latest
/// of them, and counts it; the earlier ones stay and a new earlier one takes
/// its place.
#[test]
fn a_list_of_nothing_but_note_offs_loses_the_latest() {
    let (mut d, mut tx) = rig(8);
    for k in 0u64..8 {
        tx.push(Frame::new(10_000 + k), off(10)).unwrap();
    }
    d.block();
    tx.push(Frame::new(10_000_000), off(11)).unwrap();
    tx.push(Frame::new(9_000), off(12)).unwrap();
    d.block();
    assert_eq!(tx.counts().losses().evicted, 2);
    block_at(&mut d, 9_000, R48, 64);
    assert_eq!(keys(&d), [12], "the earlier one took the latest's place");
}

/// A loseable event never displaces a must-arrive one: a note-on pushed
/// earlier than a full list of note-offs is the event lost, and every
/// note-off still goes out (a dropped note-off would leave its note hanging).
#[test]
fn a_note_on_never_displaces_a_waiting_note_off() {
    let (mut d, mut tx) = rig(8);
    for k in 0u32..8 {
        tx.push(Frame::new(10_000 + u64::from(k)), off(20 + k))
            .unwrap();
    }
    d.block();
    tx.push(Frame::new(9_000), note(30)).unwrap();
    d.block();
    assert_eq!(tx.counts().losses().evicted, 1);
    block_at(&mut d, 9_000, R48, 64);
    assert_eq!(
        keys(&d),
        Vec::<u32>::new(),
        "the note-on was the event lost"
    );
    block_at(&mut d, 10_000, R48, 64);
    assert_eq!(
        keys(&d),
        (20..28).collect::<Vec<u32>>(),
        "every note-off went out"
    );
}

/// A port narrower than the burst: the node's list holds 1500 events, the port
/// (the harness's, 1024) takes 1024, and the other 476 are counted as
/// refused by the port, and nowhere else.
#[test]
fn a_port_narrower_than_the_burst_counts_refused() {
    let burst = HAND_EVENT_CAPACITY + 476;
    let (mut d, mut tx) = rig(u32::try_from(burst).unwrap());
    for k in 0..burst {
        tx.push(Stamp::NextBlock, note((k % 100) as u32)).unwrap();
    }
    d.block();
    assert_eq!(sent(&d).len(), HAND_EVENT_CAPACITY);
    assert_eq!(tx.counts().losses().refused, 476);
    assert_eq!(
        (
            tx.counts().losses().lost,
            tx.counts().losses().evicted,
            tx.counts().losses().late
        ),
        (0, 0, 0)
    );
}

/// The clock is where the last block ended, on the clock the node ran on and
/// not a count of blocks, with the rate it was counted at; a new rate moves it
/// to the same wall-clock time.
#[test]
fn the_writer_reads_the_clock_and_its_rate() {
    let (mut d, tx) = rig(8);
    let at = |frame, rate| Some(Moment::new(Frame::new(frame), rate));
    assert_eq!(tx.clock(), at(0, R48));
    d.block();
    assert_eq!(tx.clock(), at(64, R48));
    d.set_block_len(48);
    d.block();
    assert_eq!(tx.clock(), at(112, R48));
    reprepare(&mut d, R441);
    // 112 x 44100 / 48000 = 102.9
    assert_eq!(tx.clock(), at(103, R441));
}

/// Pushed before a rate change, an event lands at the rescaled frame: the
/// same wall-clock time at the new rate, not the old frame number.
#[test]
fn what_waits_is_rescaled_by_a_rate_change() {
    let (mut d, mut tx) = rig(8);
    tx.push(Frame::new(1_000), note(60)).unwrap();
    d.block();
    d.block();
    d.block(); // the node holds it now
    assert_eq!(sent(&d), []);
    reprepare(&mut d, R441);
    // 1000 x 44100 / 48000 = 918.75, so offset 7 of the block at 912
    block_at(&mut d, 912, R441, 64);
    assert_eq!(sent(&d), [(7, 9, 60)]);
}

/// A reading taken at 48 kHz and pushed after the graph went to 44.1 kHz still
/// names its instant: the read, the change and the push, in that order.
#[test]
fn a_moment_read_before_a_re_prepare_keeps_its_instant() {
    let (mut d, mut tx) = rig(8);
    d.block();
    d.block();
    d.block();
    let read = tx.clock().expect("prepared");
    assert_eq!((read.frame(), read.rate()), (Frame::new(192), R48));
    reprepare(&mut d, R441);
    tx.push(read.after(Samples::new(500)), note(61)).unwrap();
    // 692 x 44100 / 48000 = 635.7, so offset 8 of the block at 628
    block_at(&mut d, 628, R441, 64);
    assert_eq!(sent(&d), [(8, 9, 61)]);
}

/// A bare frame carries no rate: it is counted at the rate the node has when it
/// takes the event from the ring, so one pushed before a re-prepare and taken
/// after is at the new rate's frame number (a producer that needs the instant
/// pushes a `Moment`).
#[test]
fn a_bare_frame_is_at_the_rate_the_node_has_when_it_takes_it() {
    let (mut d, mut tx) = rig(8);
    tx.push(Frame::new(1_000), note(60)).unwrap();
    reprepare(&mut d, R441); // the ring still holds it
    // frame 1000 of 44.1 kHz: offset 40 of the block at 960
    block_at(&mut d, 960, R441, 64);
    assert_eq!(sent(&d), [(40, 9, 60)]);
}

/// An event whose frame passed while the node did not run (a re-prepare
/// suspends it) lands on the first frame of the block that follows, in the
/// order it was due, and is counted late.
#[test]
fn what_came_due_while_the_node_slept_lands_on_the_first_frame() {
    let (mut d, mut tx) = rig(8);
    tx.push(Frame::new(100), note(60)).unwrap();
    tx.push(Frame::new(120), note(61)).unwrap();
    d.block(); // frames 0..64: both wait
    block_at(&mut d, 500, R48, 64);
    assert_eq!(sent(&d), [(0, 9, 60), (0, 9, 61)]);
    assert_eq!(tx.counts().losses().late, 2);
}

/// What waits survives a reset: a note-off waiting must still arrive.
#[test]
fn what_waits_survives_a_reset() {
    let (mut d, mut tx) = rig(8);
    tx.push(Frame::new(200), note(70)).unwrap();
    d.block();
    Node::<f32>::reset(&mut *d.node);
    d.block();
    d.block();
    d.block();
    assert_eq!(sent(&d), [(8, 9, 70)]);
}

const MAX_BLOCK: usize = 512;

/// A `Kind` whose fork is a fresh node of the same kind.
struct Forkable(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TestNode::new(self.0.clone()))))
    }
}

impl IntoNode for Forkable {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(TestNode::new(self.0)), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts::new(
            Box::new(TestNode::new(self.0.clone())),
            (),
            Some(Box::new(KindFork(self.0))),
            None,
        )
    }
}

fn out(node: u64) -> PortSource {
    PortSource::Node(OutputPort::new(NodeKey(node), 0))
}

/// Inlets 1..=n into a `Consumer` (key 10) whose audio is the only output, so
/// what the inlets emitted is audible as a bit-exact, order-sensitive signal.
/// With `tap`, inlet 1's port is also tapped raw.
fn graph(
    n: u16,
    workers: usize,
    no_alloc: bool,
    tap: bool,
    heavy: bool,
) -> (
    Editor,
    Executor,
    Vec<EventInletWriter>,
    Option<EventTapReader>,
) {
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    if no_alloc {
        common::pooled_fresh(&mut exec, workers);
    } else {
        common::pooled(&mut exec, workers);
    }
    let mut writers = Vec::new();
    for k in 1..=u64::from(n) {
        let (node, writer) = EventInletNode::pair(EventCapacity::of(256));
        ed.insert(NodeKey(k), "inlet", node);
        writers.push(writer);
    }
    ed.insert(
        NodeKey(10),
        "consumer",
        Forkable(Kind::Consumer { inputs: n }),
    );
    for k in 1..=n {
        ed.connect_events(
            EventInputPort::new(NodeKey(10), k - 1),
            EventEdge::Direct(EventOutputPort::new(NodeKey(u64::from(k)), 0)),
        )
        .expect("the edge wires");
    }
    // Heavy audio chains beside the inlets, so the cost model has work worth
    // a thread: the plan is spread on a pool only when that pays.
    for c in (0..4u64).filter(|_| heavy) {
        let base = 100 + c * 10;
        ed.insert(
            NodeKey(base),
            "const",
            Forkable(Kind::Const {
                value: 0.25,
                width: 1,
            }),
        );
        for g in 1..4u64 {
            ed.insert(
                NodeKey(base + g),
                "gain",
                Forkable(Kind::Gain {
                    gain: 0.5,
                    width: 1,
                }),
            );
            ed.connect_edge(tutti_graph::EdgeId {
                sink: tutti_graph::PortSink::Port(InputPort::new(NodeKey(base + g), 0)),
                from: Edge::Direct(out(base + g - 1)),
            })
            .expect("the edge wires");
        }
    }
    let sum = if heavy { 5 } else { 1 };
    ed.insert(NodeKey(30), "sum", Forkable(Kind::Sum { inputs: sum }));
    ed.connect_edge(tutti_graph::EdgeId {
        sink: tutti_graph::PortSink::Port(InputPort::new(NodeKey(30), 0)),
        from: Edge::Direct(out(10)),
    })
    .expect("the edge wires");
    for c in (0..4u16).filter(|_| heavy) {
        ed.connect_edge(tutti_graph::EdgeId {
            sink: tutti_graph::PortSink::Port(InputPort::new(NodeKey(30), c + 1)),
            from: Edge::Direct(out(103 + u64::from(c) * 10)),
        })
        .expect("the edge wires");
    }
    ed.set_outputs([out(30)]).expect("the sources exist");
    let tap = tap.then(|| {
        ed.open_event_tap(
            EventTapId::of(1),
            EventOutputPort::new(NodeKey(1), 0),
            EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
        )
        .expect("the port exists")
    });
    (ed, exec, writers, tap)
}

fn commit(ed: &mut Editor, exec: &mut Executor) {
    ed.commit().expect("compiles");
    exec.apply_pending();
    ed.collect();
}

/// Renders `total` frames in blocks of `sizes` in turn and returns the audio's
/// bits.
fn render(ed: &mut Editor, exec: &mut Executor, sizes: &[usize], total: usize) -> Vec<u32> {
    let mut bits = Vec::new();
    for n in cycle_blocks(sizes, total) {
        let chan = common::block(exec, n, 1);
        ed.collect();
        bits.extend(chan[0].iter().map(|s| s.to_bits()));
    }
    bits
}

/// What the three producers push: a mix of `NextBlock`, frames on block edges
/// of every size below, equal frames, and note-offs.
fn script(k: u32) -> Vec<(Stamp, Ump)> {
    let mut v = vec![(Stamp::NextBlock, note(k))];
    for i in 0..30u32 {
        let frame = u64::from(i) * (13 + u64::from(k)) + u64::from(k);
        let ump = if i % 4 == 3 { off(i) } else { note(i + k) };
        v.push((Stamp::Frame(Frame::new(frame)), ump));
    }
    v.push((Stamp::Frame(Frame::new(100)), note(1)));
    v.push((Stamp::Frame(Frame::new(100)), off(1)));
    v
}

/// A pooled render with the rings pre-filled equals the one-participant one, bit for
/// bit in its audio, with inlet 1's tap listing every pushed event at its
/// frame (the one-participant run is also checked against the script); every
/// participant runs inside `RtScope`.
#[test]
fn the_parallel_render_equals_the_one_participant_one_and_allocates_nothing() {
    let run = |workers: usize| {
        let gated = workers > 1;
        let (mut ed, mut exec, mut writers, tap) = graph(3, workers, gated, true, true);
        for (k, w) in writers.iter_mut().enumerate() {
            for (at, ump) in script(k as u32 + 1) {
                w.push(at, ump).unwrap();
            }
        }
        commit(&mut ed, &mut exec);
        assert_eq!(exec.is_parallel(), gated);
        let bits = render(&mut ed, &mut exec, &[100, 37, 128], 600);
        let mut tap = tap.expect("opened");
        let mut got = Vec::new();
        let mut buf = [TapEvent {
            frame: Frame::new(0),
            ump: Ump::default(),
        }; 64];
        loop {
            let n = tap.pop_into(&mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(tap.take_lost(), 0);
        (bits, got, writers[0].counts().losses().late)
    };
    let (serial_bits, serial_tap, late) =
        common::agree_across_workers([2, 3, 4], run, |(bits, tap, _)| (bits.clone(), tap.clone()));
    assert!(serial_bits.iter().any(|&b| b != 0), "the inlets are heard");
    // Inlet 1's script, in due order: `NextBlock` at frame 0, then each frame
    // in turn (the stable sort keeps a note-on before its note-off at 100).
    let mut want: Vec<(u64, Ump)> = script(1)
        .into_iter()
        .map(|(at, u)| match at {
            Stamp::Frame(f) => (f.get(), u),
            _ => (0, u),
        })
        .collect();
    want.sort_by_key(|&(f, _)| f);
    let want: Vec<TapEvent> = want
        .into_iter()
        .map(|(f, ump)| TapEvent {
            frame: Frame::new(f),
            ump,
        })
        .collect();
    assert_eq!(serial_tap, want);
    assert_eq!(late, 0);
}

/// The fork is silent: an export of a graph holding an inlet commits (the
/// audio inlet would refuse it), renders no event from it, and takes none
/// pushed afterwards, while the live graph hears the same pushes.
#[test]
fn the_fork_is_silent() {
    let (mut ed, mut exec, mut writers, _) = graph(1, 1, false, false, false);
    writers[0].push(Frame::new(10), note(5)).unwrap();
    commit(&mut ed, &mut exec);
    let (mut fed, mut fexec) = ed
        .fork(
            ForkTarget::MainOutput,
            common::prepare(MAX_BLOCK).with_render_mode(RenderMode::Offline),
        )
        .expect("forks");
    fexec.apply_pending();
    let live = render(&mut ed, &mut exec, &[64], 64);
    assert!(live.iter().any(|&b| b != 0), "the live graph hears it");
    writers[0].push(Stamp::NextBlock, note(6)).unwrap();
    let silent = render(&mut fed, &mut fexec, &[64], 128);
    assert!(silent.iter().all(|&b| b == 0), "the fork renders silence");
}
