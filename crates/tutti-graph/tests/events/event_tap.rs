//! Event taps as plan ops: one event output of any node copied out with the
//! frame each MIDI event was due on, whatever the block sizes were; aligned by
//! the compiler or raw; counted when the reader is slow, with a note-off kept
//! over any other event; reading nothing, and counting no loss, when the port
//! goes missing; a closed tap compiles to no op, a fork carries none;
//! bit-identical on the parallel executor and allocation-free on every
//! participant.
//!
//! Every figure is analytic (a script's frames, a declared latency, the ring's
//! capacity), none read from another render.
//!
//! Not covered here, by construction: a block with no reader writes nothing and
//! counts nothing. The count lives in an `Arc` only the reader reads, so once
//! the reader is dropped there is nobody to observe it; `NodeMode` (bypass
//! and mute) is not in the graph yet, so a bypassed source's silence is
//! M1's to test.

use tutti_graph::testing as common;

use common::{Kind, TestNode, prepare};

use tutti_fixtures::midi;
use tutti_graph::{CloseError, EventCapacity, EventTapId, EventTapOptions, EventTapReader};
use tutti_graph::{
    Cx, Edge, Editor, Event, EventEdge, EventInputPort, EventKind, EventOutputPort, Executor,
    ForkByClone, ForkCause, ForkSource, ForkTarget, Forked, InputPort, IntoNode, Io, MissingSource,
    Node, NodeKey, NodeParts, Offset, Op, OutputPort, PortSource, Prepare, RenderMode, Shape,
    Status, TapAlign, TapEvent, TransportPosition, Unforkable, note_off_room,
};
use tutti_graph::{NodeMode, Scheduled};
use tutti_ump::{CCNumber, Message, MidiChannel, U7, U14, Ump, Voice1};
use tutti_units::{At, Frame, Latency, Samples};

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

/// MIDI 1.0, group 0, channel 0, note-on, velocity 100.
fn on(key: u32) -> Ump {
    midi::m1_note_on(MidiChannel::FIRST, key as u8, U7::of(100))
}

fn off(key: u32) -> Ump {
    midi::m1_note_off(MidiChannel::FIRST, key as u8, U7::of(0))
}

/// What a port of the script node carries: the script's packet with word 1
/// set to the port, so a reader can tell the ports apart.
fn on_port(ump: Ump, port: u16) -> Ump {
    Ump([ump.0[0], u32::from(port), 0, 0])
}

/// Plays a script, `(frame, packet)` in frame order, on each of `ports` event
/// outputs (the port number in word 1), each event on its absolute frame. With
/// `resync`, port 0 also carries a `Resync` at the start of every block: a
/// graph event that is not MIDI.
#[derive(Clone)]
struct Script {
    events: Vec<(u64, Ump)>,
    ports: u16,
    cap: u32,
    latency: usize,
    resync: bool,
}

impl Script {
    fn new(events: Vec<(u64, Ump)>) -> Self {
        Self {
            events,
            ports: 1,
            cap: 64,
            latency: 0,
            resync: false,
        }
    }
    fn ports(mut self, ports: u16) -> Self {
        self.ports = ports;
        self
    }
    fn latency(mut self, latency: usize) -> Self {
        self.latency = latency;
        self
    }
    fn resync(mut self) -> Self {
        self.resync = true;
        self
    }
}

impl Node for Script {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .events(0, self.ports)
            .event_capacity(tutti_graph::testing::cap_of(self.cap))
            .latency(Latency::new(Samples::new(self.latency)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (p, port) in (0..self.ports).zip(self.shape().event_outputs()) {
            let out = io.event_out(port);
            if p == 0 && self.resync {
                let _ = out.push(Event::resync(Offset::ZERO));
            }
            for &(f, ump) in &self.events {
                if let Some(at) = cx.env.offset_of(Frame::new(f)) {
                    let _ = out.push(Event::midi(at, on_port(ump, p)));
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn out(node: u64) -> PortSource {
    PortSource::Node(OutputPort::new(NodeKey(node), 0))
}

fn port(node: u64, port: u16) -> EventOutputPort {
    EventOutputPort::new(NodeKey(node), port)
}

struct Rig {
    ed: Editor,
    exec: Executor,
    gated: bool,
}

/// An editor with the latent audio path `Const -> Lag(m)` on the only output
/// (so the graph's output time is `m` frames).
fn rig(workers: usize, no_alloc: bool, m: usize) -> Rig {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(if no_alloc {
            common::TestPool::new(workers)
        } else {
            common::test_pool(workers)
        }));
    }
    ed.insert(
        NodeKey(90),
        "const",
        Forkable(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    ed.insert(NodeKey(91), "lag", Forkable(Kind::Lag { latency: m }));
    let t = &mut ed.spec_mut().topology;
    t.connect(InputPort::new(NodeKey(91), 0), Edge::Direct(out(90)));
    t.set_outputs([out(91)]);
    Rig {
        ed,
        exec,
        gated: no_alloc,
    }
}

fn script(r: &mut Rig, key: u64, s: Script) {
    r.ed.insert(NodeKey(key), "script", ForkByClone(s));
}

fn commit(r: &mut Rig) {
    r.ed.commit().expect("compiles");
    r.exec.apply_pending();
    r.ed.collect();
}

/// Renders `total` frames in blocks of `sizes` taken in turn (the last one cut
/// to fit), starting at the executor's current frame.
fn render(r: &mut Rig, sizes: &[usize], total: usize) {
    let t = TransportPosition::default();
    let (mut done, mut i) = (0, 0);
    while done < total {
        let n = sizes[i % sizes.len()].min(total - done);
        let mut chan = vec![0.0f32; n];
        {
            let mut outs: Vec<&mut [f32]> = vec![chan.as_mut_slice()];
            r.exec.process(n, &t, &[], &mut outs);
        }
        r.ed.collect();
        done += n;
        i += 1;
    }
}

fn drain(tap: &mut EventTapReader) -> Vec<TapEvent> {
    let mut all = Vec::new();
    let mut buf = [TapEvent {
        frame: Frame::new(0),
        ump: Ump::default(),
    }; 64];
    loop {
        let n = tap.pop_into(&mut buf);
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

fn at(frame: u64, ump: Ump) -> TapEvent {
    TapEvent {
        frame: Frame::new(frame),
        ump,
    }
}

fn tap_ops(r: &Rig) -> usize {
    r.ed.base()
        .expect("a plan")
        .ops()
        .iter()
        .filter(|o| matches!(o, Op::EventTap { .. }))
        .count()
}

fn event_delays(r: &Rig) -> usize {
    r.ed.base()
        .expect("a plan")
        .ops()
        .iter()
        .filter(|o| matches!(o, Op::EventDelay { .. }))
        .count()
}

/// A script with events on, before and after block edges of every size below.
fn edges() -> Vec<(u64, Ump)> {
    [
        0u64, 1, 2, 2, 6, 7, 8, 63, 64, 65, 99, 100, 101, 511, 512, 513, 1000, 1023, 1024, 1999,
    ]
    .iter()
    .enumerate()
    .map(|(i, &f)| (f, on(i as u32)))
    .collect()
}

/// Every event lands at its absolute frame, whatever the blocks were: the tap
/// reads the script back, frame for frame and packet for packet, at block
/// sizes 1, 7, 64, 100 and 512, and at a mix (including events on a block's
/// first and last frame and two on one frame).
#[test]
fn every_event_lands_at_its_frame_across_block_sizes() {
    let want: Vec<TapEvent> = edges()
        .into_iter()
        .map(|(f, u)| at(f, on_port(u, 0)))
        .collect();
    for sizes in [
        &[1usize][..],
        &[7],
        &[64],
        &[100],
        &[512],
        &[1, 7, 64, 100, 512],
    ] {
        let mut r = rig(1, false, 0);
        script(&mut r, 1, Script::new(edges()));
        let mut tap =
            r.ed.open_event_tap(
                EventTapId::of(1),
                port(1, 0),
                EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
            )
            .expect("the port exists");
        assert_eq!(tap.rate(), common::RATE, "frames are on the graph's rate");
        commit(&mut r);
        render(&mut r, sizes, 2000);
        assert_eq!(drain(&mut tap), want, "block sizes {sizes:?}");
        assert_eq!(tap.take_lost(), 0);
    }
}

/// A reader that never drains: the ring takes `capacity` events of any kind,
/// then only note-offs up to `note_off_room(capacity)`, and every refusal is
/// counted once, in events. Eight note-ons then twenty-four of the packets
/// (twenty note-ons, twenty note-offs, in that order) at capacity 8: the first
/// 8 note-ons are held, 12 refused; the note-offs fill the reserve
/// (`note_off_room(8) = 17`, 9 of them held), 11 refused. 23 lost in all, and
/// the read clears the count.
#[test]
fn a_slow_reader_loses_events_counted_and_a_note_off_is_kept() {
    assert_eq!(note_off_room(8), 17);
    let ons = (0..20u32).map(|k| (u64::from(k), on(k)));
    let offs = (0..20u32).map(|k| (20 + u64::from(k), off(k)));
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(ons.chain(offs).collect()));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    assert_eq!(tap.available(), 17);
    let got = drain(&mut tap);
    let want: Vec<TapEvent> = (0..8u32)
        .map(|k| at(u64::from(k), on_port(on(k), 0)))
        .chain((0..9u32).map(|k| at(20 + u64::from(k), on_port(off(k), 0))))
        .collect();
    assert_eq!(
        got, want,
        "the first note-ons, then the note-offs the reserve held"
    );
    assert_eq!(tap.take_lost(), 12 + 11);
    assert_eq!(tap.take_lost(), 0, "the read clears the count");
}

/// `drain` yields exactly what was written, in order, and leaves nothing
/// behind: the script's twenty events come back frame for frame, the ring is
/// empty after, and a second `drain` yields none. Read inside `assert_no_alloc`
/// into a fixed array, so the iterator allocates nothing.
#[test]
fn drain_yields_what_was_written_in_order_and_leaves_nothing() {
    let want: Vec<TapEvent> = edges()
        .into_iter()
        .map(|(f, u)| at(f, on_port(u, 0)))
        .collect();
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(edges()));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 2000);
    assert_eq!(tap.available(), want.len());
    let mut got = [at(0, Ump::default()); 64];
    let mut n = 0;
    assert_no_alloc::assert_no_alloc(|| {
        for event in tap.drain() {
            got[n] = event;
            n += 1;
        }
    });
    assert_eq!(&got[..n], &want[..], "in order, packet for packet");
    assert_eq!(tap.available(), 0, "nothing left behind");
    assert_eq!(tap.drain().count(), 0, "and a second drain is empty");
}

/// `drain` does not change how a slow reader's losses are counted: with the
/// ring of the test above (capacity 8, 23 refusals), draining yields the 17
/// held events and `take_lost` still answers 23 once, then 0.
#[test]
fn drain_leaves_the_loss_count_to_take_lost() {
    let ons = (0..20u32).map(|k| (u64::from(k), on(k)));
    let offs = (0..20u32).map(|k| (20 + u64::from(k), off(k)));
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(ons.chain(offs).collect()));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    assert_eq!(tap.drain().count(), 17, "what the ring held");
    assert_eq!(
        tap.take_lost(),
        12 + 11,
        "the drain did not clear the count"
    );
    assert_eq!(tap.take_lost(), 0);
}

/// Dropped early, the iterator keeps what it did not yield: three of the
/// twenty are read, seventeen stay for the next read.
#[test]
fn a_drain_dropped_early_leaves_the_rest() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(edges()));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 2000);
    let first: Vec<TapEvent> = tap.drain().take(3).collect();
    assert_eq!(first.len(), 3);
    assert_eq!(tap.available(), 17);
    assert_eq!(drain(&mut tap).first(), Some(&at(2, on_port(on(3), 0))));
}

/// A port that narrows or vanishes after the open reads nothing: the commit
/// succeeds, the plan has no op for the tap, nothing is counted lost, the
/// other tap plays on, and restoring the port resumes the stream (the writer
/// stays in the plan's table).
#[test]
fn a_narrowed_port_reads_nothing_and_the_stream_resumes() {
    let mut r = rig(1, false, 0);
    let s = Script::new(vec![(10, on(1)), (150, on(2)), (250, on(3))]).ports(2);
    script(&mut r, 1, s.clone());
    let mut wide =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 1),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    let mut first =
        r.ed.open_event_tap(
            EventTapId::of(2),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2);
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut wide), vec![at(10, on_port(on(1), 1))]);
    script(&mut r, 1, s.clone().ports(1));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 1, "no op for the port that is gone");
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut wide), vec![], "the vanished port reads nothing");
    assert_eq!(wide.take_lost(), 0, "and that is not a loss");
    script(&mut r, 1, s.ports(2));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2);
    render(&mut r, &[100], 100);
    assert_eq!(
        drain(&mut wide),
        vec![at(250, on_port(on(3), 1))],
        "the restored port resumes"
    );
    assert_eq!(
        drain(&mut first).len(),
        3,
        "the other tap played through all of it"
    );
}

/// Removing the tapped node reads nothing and commits; a node inserted again
/// under the removed key is not read by the old tap.
#[test]
fn a_removed_node_reads_nothing_and_a_reused_key_is_not_read() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![(10, on(1))]));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(drain(&mut tap).len(), 1);
    r.ed.remove(NodeKey(1));
    commit(&mut r);
    assert_eq!(tap_ops(&r), 0);
    script(&mut r, 1, Script::new(vec![(150, on(7))]));
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(
        drain(&mut tap),
        vec![],
        "the tap followed the removal, not the key"
    );
    assert_eq!(tap.take_lost(), 0);
}

/// A source behind a node of declared latency L = 15, beside an output path of
/// latency M = 40: the raw tap reads the script's own frames, the aligned one
/// the script's plus the compiler's gap (M - L = 25, an analytic figure); a
/// source with no latency is M late when aligned. Aligned taps cost an event
/// delay each, raw ones none.
#[test]
fn aligned_against_raw_on_a_latent_path() {
    for sizes in [&[64usize][..], &[100], &[1, 7, 64]] {
        let mut r = rig(1, false, 40);
        let events = vec![(3, on(1)), (10, on(2)), (100, on(3))];
        script(&mut r, 1, Script::new(events.clone()));
        script(&mut r, 2, Script::new(events.clone()).latency(15));
        let open = |r: &mut Rig, id, node, align| {
            r.ed.open_event_tap(
                EventTapId::of(id),
                port(node, 0),
                EventTapOptions::new(EventCapacity::of(64)).align(align),
            )
            .expect("the port exists")
        };
        let mut raw1 = open(&mut r, 1, 1, TapAlign::Raw);
        let mut aligned1 = open(&mut r, 2, 1, TapAlign::ToOutput);
        let mut raw2 = open(&mut r, 3, 2, TapAlign::Raw);
        let mut aligned2 = open(&mut r, 4, 2, TapAlign::ToOutput);
        commit(&mut r);
        assert_eq!(tap_ops(&r), 4);
        assert_eq!(event_delays(&r), 2, "one delay for each aligned tap");
        render(&mut r, sizes, 300);
        let shifted = |by: u64| -> Vec<TapEvent> {
            events
                .iter()
                .map(|&(f, u)| at(f + by, on_port(u, 0)))
                .collect()
        };
        assert_eq!(drain(&mut raw1), shifted(0), "{sizes:?}");
        assert_eq!(drain(&mut aligned1), shifted(40), "{sizes:?}");
        assert_eq!(drain(&mut raw2), shifted(0), "{sizes:?}");
        assert_eq!(drain(&mut aligned2), shifted(25), "{sizes:?}");
    }
}

/// A closed tap leaves no op (and no delay), nothing is written for it, a fork
/// carries none, opening an id again replaces the tap, and a reader dropped
/// first is pruned on the control side.
#[test]
fn closed_fork_reopen_and_prune() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![(10, on(1)), (110, on(2))]));
    let open = |r: &mut Rig, id| {
        r.ed.open_event_tap(
            EventTapId::of(id),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists")
    };
    let closed = open(&mut r, 1);
    commit(&mut r);
    assert_eq!(tap_ops(&r), 1);
    r.ed.close_event_tap(closed.id()).unwrap();
    assert_eq!(
        r.ed.close_event_tap(closed.id()),
        Err(CloseError::EventTap(closed.id())),
        "a closed tap is not open"
    );
    assert_eq!(
        r.ed.close_event_tap(EventTapId::of(99)),
        Err(CloseError::EventTap(EventTapId::of(99))),
        "a tap never opened is not open"
    );
    commit(&mut r);
    assert_eq!(tap_ops(&r), 0, "a closed tap leaves no op");
    drop(closed);

    // A fork carries no event tap.
    let live = open(&mut r, 2);
    commit(&mut r);
    let (fed, _fexec) =
        r.ed.fork(
            ForkTarget::MainOutput,
            prepare(MAX_BLOCK).with_render_mode(RenderMode::Offline),
        )
        .expect("forks");
    assert!(fed.spec().topology.event_taps.is_empty());

    // Opening an id again replaces the tap.
    let mut second = open(&mut r, 2);
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(live.available(), 0, "the old reader is fed no more");
    assert_eq!(drain(&mut second).len(), 1);

    // An id opened twice between commits: the second replaces the first.
    let stale = open(&mut r, 5);
    let mut fresh = open(&mut r, 5);
    commit(&mut r);
    render(&mut r, &[100], 100);
    assert_eq!(stale.available(), 0, "the replaced reader is never fed");
    assert_eq!(drain(&mut fresh), vec![at(110, on_port(on(2), 0))]);
    r.ed.close_event_tap(EventTapId::of(5)).unwrap();
    drop((stale, fresh));

    // A reader dropped first: the tap writes nothing until it is pruned.
    let keep = open(&mut r, 3);
    let gone = open(&mut r, 4);
    commit(&mut r);
    drop(gone);
    drop(live);
    assert_eq!(
        r.ed.prune_event_taps(),
        1,
        "the dropped reader's tap; the replaced one's id is the new reader's"
    );
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2, "the others stay");
    assert_eq!(r.ed.prune_event_taps(), 0);
    assert_eq!(keep.id(), EventTapId::of(3));
}

/// A reader dropped on the audio thread is a bug the debug build reports.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "EventTapReader dropped on the audio thread")]
fn a_reader_dropped_on_the_audio_thread_panics() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![]));
    let tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    let _inside = tutti_rt::RtScope::enter();
    drop(tap);
}

/// Only MIDI is copied: a port carrying a `Resync` every block beside its notes
/// gives the notes alone, and the graph event is neither copied nor counted.
#[test]
fn only_midi_is_copied() {
    let mut r = rig(1, false, 0);
    script(
        &mut r,
        1,
        Script::new(vec![(10, on(1)), (70, on(2))]).resync(),
    );
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            port(1, 0),
            EventTapOptions::new(EventCapacity::of(2)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    commit(&mut r);
    render(&mut r, &[64], 128);
    assert_eq!(
        drain(&mut tap),
        vec![at(10, on_port(on(1), 0)), at(70, on_port(on(2), 0))]
    );
    assert_eq!(tap.take_lost(), 0, "a graph event does not fill the ring");
}

/// Opening refuses a node or a port that does not exist now, edits not yet
/// committed included, and declares nothing. An audio output is not an event
/// port.
#[test]
fn opening_refuses_a_port_that_does_not_exist_now() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![]).ports(2));
    let open = |r: &mut Rig, p: EventOutputPort| {
        r.ed.open_event_tap(
            EventTapId::of(9),
            p,
            EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw),
        )
        .err()
    };
    assert_eq!(
        open(&mut r, port(1, 2)),
        Some(MissingSource::EventPort {
            node: NodeKey(1),
            port: 2,
            width: 2
        })
    );
    assert_eq!(
        open(&mut r, port(77, 0)),
        Some(MissingSource::Node { node: NodeKey(77) })
    );
    assert_eq!(
        open(&mut r, port(91, 0)),
        Some(MissingSource::EventPort {
            node: NodeKey(91),
            port: 0,
            width: 0
        }),
        "a node with audio outputs and no event output"
    );
    assert!(r.ed.spec().topology.event_taps.is_empty());
    assert_eq!(open(&mut r, port(1, 1)), None, "it exists, uncommitted");
}

/// A node with one event input that records every MIDI event it is handed,
/// with the frame it was due on: what the node's `process` saw.
#[derive(Clone)]
struct Probe {
    heard: tutti_fixtures::Record<TapEvent>,
    ports: usize,
    /// The event capacity the probe declares; 0 declares none.
    capacity: u32,
    /// Whether the probe has an event output, so a bypass discards its events
    /// (a node with no outputs is never bypassed).
    audible: bool,
}

impl Probe {
    const BASE: Shape = Shape::EMPTY.events(1, 0);
    const IN: tutti_graph::InEvents = Self::BASE.event_input(0).expect("declared");
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        let shape = if self.audible {
            Shape::EMPTY.events(self.ports as u16, 1)
        } else {
            Shape::EMPTY.events(self.ports as u16, 0)
        };
        if self.capacity > 0 {
            shape.event_capacity(tutti_graph::testing::cap_of(self.capacity))
        } else {
            shape
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut heard = self.heard.lock().unwrap();
        for e in io.events(Self::IN) {
            if let EventKind::Midi(ump) = e.kind {
                heard.push(TapEvent {
                    frame: cx.env.frame_at(e.offset),
                    ump,
                });
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Two scripts, one latent, feed one input of a probe, with events on shared
/// frames (so the merge's tie-break shows) and a note-off before a note-on.
fn fan_in(r: &mut Rig, probe_ports: usize) -> tutti_fixtures::Record<TapEvent> {
    let shared = vec![
        (5, on(1)),
        (40, off(1)),
        (40, on(2)),
        (100, on(3)),
        (101, on(4)),
    ];
    script(r, 1, Script::new(shared.clone()));
    script(r, 2, Script::new(shared).latency(15));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: probe_ports,
            capacity: 0,
            audible: false,
        }),
    );
    for from in [1, 2] {
        r.ed.spec_mut().connect_events(
            EventInputPort::new(NodeKey(3), 0),
            EventEdge::Direct(port(from, 0)),
        );
    }
    heard
}

/// A tap of an event input copies exactly what the node's `process` was
/// handed there: both sources merged in the order the executor delivers them
/// (sources in key order at equal frames), after the latency delay on the
/// shorter path, at every block size. The raw tap's frames are the node's own.
/// An aligned one is later by the gap from the node's input time (15) to the
/// output time (40).
#[test]
fn an_input_tap_reads_what_the_node_heard() {
    for (workers, sizes) in [
        (1, &[64usize][..]),
        (1, &[100]),
        (1, &[1, 7, 64]),
        (3, &[100]),
    ] {
        let mut r = rig(workers, false, 40);
        let heard = fan_in(&mut r, 1);
        let input = EventInputPort::new(NodeKey(3), 0);
        let mut raw =
            r.ed.open_event_tap(
                EventTapId::of(1),
                input,
                EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
            )
            .expect("the input exists");
        let mut aligned =
            r.ed.open_event_tap(
                EventTapId::of(2),
                input,
                EventTapOptions::new(EventCapacity::of(64)),
            )
            .expect("the input exists");
        commit(&mut r);
        assert_eq!(tap_ops(&r), 2);
        render(&mut r, sizes, 300);
        let node_heard = heard.lock().unwrap().to_vec();
        assert_eq!(node_heard.len(), 10, "both sources reached the node");
        assert_eq!(drain(&mut raw), node_heard, "{sizes:?}");
        let later: Vec<TapEvent> = node_heard
            .iter()
            .map(|e| at(e.frame.get() + 25, e.ump))
            .collect();
        assert_eq!(drain(&mut aligned), later, "{sizes:?}");
        assert_eq!(raw.take_lost(), 0);
    }
}

/// An input with a single source is tapped as that source's own slot, and an
/// input nothing feeds reads no events. Both match what the node heard.
#[test]
fn an_input_tap_of_one_source_and_of_none() {
    let mut r = rig(1, false, 0);
    let heard = fan_in(&mut r, 2);
    r.ed.spec_mut()
        .disconnect_events(EventInputPort::new(NodeKey(3), 0), port(2, 0));
    let mut one =
        r.ed.open_event_tap(
            EventTapId::of(1),
            EventInputPort::new(NodeKey(3), 0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    let mut none =
        r.ed.open_event_tap(
            EventTapId::of(2),
            EventInputPort::new(NodeKey(3), 1),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    render(&mut r, &[64], 200);
    let node_heard = heard.lock().unwrap().to_vec();
    assert_eq!(node_heard.len(), 5);
    assert_eq!(drain(&mut one), node_heard);
    assert!(drain(&mut none).is_empty());
    assert_eq!(none.take_lost(), 0);
}

/// What an aligned input tap holds is what the node can receive, not what one
/// port writes: two sources declaring 16 events each, writing 16 on one frame,
/// feed a probe that declares 1, and the delayed tap loses none of the 32
/// the probe heard.
#[test]
fn an_aligned_input_tap_is_sized_for_every_source_of_the_input() {
    let mut r = rig(1, false, 40);
    let burst: Vec<(u64, Ump)> = (0..16).map(|k| (10, on(k))).collect();
    for key in [1, 2] {
        let mut s = Script::new(burst.clone());
        s.cap = 16;
        script(&mut r, key, s);
    }
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 1,
            audible: false,
        }),
    );
    for from in [1, 2] {
        r.ed.spec_mut().connect_events(
            EventInputPort::new(NodeKey(3), 0),
            EventEdge::Direct(port(from, 0)),
        );
    }
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            EventInputPort::new(NodeKey(3), 0),
            EventTapOptions::new(EventCapacity::of(64)),
        )
        .expect("the input exists");
    commit(&mut r);
    assert_eq!(event_delays(&r), 1, "the aligned tap is delayed");
    render(&mut r, &[64], 200);
    let node_heard = heard.lock().unwrap().to_vec();
    assert_eq!(node_heard.len(), 32);
    let later: Vec<TapEvent> = node_heard
        .iter()
        .map(|e| at(e.frame.get() + 40, e.ump))
        .collect();
    assert_eq!(drain(&mut tap), later);
    assert_eq!(tap.take_lost(), 0);
}

/// Opening an input tap refuses a node that is not there and a port past the
/// node's event inputs, and a node's event output is not an input.
#[test]
fn opening_an_input_tap_refuses_a_port_that_does_not_exist_now() {
    let mut r = rig(1, false, 0);
    let _heard = fan_in(&mut r, 1);
    let open = |r: &mut Rig, node: u64, port: u16| {
        r.ed.open_event_tap(
            EventTapId::of(9),
            EventInputPort::new(NodeKey(node), port),
            EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw),
        )
        .err()
    };
    assert_eq!(
        open(&mut r, 3, 1),
        Some(MissingSource::EventInputPort {
            node: NodeKey(3),
            port: 1,
            width: 1
        })
    );
    assert_eq!(
        open(&mut r, 1, 0),
        Some(MissingSource::EventInputPort {
            node: NodeKey(1),
            port: 0,
            width: 0
        }),
        "a script has event outputs and no input"
    );
    assert_eq!(
        open(&mut r, 77, 0),
        Some(MissingSource::Node { node: NodeKey(77) })
    );
    assert_eq!(open(&mut r, 3, 0), None);
}

/// Six scripted nodes of two ports each, every port tapped (raw and aligned
/// alternately) on a path whose output time is 40 frames.
fn busy(r: &mut Rig) -> Vec<EventTapReader> {
    let mut taps = Vec::new();
    for k in 1..=6u64 {
        let events: Vec<(u64, Ump)> = (0..40u64)
            .map(|i| (i * (9 + k) + k, on((i % 100) as u32)))
            .collect();
        script(r, k, Script::new(events).ports(2).latency(3 * k as usize));
        for p in 0..2u16 {
            let align = if (k + u64::from(p)) % 2 == 0 {
                TapAlign::Raw
            } else {
                TapAlign::ToOutput
            };
            let id = EventTapId::of(k * 2 + u64::from(p) + 1);
            taps.push(
                r.ed.open_event_tap(
                    id,
                    port(k, p),
                    EventTapOptions::new(EventCapacity::of(1 << 10)).align(align),
                )
                .expect("the port exists"),
            );
        }
    }
    commit(r);
    taps
}

fn run_busy(r: &mut Rig) -> Vec<Vec<TapEvent>> {
    let mut taps = busy(r);
    assert_eq!(r.exec.is_parallel(), r.gated, "a pool was set iff gated");
    render(r, &[100, 37, 128], 600);
    let got: Vec<Vec<TapEvent>> = taps.iter_mut().map(drain).collect();
    for t in &taps {
        assert_eq!(t.take_lost(), 0);
    }
    got
}

/// The pooled render's taps equal the one-participant one's, event for event, at
/// several worker counts with every participant inside `RtScope`; the
/// one-participant run itself is checked against the script and the compensation.
#[test]
fn the_parallel_event_taps_are_bit_identical_and_allocation_free() {
    let mut s = rig(1, false, 40);
    let serial = run_busy(&mut s);
    assert!(
        serial.iter().all(|t| !t.is_empty()),
        "every tap carries events"
    );
    // Tap 0 is node 1 port 0, aligned (1 + 0 is odd): the script's events
    // shifted by the gap between the output time (40) and its latency (3).
    let first: Vec<TapEvent> = (0..40u64)
        .map(|i| (i * 10 + 1 + 37, on((i % 100) as u32)))
        .filter(|&(f, _)| f < 600)
        .map(|(f, u)| at(f, on_port(u, 0)))
        .collect();
    assert_eq!(serial[0], first);
    for workers in [2, 3, 4] {
        let mut p = rig(workers, true, 40);
        assert_eq!(serial, run_busy(&mut p), "{workers} workers");
    }
}

/// MIDI 1.0 sustain pedal (CC 64) at `value`.
fn pedal(value: u8) -> Ump {
    midi::m1_cc(MidiChannel::FIRST, CCNumber::SUSTAIN, U7::of(value))
}

/// MIDI 1.0 pitch bend on channel 0.
fn bend(value: u16) -> Ump {
    Ump::from(Message::midi1(
        MidiChannel::FIRST,
        Voice1::PitchBend(U14::of(value)),
    ))
}

/// What a cut edge into a probe leaves behind, per probe: what it heard and
/// what its raw and its aligned input tap carried.
struct Cut {
    heard: Vec<TapEvent>,
    raw: Vec<TapEvent>,
    aligned: Vec<TapEvent>,
}

/// `pairs` scripts each holding a key, the sustain pedal and an off-centre
/// bend feed one probe each over an event edge, with an input tap on every
/// probe (raw and aligned); the edges are cut after the first block and two
/// more blocks render on `workers` participants.
fn cut_edges(workers: usize, pairs: u64) -> Vec<Cut> {
    let mut r = rig(workers, workers > 1, 40);
    let held = vec![(3, on(60)), (5, pedal(127)), (7, bend(0))];
    let mut taps = Vec::new();
    for i in 0..pairs {
        r.ed.insert(
            NodeKey(10 + i),
            "script",
            Unforkable(Script::new(held.clone())),
        );
        let heard = tutti_fixtures::Record::new();
        r.ed.insert(
            NodeKey(20 + i),
            "probe",
            ForkByClone(Probe {
                heard: heard.clone(),
                ports: 1,
                capacity: 0,
                audible: false,
            }),
        );
        let input = EventInputPort::new(NodeKey(20 + i), 0);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(10 + i, 0)));
        let open = |r: &mut Rig, id, align| {
            r.ed.open_event_tap(
                EventTapId::of(id),
                input,
                EventTapOptions::new(EventCapacity::of(64)).align(align),
            )
            .expect("the input exists")
        };
        let raw = open(&mut r, 2 * i + 1, TapAlign::Raw);
        let aligned = open(&mut r, 2 * i + 2, TapAlign::ToOutput);
        taps.push((heard, raw, aligned));
    }
    commit(&mut r);
    render(&mut r, &[64], 64);
    for i in 0..pairs {
        let input = EventInputPort::new(NodeKey(20 + i), 0);
        assert!(r.ed.spec_mut().disconnect_events(input, port(10 + i, 0)));
    }
    commit(&mut r);
    assert_eq!(r.exec.is_parallel(), workers > 1 && pairs > 1);
    render(&mut r, &[64], 128);
    taps.into_iter()
        .map(|(heard, mut raw, mut aligned)| Cut {
            heard: heard.lock().unwrap().to_vec(),
            raw: drain(&mut raw),
            aligned: drain(&mut aligned),
        })
        .collect()
}

/// **An input tap carries what the node is handed, the closes of a cut edge
/// included.** A script holds key 60, the sustain pedal and an off-centre
/// bend on an edge that is then cut: the node is sent, at the first frame of
/// the next block, the pedal up, the bend centred (0x2000) and the key's note
/// off, and the raw tap carries exactly those at those frames. The aligned
/// one is later by the gap to the output time (40).
#[test]
fn an_input_tap_carries_the_closes_of_a_cut_edge() {
    let cut = &cut_edges(1, 1)[0];
    let expect = vec![
        at(3, on(60)),
        at(5, pedal(127)),
        at(7, bend(0)),
        at(64, pedal(0)),
        at(64, bend(0x2000)),
        at(64, off(60)),
    ];
    assert_eq!(cut.heard, expect, "what the node was handed");
    assert_eq!(cut.raw, expect);
    let later: Vec<TapEvent> = expect
        .iter()
        .map(|e| at(e.frame.get() + 40, e.ump))
        .collect();
    assert_eq!(cut.aligned, later);
}

/// The taps of several probes on the pool carry what the one-participant
/// render's do, event for event.
#[test]
fn the_closes_of_cut_edges_reach_input_taps_on_the_pool() {
    let serial = cut_edges(1, 4);
    for workers in [2, 3] {
        let pooled = cut_edges(workers, 4);
        for (s, p) in serial.iter().zip(&pooled) {
            assert_eq!(s.raw, p.raw, "{workers} workers");
            assert_eq!(s.aligned, p.aligned, "{workers} workers");
        }
    }
    assert!(
        serial
            .iter()
            .all(|c| c.raw.len() == 6 && c.aligned.len() == 6)
    );
}

/// **A scheduled event command to an input is in the input tap**, at the
/// frame it is delivered on, after the edge's events of that frame.
#[test]
fn an_input_tap_carries_a_scheduled_event() {
    for align in [TapAlign::Raw, TapAlign::ToOutput] {
        let mut r = rig(1, false, 40);
        script(&mut r, 1, Script::new(vec![(3, on(1))]));
        let heard = tutti_fixtures::Record::new();
        r.ed.insert(
            NodeKey(3),
            "probe",
            ForkByClone(Probe {
                heard: heard.clone(),
                ports: 1,
                capacity: 0,
                audible: false,
            }),
        );
        let input = EventInputPort::new(NodeKey(3), 0);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(1, 0)));
        let mut tap =
            r.ed.open_event_tap(
                EventTapId::of(1),
                input,
                EventTapOptions::new(EventCapacity::of(64)).align(align),
            )
            .expect("the input exists");
        commit(&mut r);
        r.ed.schedule(
            At::Frame(Frame::new(70)),
            input,
            Scheduled::Event(EventKind::Midi(on(5))),
        )
        .expect("room");
        render(&mut r, &[64], 128);
        let by = if align == TapAlign::Raw { 0 } else { 40 };
        assert_eq!(
            heard.lock().unwrap().to_vec(),
            vec![at(3, on_port(on(1), 0)), at(70, on(5))]
        );
        assert_eq!(
            drain(&mut tap),
            vec![at(3 + by, on_port(on(1), 0)), at(70 + by, on(5))],
            "{align:?}"
        );
    }
}

/// **A bypassed node's input tap carries what the bypass sends it**: the
/// close of the key it held, and none of the events it discards.
#[test]
fn a_bypassed_nodes_input_tap_carries_its_closes() {
    for align in [TapAlign::Raw, TapAlign::ToOutput] {
        let mut r = rig(1, false, 40);
        r.ed.insert(
            NodeKey(1),
            "script",
            Unforkable(Script::new(vec![(3, on(60)), (100, on(2))])),
        );
        let heard = tutti_fixtures::Record::new();
        r.ed.insert(
            NodeKey(3),
            "probe",
            Unforkable(Probe {
                heard: heard.clone(),
                ports: 1,
                capacity: 0,
                audible: true,
            }),
        );
        let input = EventInputPort::new(NodeKey(3), 0);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(1, 0)));
        let mut tap =
            r.ed.open_event_tap(
                EventTapId::of(1),
                input,
                EventTapOptions::new(EventCapacity::of(64)).align(align),
            )
            .expect("the input exists");
        commit(&mut r);
        render(&mut r, &[64], 64);
        r.ed.set_mode(NodeKey(3), NodeMode::Bypassed)
            .expect("a node");
        commit(&mut r);
        render(&mut r, &[64], 192);
        let by = if align == TapAlign::Raw { 0 } else { 40 };
        let node = vec![at(3, on_port(on(60), 0)), at(64, off(60))];
        assert_eq!(heard.lock().unwrap().to_vec(), node, "{align:?}");
        let tapped: Vec<TapEvent> = node.iter().map(|e| at(e.frame.get() + by, e.ump)).collect();
        assert_eq!(drain(&mut tap), tapped, "{align:?}");
    }
}

/// A probe with a declared input bound, and an event output unless `audible`
/// is false.
#[derive(Clone)]
struct Bounded {
    probe: Probe,
    bound: u32,
}

impl Node for Bounded {
    fn shape(&self) -> Shape {
        self.probe
            .shape()
            .event_input_capacity(std::num::NonZeroU32::new(self.bound).expect("a bound"))
    }
    fn prepare(&mut self, p: &Prepare) {
        self.probe.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.probe.process(cx, io)
    }
    fn reset(&mut self) {}
}

/// A source writing `on(1)` at 2, `on(2)` at 4 and `off(1)` at 8 feeds a probe
/// bounded at 3, and a command to it at 3 brings a fourth event. Seen by the
/// probe, and so by its taps, are the earliest that fit with the note-off
/// kept: the node-wide cut, not the merge in front of it, is what drops
/// `on(2)`. Raw and aligned, serially and on the pool.
fn bounded(workers: usize, pairs: u64) -> Vec<Cut> {
    let mut r = rig(workers, workers > 1, 40);
    let mut taps = Vec::new();
    for i in 0..pairs {
        script(
            &mut r,
            10 + i,
            Script::new(vec![(2, on(1)), (4, on(2)), (8, off(1))]),
        );
        let heard = tutti_fixtures::Record::new();
        r.ed.insert(
            NodeKey(20 + i),
            "probe",
            ForkByClone(Bounded {
                probe: Probe {
                    heard: heard.clone(),
                    ports: 1,
                    capacity: 0,
                    audible: false,
                },
                bound: 3,
            }),
        );
        let input = EventInputPort::new(NodeKey(20 + i), 0);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(10 + i, 0)));
        let open = |r: &mut Rig, id, align| {
            r.ed.open_event_tap(
                EventTapId::of(id),
                input,
                EventTapOptions::new(EventCapacity::of(64)).align(align),
            )
            .expect("the input exists")
        };
        let raw = open(&mut r, 2 * i + 1, TapAlign::Raw);
        let aligned = open(&mut r, 2 * i + 2, TapAlign::ToOutput);
        taps.push((input, heard, raw, aligned));
    }
    commit(&mut r);
    for (input, ..) in &taps {
        r.ed.schedule(
            At::Frame(Frame::new(3)),
            *input,
            Scheduled::Event(EventKind::Midi(on(9))),
        )
        .expect("room");
    }
    assert_eq!(r.exec.is_parallel(), workers > 1 && pairs > 1);
    render(&mut r, &[64], 64);
    assert_eq!(r.exec.dropped_event_count(), pairs, "the cut is counted");
    taps.into_iter()
        .map(|(_, heard, mut raw, mut aligned)| Cut {
            heard: heard.lock().unwrap().to_vec(),
            raw: drain(&mut raw),
            aligned: drain(&mut aligned),
        })
        .collect()
}

/// **An input tap carries what the node's declared bound leaves.** The
/// scheduled `on(9)` makes four events for a bound of 3: the earliest by frame
/// land, the note-off before any other, so `on(2)` at 4 is the one cut.
#[test]
fn an_input_tap_carries_the_events_the_input_bound_leaves() {
    let cut = &bounded(1, 1)[0];
    let expect = vec![
        at(2, on_port(on(1), 0)),
        at(3, on(9)),
        at(8, on_port(off(1), 0)),
    ];
    assert_eq!(cut.heard, expect, "what the node was handed");
    assert_eq!(cut.raw, expect);
    let later: Vec<TapEvent> = expect
        .iter()
        .map(|e| at(e.frame.get() + 40, e.ump))
        .collect();
    assert_eq!(cut.aligned, later);
}

/// The cut taps of several probes on the pool equal the serial render's, event
/// for event.
#[test]
fn the_bounded_input_taps_are_bit_identical_on_the_pool() {
    let serial = bounded(1, 4);
    for workers in [2, 3] {
        let pooled = bounded(workers, 4);
        for (s, p) in serial.iter().zip(&pooled) {
            assert_eq!(s.heard, p.heard, "{workers} workers");
            assert_eq!(s.raw, p.raw, "{workers} workers");
            assert_eq!(s.aligned, p.aligned, "{workers} workers");
        }
    }
    assert!(serial.iter().all(|c| c.raw.len() == 3));
}

/// **A command to an input nothing feeds is in its taps**: the view exists
/// whatever feeds the input, and an aligned tap's delay holds the command.
#[test]
fn an_unwired_input_tap_carries_a_scheduled_command() {
    let mut r = rig(1, false, 40);
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    let open = |r: &mut Rig, id, align| {
        r.ed.open_event_tap(
            EventTapId::of(id),
            input,
            EventTapOptions::new(EventCapacity::of(64)).align(align),
        )
        .expect("the input exists")
    };
    let mut raw = open(&mut r, 1, TapAlign::Raw);
    let mut aligned = open(&mut r, 2, TapAlign::ToOutput);
    commit(&mut r);
    assert_eq!(tap_ops(&r), 2, "an unwired input is tapped");
    assert_eq!(event_delays(&r), 1);
    r.ed.schedule(
        At::Frame(Frame::new(70)),
        input,
        Scheduled::Event(EventKind::Midi(on(5))),
    )
    .expect("room");
    render(&mut r, &[64], 192);
    assert_eq!(heard.lock().unwrap().to_vec(), vec![at(70, on(5))]);
    assert_eq!(drain(&mut raw), vec![at(70, on(5))]);
    assert_eq!(drain(&mut aligned), vec![at(110, on(5))]);
    assert_eq!(aligned.take_lost(), 0);
}

/// **An aligned input tap holds one block of commands, and counts the rest.**
/// A delay of 200 frames at blocks of 64 keeps what the commands of one block
/// bring (`COMMAND_CAPACITY` of them) in flight; a second block's commands
/// meet a full queue, and what it refuses is counted as dropped events, the
/// first block's commands all reaching the reader.
#[test]
fn an_aligned_input_tap_holds_a_blocks_commands_and_counts_the_rest() {
    let mut r = rig(1, false, 200);
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            input,
            EventTapOptions::new(EventCapacity::of(2048)),
        )
        .expect("the input exists");
    commit(&mut r);
    let burst = |r: &mut Rig, frame: u64| {
        for _ in 0..tutti_graph::COMMAND_CAPACITY {
            r.ed.schedule(
                At::Frame(Frame::new(frame)),
                input,
                Scheduled::Event(EventKind::Midi(on(5))),
            )
            .expect("room");
        }
    };
    burst(&mut r, 10);
    render(&mut r, &[64], 64);
    assert_eq!(heard.lock().unwrap().len(), tutti_graph::COMMAND_CAPACITY);
    assert_eq!(
        r.exec.dropped_event_count(),
        0,
        "one block of commands fits"
    );
    burst(&mut r, 70);
    render(&mut r, &[64], 512);
    let carried = drain(&mut tap);
    let lost = r.exec.dropped_event_count();
    assert_eq!(
        carried.len() as u64 + lost,
        2 * tutti_graph::COMMAND_CAPACITY as u64,
        "every command reaches the reader or is counted"
    );
    assert!(lost > 0, "the second block meets a full queue");
    assert!(
        carried
            .iter()
            .take(tutti_graph::COMMAND_CAPACITY)
            .all(|e| e.frame.get() == 210),
        "the first block's commands all arrive, 200 frames later"
    );
    assert_eq!(tap.take_lost(), 0, "the reader's own ring lost none");
}

/// **A bypassed node that has parked has an empty input tap**: it is handed
/// nothing, and the view is not left holding the close it was last sent. A
/// probe bounded at one event holds 120 keys and is sent one close a block, so
/// the block that sends the last one is a settled bypass's: the next block the
/// node is parked in, and its taps carry no 121st close, nor the events its
/// source goes on writing every block (whose slot its view may share).
#[test]
fn a_parked_nodes_input_tap_carries_nothing() {
    for align in [TapAlign::Raw, TapAlign::ToOutput] {
        let mut r = rig(1, false, 40);
        let keys: Vec<(u64, Ump)> = (0..370u32)
            .map(|k| (64 * u64::from(k) + 3, on(k % 120)))
            .collect();
        r.ed.insert(NodeKey(1), "script", Unforkable(Script::new(keys)));
        let heard = tutti_fixtures::Record::new();
        r.ed.insert(
            NodeKey(3),
            "probe",
            Unforkable(Bounded {
                probe: Probe {
                    heard: heard.clone(),
                    ports: 1,
                    capacity: 0,
                    audible: true,
                },
                bound: 1,
            }),
        );
        let input = EventInputPort::new(NodeKey(3), 0);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(1, 0)));
        let mut tap =
            r.ed.open_event_tap(
                EventTapId::of(1),
                input,
                EventTapOptions::new(EventCapacity::of(256)).align(align),
            )
            .expect("the input exists");
        commit(&mut r);
        render(&mut r, &[64], 64 * 120);
        r.ed.set_mode(NodeKey(3), NodeMode::Bypassed)
            .expect("a node");
        commit(&mut r);
        render(&mut r, &[64], 64 * 250);
        let by = if align == TapAlign::Raw { 0 } else { 40 };
        let mut node: Vec<TapEvent> = (0..120u32)
            .map(|k| at(64 * u64::from(k) + 3, on_port(on(k), 0)))
            .collect();
        node.extend((0..120u32).map(|k| at(64 * (120 + u64::from(k)), off(k))));
        assert_eq!(heard.lock().unwrap().to_vec(), node, "{align:?}");
        let tapped: Vec<TapEvent> = node.iter().map(|e| at(e.frame.get() + by, e.ump)).collect();
        assert_eq!(
            drain(&mut tap),
            tapped,
            "the closes once, and nothing after the node parked: {align:?}"
        );
    }
}

/// **A node that is skipped has an empty input tap**, not what the view last
/// held: an idle node is called for the block with an event and not after.
#[derive(Clone)]
struct Idle {
    heard: tutti_fixtures::Record<TapEvent>,
}

impl Node for Idle {
    fn shape(&self) -> Shape {
        Probe::BASE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut heard = self.heard.lock().unwrap();
        for e in io.events(Probe::IN) {
            if let EventKind::Midi(ump) = e.kind {
                heard.push(TapEvent {
                    frame: cx.env.frame_at(e.offset),
                    ump,
                });
            }
        }
        Status::Idle
    }
    fn reset(&mut self) {}
}

#[test]
fn a_skipped_nodes_input_tap_carries_nothing_after_its_events() {
    let mut r = rig(1, false, 0);
    script(&mut r, 1, Script::new(vec![(3, on(1)), (200, on(2))]));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "idle",
        ForkByClone(Idle {
            heard: heard.clone(),
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    r.ed.spec_mut()
        .connect_events(input, EventEdge::Direct(port(1, 0)));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            input,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    render(&mut r, &[64], 512);
    let node = vec![at(3, on_port(on(1), 0)), at(200, on_port(on(2), 0))];
    assert_eq!(heard.lock().unwrap().to_vec(), node);
    assert_eq!(
        drain(&mut tap),
        node,
        "once each, though blocks were skipped"
    );
}

/// **An aligned tap of an input fed only by a feedback edge is delayed** like
/// any other aligned tap: the node hears the feedback edge's events a delay
/// late, and the tap carries them the gap to the output time (40) later.
#[test]
fn an_aligned_tap_of_a_feedback_fed_input_is_delayed() {
    let mut r = rig(1, false, 40);
    script(&mut r, 1, Script::new(vec![(5, on(1)), (200, on(2))]));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    r.ed.spec_mut()
        .connect_events(input, EventEdge::feedback(port(1, 0), Samples::new(512)));
    let open = |r: &mut Rig, id, align| {
        r.ed.open_event_tap(
            EventTapId::of(id),
            input,
            EventTapOptions::new(EventCapacity::of(64)).align(align),
        )
        .expect("the input exists")
    };
    let mut raw = open(&mut r, 1, TapAlign::Raw);
    let mut aligned = open(&mut r, 2, TapAlign::ToOutput);
    commit(&mut r);
    assert_eq!(event_delays(&r), 1, "the aligned tap is delayed");
    render(&mut r, &[64], 1280);
    let node = heard.lock().unwrap().to_vec();
    assert_eq!(node.len(), 2, "both events came round");
    assert_eq!(drain(&mut raw), node);
    let later: Vec<TapEvent> = node.iter().map(|e| at(e.frame.get() + 40, e.ump)).collect();
    assert_eq!(drain(&mut aligned), later);
}

/// **The events a removed delay flushes are in the input tap**, at the first
/// frames of the next block as the node hears them.
#[test]
fn an_input_tap_carries_the_events_a_removed_delay_flushes() {
    let mut r = rig(1, false, 40);
    // Source 1 is the shorter path: its edge is delayed by the 15 frames
    // source 2's latency adds.
    script(&mut r, 1, Script::new(vec![(60, on(1)), (61, off(1))]));
    script(&mut r, 2, Script::new(vec![]).latency(15));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        ForkByClone(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    for from in [1, 2] {
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(from, 0)));
    }
    let mut raw =
        r.ed.open_event_tap(
            EventTapId::of(1),
            input,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    assert!(heard.lock().unwrap().is_empty(), "both still in the delay");
    assert!(r.ed.spec_mut().disconnect_events(input, port(1, 0)));
    commit(&mut r);
    render(&mut r, &[64], 128);
    let flushed = vec![at(64, on_port(on(1), 0)), at(65, on_port(off(1), 0))];
    assert_eq!(heard.lock().unwrap().to_vec(), flushed);
    assert_eq!(drain(&mut raw), flushed);
}

/// A node with 64 event inputs and 64 event outputs on the pool: its op holds a
/// source, a view and an output of every port at once.
#[derive(Clone)]
struct Wide;

impl Node for Wide {
    fn shape(&self) -> Shape {
        Shape::EMPTY.events(64, 64)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

#[test]
fn a_full_width_event_node_holds_its_claims_on_the_pool() {
    let mut r = rig(3, true, 40);
    script(&mut r, 1, Script::new(vec![(3, on(1))]).ports(64));
    script(&mut r, 2, Script::new(vec![(4, on(2))]));
    r.ed.insert(NodeKey(3), "wide", Unforkable(Wide));
    for p in 0..64 {
        r.ed.spec_mut().connect_events(
            EventInputPort::new(NodeKey(3), p),
            EventEdge::Direct(port(1, p)),
        );
    }
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            EventInputPort::new(NodeKey(3), 63),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    assert!(r.exec.is_parallel());
    render(&mut r, &[64], 128);
    assert_eq!(drain(&mut tap), vec![at(3, on_port(on(1), 63))]);
}

/// **A bypassed node is sent every close it owes in one block, up to what a
/// block's events hold.** A source holding 1000 keys over sixteen channels is
/// bypassed: the first block after sends the 1000 note-offs, all at the
/// block's first frame, and the input tap carries them.
#[test]
fn a_bypassed_node_is_sent_a_blocks_worth_of_closes() {
    let mut r = rig(1, false, 40);
    let keys: Vec<(u64, Ump)> = (0..1000u32)
        .map(|i| {
            let channel = MidiChannel::new((i / 64) as u8).expect("a channel");
            (
                3,
                midi::m1_note_on(channel, (i % 64) as u8 + 20, U7::of(100)),
            )
        })
        .collect();
    let mut s = Script::new(keys);
    s.cap = 1024;
    r.ed.insert(NodeKey(1), "script", Unforkable(s));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        Unforkable(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: true,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    r.ed.spec_mut()
        .connect_events(input, EventEdge::Direct(port(1, 0)));
    let mut tap =
        r.ed.open_event_tap(
            EventTapId::of(1),
            input,
            EventTapOptions::new(EventCapacity::of(2048)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    heard.lock().unwrap().clear();
    r.ed.set_mode(NodeKey(3), NodeMode::Bypassed)
        .expect("a node");
    commit(&mut r);
    render(&mut r, &[64], 128);
    let closes = heard.lock().unwrap().to_vec();
    assert_eq!(closes.len(), 1000, "one block sends every close");
    assert!(
        closes
            .iter()
            .all(|e| e.frame.get() == 64 && e.ump.is_note_off())
    );
    let tapped = drain(&mut tap);
    assert_eq!(tapped.len(), 2000, "1000 note-ons, then 1000 note-offs");
    assert_eq!(tapped[1000..], closes[..]);
}

/// **A lone source is handed over whole and in order, and a command beside it
/// is merged in.** A source writes 32 note-ons a block, one on each even
/// frame, into a probe (nothing else reaches the input, so the view is a copy
/// of the source); in the second block a command lands on a frame between two
/// of them, and the merged block is what the node heard and the taps carry,
/// followed by a block that is a copy again.
#[test]
fn a_lone_source_is_copied_whole_and_a_command_is_merged_beside_it() {
    let mut r = rig(1, false, 40);
    let events: Vec<(u64, Ump)> = (0..96u32).map(|i| (u64::from(i) * 2, on(i + 10))).collect();
    let mut s = Script::new(events);
    s.cap = 32;
    r.ed.insert(NodeKey(1), "script", Unforkable(s));
    let heard = tutti_fixtures::Record::new();
    r.ed.insert(
        NodeKey(3),
        "probe",
        Unforkable(Probe {
            heard: heard.clone(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    let input = EventInputPort::new(NodeKey(3), 0);
    r.ed.spec_mut()
        .connect_events(input, EventEdge::Direct(port(1, 0)));
    let mut raw =
        r.ed.open_event_tap(
            EventTapId::of(1),
            input,
            EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    commit(&mut r);
    render(&mut r, &[64], 64);
    r.ed.schedule(
        At::Frame(Frame::new(65)),
        input,
        Scheduled::Event(EventKind::Midi(on(127))),
    )
    .expect("room");
    render(&mut r, &[64], 128);
    // 32 events a block (frames 0..64 step 2), the command at 65 between the
    // events at 64 and 66; the source's keys are 10 + the event's index.
    let mut want: Vec<TapEvent> = (0..96u32)
        .map(|i| at(u64::from(i) * 2, on_port(on(i + 10), 0)))
        .collect();
    want.insert(33, at(65, on(127)));
    assert_eq!(heard.lock().unwrap().to_vec(), want, "what the node heard");
    assert_eq!(drain(&mut raw), want, "what the tap carried");
    assert_eq!(r.exec.dropped_event_count(), 0);
}

/// A node with two event inputs and a declared input bound, which records
/// what it is handed at each input.
#[derive(Clone)]
struct TwoIn {
    heard: [tutti_fixtures::Record<TapEvent>; 2],
    bound: u32,
}

impl TwoIn {
    const SHAPE: Shape = Shape::EMPTY.events(2, 0);
    const IN: [tutti_graph::InEvents; 2] = [
        Self::SHAPE.event_input(0).expect("declared"),
        Self::SHAPE.event_input(1).expect("declared"),
    ];
}

impl Node for TwoIn {
    fn shape(&self) -> Shape {
        Self::SHAPE.event_input_capacity(std::num::NonZeroU32::new(self.bound).expect("a bound"))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        for (p, heard) in self.heard.iter().enumerate() {
            let mut heard = heard.lock().unwrap();
            for e in io.events(Self::IN[p]) {
                if let EventKind::Midi(ump) = e.kind {
                    heard.push(TapEvent {
                        frame: cx.env.frame_at(e.offset),
                        ump,
                    });
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// What one block brings a node with two inputs and a bound: per port a source
/// of 64 events (one a frame, a note-off on the last frame) and 128 scheduled
/// commands, two a frame; 384 in all against a bound of 360 that is larger
/// than a view holds of one port (a source priced 64 and 256 of command room).
/// Returns what the node was handed and what each input's tap carried, per
/// port, and the executor's dropped count.
fn two_bounded(workers: usize) -> (Vec<Vec<TapEvent>>, Vec<Vec<TapEvent>>, u64) {
    const BOUND: u32 = 360;
    let mut r = rig(workers, workers > 1, 40);
    for p in 0..2u64 {
        let base = 64 * p as u32;
        let mut events: Vec<(u64, Ump)> =
            (0..63u32).map(|f| (u64::from(f), on(base + f))).collect();
        events.push((63, off(base)));
        let mut s = Script::new(events);
        s.cap = 64;
        r.ed.insert(NodeKey(10 + p), "script", Unforkable(s));
    }
    // An unrelated pair, so the graph has tasks to spread.
    script(&mut r, 30, Script::new(vec![(1, on(1))]));
    r.ed.insert(
        NodeKey(31),
        "probe",
        ForkByClone(Probe {
            heard: tutti_fixtures::Record::new(),
            ports: 1,
            capacity: 0,
            audible: false,
        }),
    );
    r.ed.spec_mut().connect_events(
        EventInputPort::new(NodeKey(31), 0),
        EventEdge::Direct(port(30, 0)),
    );
    let heard = [tutti_fixtures::Record::new(), tutti_fixtures::Record::new()];
    r.ed.insert(
        NodeKey(20),
        "two",
        Unforkable(TwoIn {
            heard: heard.clone(),
            bound: BOUND,
        }),
    );
    let mut taps = Vec::new();
    for p in 0..2u16 {
        let input = EventInputPort::new(NodeKey(20), p);
        r.ed.spec_mut()
            .connect_events(input, EventEdge::Direct(port(10 + u64::from(p), 0)));
        taps.push(
            r.ed.open_event_tap(
                EventTapId::of(u64::from(p) + 1),
                input,
                EventTapOptions::new(EventCapacity::of(512)).align(TapAlign::Raw),
            )
            .expect("the input exists"),
        );
    }
    commit(&mut r);
    for p in 0..2u16 {
        for i in 0..128u32 {
            r.ed.schedule(
                At::Frame(Frame::new(u64::from(i % 64))),
                EventInputPort::new(NodeKey(20), p),
                Scheduled::Event(EventKind::Midi(on(10 + (i % 50)))),
            )
            .expect("room");
        }
    }
    assert_eq!(r.exec.is_parallel(), workers > 1);
    render(&mut r, &[64], 64);

    // The merge the node is owed, from the rule alone: the sources of a port
    // are its edge (index 1) and its commands (index 2) below the other
    // port's; ordered by frame, ties to the lower source; of `BOUND` events,
    // every note-off lands and the others take what is left.
    let mut all: Vec<(u64, usize, Ump)> = Vec::new();
    for p in 0..2usize {
        let base = 64 * p as u32;
        for f in 0..63u32 {
            all.push((u64::from(f), 3 * p + 1, on_port(on(base + f), 0)));
        }
        all.push((63, 3 * p + 1, on_port(off(base), 0)));
        for i in 0..128u32 {
            all.push((u64::from(i % 64), 3 * p + 2, on(10 + (i % 50))));
        }
    }
    all.sort_by_key(|&(f, src, _)| (f, src));
    assert_eq!(all.len(), 384);
    let offs = all.iter().filter(|e| e.2.is_note_off()).count();
    let mut others = BOUND as usize - offs;
    let mut owed: Vec<Vec<TapEvent>> = vec![Vec::new(); 2];
    let mut kept = 0;
    for &(f, src, ump) in &all {
        let lands = kept < BOUND as usize
            && (ump.is_note_off() || {
                let fits = others > 0;
                others = others.saturating_sub(1);
                fits
            });
        if lands {
            kept += 1;
            owed[src / 3].push(at(f, ump));
        }
    }
    assert_eq!(kept, BOUND as usize);
    let handed: Vec<Vec<TapEvent>> = heard.iter().map(|h| h.lock().unwrap().to_vec()).collect();
    assert_eq!(
        handed.iter().map(Vec::len).sum::<usize>(),
        BOUND as usize,
        "events handed in all, {workers} workers"
    );
    assert_eq!(handed, owed, "what the node was handed, {workers} workers");
    assert_eq!(
        r.exec.dropped_event_count(),
        384 - u64::from(BOUND),
        "the cut is counted once"
    );
    let carried: Vec<Vec<TapEvent>> = taps.iter_mut().map(drain).collect();
    (handed, carried, r.exec.dropped_event_count())
}

/// **A node's input bound is over all its inputs together**, whatever one
/// view holds: a block that brings 384 events to two inputs bounded at 360
/// hands the node 360 in all, the first in frame order with the note-offs
/// kept, and counts 24; the taps carry what it was handed, on the pool too.
#[test]
fn an_input_bound_cuts_over_all_inputs_together() {
    let (handed, carried, _) = two_bounded(1);
    assert_eq!(handed, carried, "what each input's tap carried");
    let total: usize = handed.iter().map(Vec::len).sum();
    assert_eq!(total, 360);
    for workers in [2, 3] {
        let pooled = two_bounded(workers);
        assert_eq!(pooled.0, handed, "{workers} workers");
        assert_eq!(pooled.1, carried, "{workers} workers");
    }
}
