//! `NoteLedger` through real graphs: a refused note-off is paid, no note is
//! left sounding whatever the port refused, a full table counts what it
//! cannot keep, and the clip node's reset and fork leave its notes ended.
//!
//! Every figure is counted by hand from the scenario or is an invariant of
//! the run (nothing sounds after a stop, every note-on has its note-off); no
//! test compares against another render. The clip plays at 120 BPM and
//! 48 kHz, so a beat is 24,000 frames.

use std::collections::BTreeSet;
use std::sync::Arc;
use tutti_buffers::MaxBlock;
use tutti_rt::Mutex;

use self::common::{FRAMES_PER_BEAT, at, beat_of, connect, off, on, rolling};

use crate::ledger::NoteLedger;
use crate::{ClipEvents, MidiClipControls, MidiClipNode, TimedMidiEvent};
use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::contract::Direct;
use tutti_graph::{
    Cx, Editor, Event, EventInputPort, EventKind, EventOutputPort, Executor, ForkCause, ForkSource,
    ForkTarget, Forked, InEvents, IntoNode, Io, Node, NodeParts, Offset, OutEvents, Prepare,
    RenderMode, Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_music::LoopRange;
use tutti_rt::SplitMix64;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, NoteId, NoteNumber, Ump};
use tutti_units::{Beat, Bpm, Latency, SampleRate, Samples};

/// The clip clock at 120 BPM and 48 kHz and the helpers that build on it.
mod common {
    use tutti_fixtures::midi;
    use tutti_graph::{
        Editor, EventEdge, EventInputPort, EventOutputPort, NodeKey, TransportPosition,
    };
    use tutti_ump::{MidiChannel, Ump};
    use tutti_units::{Beat, Bpm};

    use crate::TimedMidiEvent;

    /// Frames in a beat at 120 BPM and 48 kHz, so a frame is exact in beats.
    pub(super) const FRAMES_PER_BEAT: f64 = 24_000.0;

    pub(super) fn beat_of(frame: u64) -> Beat {
        Beat::new(frame as f64 / FRAMES_PER_BEAT)
    }

    /// Rolling at 120 BPM from `frame`.
    pub(super) fn rolling(frame: u64) -> TransportPosition {
        TransportPosition::new(true, Bpm::of(120.0), beat_of(frame), None)
    }

    /// `event` on `frame`.
    pub(super) fn at(frame: u64, event: Ump) -> TimedMidiEvent {
        TimedMidiEvent::new(beat_of(frame), event).unwrap()
    }

    /// A full-velocity Note On of key `n` on the first Group and Channel.
    pub(super) fn on(n: u8) -> Ump {
        midi::note_on(MidiChannel::FIRST, n, 0xFFFF)
    }

    /// A Note Off of key `n` on the first Group and Channel.
    pub(super) fn off(n: u8) -> Ump {
        midi::note_off(MidiChannel::FIRST, n, 0)
    }

    /// Wires event output 0 of node `from` to event input 0 of node `to`.
    pub(super) fn connect(ed: &mut Editor, from: u64, to: u64) {
        ed.spec_mut().connect_events(
            EventInputPort {
                node: NodeKey(to),
                port: 0,
            },
            EventEdge::Direct(EventOutputPort {
                node: NodeKey(from),
                port: 0,
            }),
        );
    }
}

fn stopped(frame: u64) -> TransportPosition {
    TransportPosition::new(false, Bpm::of(120.0), beat_of(frame), None)
}

/// What a sink heard: the keys it holds, offs for keys it does not hold
/// (orphans), and the frame each off arrived on.
#[derive(Default)]
struct Tally {
    held: BTreeSet<u32>,
    orphans: usize,
    ons: usize,
    offs: usize,
    /// Events that are not note messages.
    others: usize,
    off_frames: Vec<u64>,
}

type Shared = Arc<Mutex<Tally>>;

struct Sink {
    tally: Shared,
    audio_in: bool,
    audio_out: bool,
}

impl Sink {
    fn new(tally: &Shared) -> Self {
        Self {
            tally: Arc::clone(tally),
            audio_in: false,
            audio_out: false,
        }
    }
}

impl Sink {
    const fn declared(audio_in: bool, audio_out: bool) -> Shape {
        const fn layout(on: bool) -> ChannelLayout {
            if on {
                ChannelLayout::MONO
            } else {
                ChannelLayout::EMPTY
            }
        }
        Shape::buses(
            tutti_graph::Buses::unassigned(layout(audio_in)),
            tutti_graph::Buses::unassigned(layout(audio_out)),
        )
        .with_events(1, 0)
    }
    const IN: InEvents = Self::declared(false, false)
        .event_input(0)
        .expect("declared");
}

impl<F: Float> Node<F> for Sink {
    fn shape(&self) -> Shape {
        Self::declared(self.audio_in, self.audio_out)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let mut t = self
            .tally
            .try_lock()
            .expect("a test reads the tally only between blocks, so it is never held here");
        for e in io.events(Self::IN) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                continue;
            };
            let key = w[0] & 0x0f0f_7f00;
            let frame = cx.env.frame.get() + u64::from(e.offset.get());
            if e.is_note_off() {
                t.off_frames.push(frame);
                t.offs += 1;
                if !t.held.remove(&key) {
                    t.orphans += 1;
                }
            } else if (w[0] >> 20) & 0xf == 0x9 {
                t.ons += 1;
                t.held.insert(key);
            } else {
                t.others += 1;
            }
        }
        if self.audio_out {
            io.output(0).fill(F::ZERO);
            return Status::Modified;
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A sink a fork can copy: the fork hears into a tally of its own.
struct ForkableSink {
    sink: Sink,
    forks: Arc<Mutex<Vec<Shared>>>,
}

struct SinkFork(Arc<Mutex<Vec<Shared>>>);

impl ForkSource for SinkFork {
    fn fork(&self, _: RenderMode) -> Result<Forked, ForkCause> {
        let tally = Shared::default();
        self.0.lock().push(Arc::clone(&tally));
        Ok(Forked::new(Box::new(Sink {
            tally,
            audio_in: false,
            audio_out: true,
        })))
    }
}

impl IntoNode for ForkableSink {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self.sink),
            controls: (),
            fork: Some(Box::new(SinkFork(self.forks))),
        }
    }
}

/// A silent audio source with a declared latency: the compiler delays every
/// other path into a node it shares with a sink.
struct Latent(usize);

impl<F: Float> Node<F> for Latent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_latency(Latency::new(Samples::new(self.0)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        io.output(0).fill(F::ZERO);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn disconnect(ed: &mut Editor, from: u64, to: u64) {
    assert!(ed.spec_mut().disconnect_events(
        EventInputPort {
            node: NodeKey(to),
            port: 0
        },
        EventOutputPort {
            node: NodeKey(from),
            port: 0
        }
    ));
}

fn sink(ed: &mut Editor, key: u64) -> Shared {
    let tally = Shared::default();
    ed.insert(NodeKey(key), "sink", Unforkable(Sink::new(&tally)));
    tally
}

/// `blocks` blocks of `block` frames from frame `from`, rolling; returns the
/// next frame.
fn run(exec: &mut Executor, from: u64, blocks: u64, block: usize) -> u64 {
    (0..blocks).fold(from, |f, _| {
        exec.process(block, &rolling(f), &[], &mut []);
        f + block as u64
    })
}

/// `run` for a graph with one audio output, which nobody reads.
fn run_out(exec: &mut Executor, from: u64, blocks: u64) -> u64 {
    (0..blocks).fold(from, |f, _| {
        let mut out = [0.0f32; 256];
        exec.process(256, &rolling(f), &[], &mut [&mut out[..]]);
        f + 256
    })
}

/// A value below `n` from a `SplitMix64` stream.
trait Below {
    fn below(&mut self, n: u64) -> u64;
}

impl Below for SplitMix64 {
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

// ---------------------------------------------------------------------------
// A relay through the ledger.
// ---------------------------------------------------------------------------

/// Emits `blocks[k]` on block `k` (all on the block's first frame), from a
/// port big enough for all of it.
struct Script {
    blocks: Vec<Vec<Ump>>,
    k: usize,
}

impl Script {
    const SHAPE: Shape = Shape::EMPTY
        .with_events(0, 1)
        .with_event_capacity(std::num::NonZeroU32::new(2048).expect("a capacity"));
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Script {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if let Some(block) = self.blocks.get(self.k) {
            let out = io.event_out(Self::OUT);
            for u in block {
                let _ = out.push(Event::midi(Offset::ZERO, *u));
            }
        }
        self.k += 1;
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Passes every event on through a ledger, into a port of `capacity` events.
struct Relay {
    ledger: Option<NoteLedger<()>>,
    capacity: u32,
}

impl Relay {
    const fn declared(capacity: u32) -> Shape {
        Shape::EMPTY
            .with_events(1, 1)
            .with_event_capacity(std::num::NonZeroU32::new(capacity).expect("a capacity"))
    }
    const IN: InEvents = Self::declared(1).event_input(0).expect("declared");
    const OUT: OutEvents = Self::declared(1).event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Relay {
    fn shape(&self) -> Shape {
        Self::declared(self.capacity)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let events = io.events(Self::IN);
        let Some(first) = cx.env.offset(0) else {
            return Status::Modified;
        };
        let out = io.event_out(Self::OUT);
        match &mut self.ledger {
            Some(ledger) => {
                let mut sending = ledger.block(out, first);
                for e in events {
                    if let EventKind::Midi(u) = e.kind {
                        sending.send(e.offset, u, None);
                    }
                }
            }
            None => {
                for e in events {
                    let _ = out.push(*e);
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        if let Some(ledger) = &mut self.ledger {
            ledger.forget();
        }
    }
}

/// The channel and note of the key a test numbers `n`: `channel * 128 +
/// note`, `tutti_ump::NoteId`'s own (a number past channel 15 wraps).
fn key(n: u32) -> (MidiChannel, NoteNumber) {
    NoteId::from_raw(n % (16 * 128))
        .channel_note()
        .expect("inside 16 channels")
}

fn ump_on(n: u32) -> Ump {
    let (c, n) = key(n);
    midi::note_on(c, n.get(), 0xFFFF)
}

fn ump_off(n: u32) -> Ump {
    let (c, n) = key(n);
    midi::note_off(c, n.get(), 0)
}

fn ump_cc(v: u32) -> Ump {
    midi::cc(MidiChannel::FIRST, CCNumber::VOLUME, v)
}

/// `script` through `relay` into a sink, in 64-frame blocks; `blocks` of
/// them.
fn through(script: Vec<Vec<Ump>>, relay: Relay, blocks: u64) -> (Shared, Executor) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(
        NodeKey(1),
        "script",
        Unforkable(Script {
            blocks: script,
            k: 0,
        }),
    );
    ed.insert(NodeKey(2), "relay", Unforkable(relay));
    let tally = sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    connect(&mut ed, 2, 3);
    ed.commit().unwrap();
    run(&mut exec, 0, blocks, 64);
    (tally, exec)
}

/// 256 notes start in block 0. Block 1 sends 48 controllers and the 256
/// offs into a port of 256 events with room for 296 note-offs
/// (`note_off_room`: 256 + 32 + 8): 48 + 248 fit, 8 offs are refused. They
/// are owed, and all 8 go out on the first frame of block 2, which is frame
/// 128 of 64-frame blocks.
#[test]
fn a_refused_off_is_paid_at_the_first_frame_of_the_next_block() {
    let script = vec![
        (0..256).map(ump_on).collect(),
        (0..48).map(ump_cc).chain((0..256).map(ump_off)).collect(),
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(2048)),
        capacity: 256,
    };
    let (tally, exec) = through(script, relay, 6);
    let t = tally.lock();
    assert_eq!((t.ons, t.offs), (256, 256));
    assert!(t.held.is_empty());
    let late: Vec<u64> = t.off_frames.iter().copied().filter(|&f| f == 128).collect();
    assert_eq!(late.len(), 8, "{:?}", t.off_frames);
    // Refused once each and delivered later: the loss nets zero.
    assert_eq!(exec.dropped_event_count(), 8);
    assert_eq!(exec.lost_event_count(), 0);
}

/// A table of 4 keys against 6 notes: the last 2 note-ons are skipped and
/// counted (2 in the dropped count, never silent), the 4 that started end,
/// and the table is free again afterwards, so a later note starts. A relay's
/// off carries no owner, so the 2 offs of the notes that never started pass
/// through as orphans.
#[test]
fn a_small_table_withholds_the_on_it_cannot_keep() {
    let script = vec![
        (0..6).map(ump_on).collect(),
        (0..6).map(ump_off).collect(),
        vec![ump_on(7)],
        vec![ump_off(7)],
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(4)),
        capacity: 256,
    };
    let (tally, exec) = through(script, relay, 6);
    let t = tally.lock();
    assert_eq!((t.ons, t.offs), (5, 7), "4 + the later note, 6 + 1 offs");
    assert_eq!(t.orphans, 2);
    assert!(t.held.is_empty());
    assert_eq!(exec.dropped_event_count(), 2);
}

/// 20 random blocks of toggling 12 keys (a held key's next event is its off)
/// among controller filler that saturates an 8-event port, then every held
/// key is ended and the port drains for 12 blocks. Returns the keys the sink
/// still holds.
fn random_relay(seed: u64, ledger: bool) -> usize {
    let mut r = SplitMix64::new(seed);
    let mut sender_held = [false; 12];
    let mut blocks: Vec<Vec<Ump>> = (0..20)
        .map(|_| {
            (0..r.below(30))
                .map(|_| {
                    if r.below(3) == 0 {
                        ump_cc(r.below(100) as u32)
                    } else {
                        let k = r.below(12) as usize;
                        sender_held[k] = !sender_held[k];
                        let key = k as u32;
                        if sender_held[k] {
                            ump_on(key)
                        } else {
                            ump_off(key)
                        }
                    }
                })
                .collect()
        })
        .collect();
    let last = (0..40)
        .map(ump_cc)
        .chain((0..12u32).filter(|&k| sender_held[k as usize]).map(ump_off))
        .collect();
    blocks.push(last);
    let relay = Relay {
        ledger: ledger.then(|| NoteLedger::new(64)),
        capacity: 8,
    };
    let (tally, _) = through(blocks, relay, 33);
    tally.lock().held.len()
}

/// Whatever the port refuses, a relay through the ledger leaves no note
/// sounding. The plain relay is the control: it strands notes in this very
/// scenario, so the scenario does refuse.
#[test]
fn a_ledger_relay_strands_no_note_under_random_refusal() {
    const SEEDS: u64 = 3_000;
    for seed in 1..=SEEDS {
        assert_eq!(random_relay(seed, true), 0, "seed {seed}");
    }
    let stranded = (1..=SEEDS).filter(|&s| random_relay(s, false) > 0).count();
    assert!(stranded > 0, "the scenario never refused an off");
}

// ---------------------------------------------------------------------------
// The clip node.
// ---------------------------------------------------------------------------

/// A clip into a sink in 64-frame blocks (a port of 256 events).
fn clip_rig(events: Vec<TimedMidiEvent>) -> (Editor, Executor, Shared) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    let tally = sink(&mut ed, 2);
    connect(&mut ed, 1, 2);
    ed.commit().unwrap();
    (ed, exec, tally)
}

/// Random notes on 384 keys, most starting on a few shared frames so that a
/// block holds more than the port takes, played through random seeks and
/// stops, then stopped for 20 blocks. Returns the sink's tally and the
/// events the port refused.
fn random_clip(seed: u64) -> (Shared, u64) {
    let mut r = SplitMix64::new(seed);
    let starts = [0u64, 10, 70, 500];
    let events = (0..100 + r.below(500))
        .flat_map(|_| {
            let g = MidiGroup::of(r.below(4) as u8);
            let c = MidiChannel::of(r.below(4) as u8);
            let n = r.below(24) as u8;
            let start = if r.below(4) == 0 {
                r.below(1_500)
            } else {
                starts[r.below(4) as usize]
            };
            let end = start + 1 + r.below(1_200);
            [
                at(
                    start,
                    midi::note_on(tutti_ump::Address::new(g, c), n, 0xFFFF),
                ),
                at(end, midi::note_off(tutti_ump::Address::new(g, c), n, 0)),
            ]
        })
        .collect();
    let (_ed, mut exec, tally) = clip_rig(events);
    let mut frame = 0;
    for _ in 0..r.below(6) + 2 {
        frame = run(&mut exec, frame, 1 + r.below(8), 64);
        match r.below(3) {
            0 => frame = r.below(1_900),
            1 => (0..1 + r.below(3)).for_each(|_| exec.process(64, &stopped(frame), &[], &mut [])),
            _ => {}
        }
    }
    (0..20).for_each(|_| exec.process(64, &stopped(frame), &[], &mut []));
    (tally, exec.dropped_event_count())
}

/// Whatever the port refused, nothing sounds after a stop, every note-on the
/// sink heard has its note-off, and the sink never hears an off for a note
/// it did not start.
#[test]
fn a_clip_leaves_nothing_sounding_after_a_stop() {
    let mut refused = 0;
    for seed in 1..=1_000 {
        let (tally, dropped) = random_clip(seed);
        let t = tally.lock();
        assert!(t.held.is_empty(), "seed {seed}: {:?}", t.held);
        assert_eq!(t.ons, t.offs, "seed {seed}");
        assert_eq!(t.orphans, 0, "seed {seed}");
        refused += dropped;
    }
    assert!(refused > 0, "the scenario never refused an event");
}

/// 2,100 notes sound at once across two groups, one starting on each frame
/// from frame 0, against a table of 2,048 keys. The first 2,048 start. The
/// last 52 are skipped, each counted once in the dropped count and charged
/// to the clip's port; their offs are swallowed, not counted again. The 2,048
/// that started all end (frame 100,000 on), nothing is orphaned, and once
/// the table has room a later note starts.
#[test]
fn a_full_table_withholds_what_it_cannot_keep() {
    let key = |i: u32| {
        let (g, rest) = (i / 2_048, i % 2_048);
        (
            MidiGroup::of(g as u8),
            MidiChannel::of((rest / 128) as u8),
            (rest % 128) as u8,
        )
    };
    let note = |i: u32, start: u64, end: u64| {
        let (g, c, n) = key(i);
        [
            at(
                start,
                midi::note_on(tutti_ump::Address::new(g, c), n, 0xFFFF),
            ),
            at(end, midi::note_off(tutti_ump::Address::new(g, c), n, 0)),
        ]
    };
    let events = (0..2_100u32)
        .flat_map(|i| note(i, u64::from(i), 100_000 + u64::from(i)))
        .chain(note(2_100, 104_000, 104_100))
        .collect();
    let (mut ed, mut exec, tally) = clip_rig(events);
    run(&mut exec, 0, 1_700, 64); // past frame 108,800
    assert_eq!(exec.dropped_event_count(), 52);
    let losses: Vec<_> = ed.take_event_losses().collect();
    assert_eq!(losses.iter().map(|l| l.count).sum::<u32>(), 52);
    assert!(
        losses
            .iter()
            .all(|l| l.node == NodeKey(1) && l.port == Some(0))
    );
    let t = tally.lock();
    assert_eq!((t.ons, t.offs), (2_049, 2_049));
    assert_eq!(t.orphans, 0);
    assert!(t.held.is_empty());
}

// ---------------------------------------------------------------------------
// A clip re-aimed while it holds a note.
// ---------------------------------------------------------------------------

/// A clip holds note 60 from frame 0 to frame 100,000 and feeds sink A.
fn holding_clip() -> (Editor, Executor, crate::MidiClipControls, Shared, Shared) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(256)),
    ));
    let controls = ed
        .insert(
            NodeKey(1),
            "clip",
            MidiClipNode::new([at(0, on(60)), at(100_000, off(60))]),
        )
        .controls;
    let a = sink(&mut ed, 2);
    let b = sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    ed.commit().unwrap();
    (ed, exec, controls, a, b)
}

/// Re-aims the clip from A to B and runs past frame 100,000.
fn rewire(ed: &mut Editor, exec: &mut Executor, from: u64) {
    disconnect(ed, 1, 2);
    connect(ed, 1, 3);
    ed.commit().unwrap();
    run(exec, from, 400, 256);
}

/// Re-aiming a clip that holds a note ends it on A, which the clip can no
/// longer reach; B hears the clip's own off at frame 100,000 for a note it
/// never started.
#[test]
fn a_rewire_ends_the_held_note_at_the_old_sink() {
    let (mut ed, mut exec, _controls, a, b) = holding_clip();
    let f = run(&mut exec, 0, 2, 256);
    rewire(&mut ed, &mut exec, f);
    assert!(a.lock().held.is_empty());
    assert_eq!(b.lock().orphans, 1);
}

/// Sink A also takes audio from a node of 1,024 frames of latency, so the
/// compiler delays the event edge into A by 1,024 frames (4 blocks of 256).
/// The note reached A through the delay; cutting the edge sends the close at
/// the cut, frame 2,304 here.
#[test]
fn the_close_reaches_a_sink_behind_a_delayed_edge() {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(256)),
    ));
    ed.insert(
        NodeKey(1),
        "clip",
        MidiClipNode::new([at(0, on(60)), at(100_000, off(60))]),
    );
    ed.insert(NodeKey(4), "latent", Unforkable(Latent(1_024)));
    let a = Shared::default();
    ed.insert(
        NodeKey(2),
        "sink",
        Unforkable(Sink {
            audio_in: true,
            ..Sink::new(&a)
        }),
    );
    sink(&mut ed, 3);
    connect(&mut ed, 1, 2);
    ed.spec_mut().topology.edges.insert(
        InputPort {
            node: NodeKey(2),
            port: 0,
        },
        vec![Edge::Direct(PortSource::Node(OutputPort {
            node: NodeKey(4),
            port: 0,
        }))],
    );
    ed.commit().unwrap();
    let f = run(&mut exec, 0, 8, 256);
    assert_eq!(a.lock().held.len(), 1, "the note reached A");
    let f = run(&mut exec, f, 1, 256);
    assert_eq!(f, 2_304);
    rewire(&mut ed, &mut exec, f);
    let a = a.lock();
    assert!(a.held.is_empty());
    assert_eq!(a.off_frames, vec![2_304]);
}

// ---------------------------------------------------------------------------
// Fork and reset.
// ---------------------------------------------------------------------------

/// A clip with a note from frame 10 to frame 300, into a sink with audio
/// output so the sink can be a fork target.
fn forkable_rig() -> (
    Editor,
    Executor,
    crate::MidiClipControls,
    Shared,
    Arc<Mutex<Vec<Shared>>>,
) {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(256)),
    ));
    let controls = ed
        .insert(
            NodeKey(1),
            "clip",
            MidiClipNode::new([at(10, on(60)), at(300, off(60))]),
        )
        .controls;
    let tally = Shared::default();
    let forks = Arc::default();
    ed.insert(
        NodeKey(2),
        "sink",
        ForkableSink {
            sink: Sink {
                audio_out: true,
                ..Sink::new(&tally)
            },
            forks: Arc::clone(&forks),
        },
    );
    connect(&mut ed, 1, 2);
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().unwrap();
    (ed, exec, controls, tally, forks)
}

fn fork_of(ed: &Editor) -> (Editor, Executor) {
    let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(256)));
    ed.fork(
        ForkTarget::Node(NodeKey(2)),
        prepare.with_render_mode(RenderMode::Offline),
    )
    .expect("the clip and the sink fork")
}

/// The live clip holds note 60 when the graph is forked; the fork renders
/// from frame 256, inside the note. The fork starts with an empty table, so
/// the clip's off at frame 300 is swallowed there (its note never started in
/// the fork), while the live clip's off still arrives.
#[test]
fn a_fork_starts_with_an_empty_table() {
    let (ed, mut exec, _controls, live, forks) = forkable_rig();
    let f = run_out(&mut exec, 0, 1);
    assert_eq!(live.lock().held.len(), 1);
    let (_fed, mut fexec) = fork_of(&ed);
    run_out(&mut fexec, f, 1);
    let fork = forks.lock()[0].clone();
    {
        let fork = fork.lock();
        assert_eq!((fork.ons, fork.offs, fork.orphans), (0, 0, 0));
    }
    run_out(&mut exec, f, 1);
    let live = live.lock();
    assert_eq!((live.ons, live.offs), (1, 1));
    assert!(live.held.is_empty());
}

/// A reset node has forgotten what it held: stopping the transport ends
/// nothing. The twin without the reset ends note 60 on the stopping block,
/// so the empty stop is the reset's doing.
#[test]
fn a_reset_node_forgets_the_notes_it_held() {
    let stop_after = |reset: bool| {
        let clip = MidiClipNode::new([at(10, on(60)), at(5_000, off(60))]);
        let mut d = Direct::new(clip, SampleRate::SR_48K, 256);
        let out = tutti_graph::Node::<f32>::shape(&*d.node)
            .event_output(0)
            .expect("an event output");
        d.set_transport(rolling(0));
        d.block();
        assert_eq!(d.event_output(out).len(), 1, "the note started");
        if reset {
            tutti_graph::Node::<f32>::reset(&mut *d.node);
        }
        d.set_transport(stopped(256));
        d.block();
        d.event_output(out).len()
    };
    assert_eq!(stop_after(false), 1, "the stop ends the held note");
    assert_eq!(stop_after(true), 0);
}

// ---------------------------------------------------------------------------
// Counting a refusal exactly.
// ---------------------------------------------------------------------------

enum Act {
    Send(Ump, Option<u32>),
    Release,
}

/// Plays `blocks[k]` through a ledger on block `k`, into a port of 8 events
/// (room for 17 note-offs), every action on the block's first frame.
struct Probe {
    ledger: NoteLedger<u32>,
    blocks: Vec<Vec<Act>>,
    k: usize,
}

impl Probe {
    const SHAPE: Shape = Shape::EMPTY
        .with_events(0, 1)
        .with_event_capacity(std::num::NonZeroU32::new(8).expect("a capacity"));
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Probe {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let Some(first) = cx.env.offset(0) else {
            return Status::Modified;
        };
        let mut sending = self.ledger.block(io.event_out(Self::OUT), first);
        for act in self.blocks.get(self.k).into_iter().flatten() {
            match act {
                Act::Send(ump, owner) => sending.send(first, *ump, *owner),
                Act::Release => sending.release_all(first),
            }
        }
        self.k += 1;
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Fills the probe's port for the rest of the block with 8 controllers and 9
/// offs of keys the ledger does not know (which pass), the port's 17, then
/// sends `then`.
fn filled(then: Vec<Act>) -> Vec<Act> {
    (0..8)
        .map(|v| Act::Send(ump_cc(v), None))
        .chain((100..109).map(|n| Act::Send(ump_off(n), None)))
        .chain(then)
        .collect()
}

/// What a probe's sink heard, and the executor's counts.
fn probe(capacity: usize, blocks: Vec<Vec<Act>>) -> (Shared, u64, u64) {
    let n = blocks.len() as u64 + 1;
    probe_for(capacity, blocks, n)
}

/// [`probe`] run for `n` blocks, so a block the script does not name need not
/// pay what is owed.
fn probe_for(capacity: usize, blocks: Vec<Vec<Act>>, n: u64) -> (Shared, u64, u64) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(
        NodeKey(1),
        "probe",
        Unforkable(Probe {
            ledger: NoteLedger::new(capacity),
            blocks,
            k: 0,
        }),
    );
    let tally = sink(&mut ed, 2);
    connect(&mut ed, 1, 2);
    ed.commit().unwrap();
    run(&mut exec, 0, n, 64);
    (tally, exec.dropped_event_count(), exec.lost_event_count())
}

fn send(n: u32, on: bool, owner: u32) -> Act {
    Act::Send(if on { ump_on(n) } else { ump_off(n) }, Some(owner))
}

/// Note 1 holds key 60. In block 1 the port is full and note 2 takes the key:
/// the off that ends note 1 is refused (counted, 1), note 2 does not start
/// (skipped, counted, 2). In block 2 the off is paid (given back) and note 3
/// starts and ends on the key, which note 2's off then finds still marked, so
/// it is skipped and counted (3). Two events are lost: note 2's on and off.
#[test]
fn a_refused_mark_outlives_the_off_that_was_paid_and_the_note_that_used_the_key() {
    let (tally, dropped, lost) = probe(
        8,
        vec![
            vec![send(60, true, 1)],
            filled(vec![send(60, true, 2)]),
            vec![send(60, true, 3), send(60, false, 3), send(60, false, 2)],
        ],
    );
    let t = tally.lock();
    assert_eq!(t.ons, 2, "notes 1 and 3");
    assert!(t.held.is_empty());
    assert_eq!((dropped, lost), (3, 2));
}

/// As above, but note 1's own off arrives in the same full block: refused
/// again (3), and the same loss as the one owed, so it is given back at
/// once. Paid in block 2 (given back), note 2's off skipped (4). Lost: 2.
#[test]
fn an_off_refused_twice_is_one_loss() {
    let (tally, dropped, lost) = probe(
        8,
        vec![
            vec![send(60, true, 1)],
            filled(vec![send(60, true, 2), send(60, false, 1)]),
            vec![send(60, false, 2)],
        ],
    );
    assert!(tally.lock().held.is_empty());
    assert_eq!((dropped, lost), (4, 2));
}

/// Three pedals are down and a release finds the port full: the first release is
/// refused (counted by the port, 1) and the other two are not tried but are
/// skipped and counted (3), all three losses. The next block pays them, each
/// attempt giving its count back, so the dropped count stays 3 and the lost
/// count falls by exactly the 3 paid, to 0.
#[test]
fn a_refused_pedal_release_is_counted_and_its_payment_gives_the_count_back() {
    let pedal = |cc| Act::Send(midi::cc(MidiChannel::FIRST, cc, u32::MAX), None);
    let blocks = || {
        vec![
            vec![
                pedal(CCNumber::SUSTAIN),
                pedal(CCNumber::SOSTENUTO),
                pedal(CCNumber::SOFT_PEDAL),
            ],
            filled(vec![Act::Release]),
            vec![],
        ]
    };
    let (_, dropped, lost) = probe_for(8, blocks(), 2);
    assert_eq!((dropped, lost), (3, 3), "refused, then skipped twice");
    let (tally, dropped, lost) = probe(8, blocks());
    assert_eq!(
        (dropped, lost),
        (3, 0),
        "each paid release gives its count back"
    );
    let t = tally.lock();
    assert_eq!(
        t.others,
        8 + 3 + 3,
        "the filler, the pedals down, the releases"
    );
}

/// Two notes on one key are both refused by a full port, and both offs come:
/// each on is a loss and each off is skipped and counted: 4, none given back.
#[test]
fn overlapping_refused_ons_on_one_key_are_each_counted_with_their_offs() {
    let (tally, dropped, lost) = probe(
        8,
        vec![filled(vec![
            send(60, true, 2),
            send(60, true, 3),
            send(60, false, 2),
            send(60, false, 3),
        ])],
    );
    assert_eq!(tally.lock().ons, 0);
    assert_eq!((dropped, lost), (4, 4));
}

/// A table of one key. Note 2 takes key 60 in a full port (note 1's off owed,
/// the key marked refused) and a release then finds the port still full: the
/// row stays for the owed off but loses its mark. Once the off is paid the
/// row is gone, so a note on another key starts. A row that kept its mark
/// would fill the table for good.
#[test]
fn a_release_clears_the_mark_of_a_row_that_stays_owed() {
    let (tally, ..) = probe(
        1,
        vec![
            vec![send(60, true, 1)],
            filled(vec![send(60, true, 2), Act::Release]),
            vec![send(61, true, 4)],
        ],
    );
    let t = tally.lock();
    assert_eq!(t.ons, 2, "note 1, then note 4 on another key");
}

/// An untagged source (a relay) has a note-on refused behind a full port.
/// Its table holds 6 keys, so 6 of the 8 ons leave a refused row and 2 are
/// skipped. The offs of the 6 find their rows: cleared, swallowed and
/// counted like a tagged off; the offs of the other 2 have no row and pass.
/// Six new keys then all start: the table has room again.
///
/// Counted: 32 controllers refused, 8 note-ons (6 refused by the port, 2
/// skipped), 6 offs skipped: 46, none given back.
#[test]
fn an_untagged_off_settles_the_row_of_a_refused_on() {
    let script = vec![
        (0..40).map(ump_cc).chain((0..8).map(ump_on)).collect(),
        (0..8).map(ump_off).collect(),
        (10..16).map(ump_on).collect(),
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(6)),
        capacity: 8,
    };
    let (tally, exec) = through(script, relay, 6);
    let t = tally.lock();
    assert_eq!((t.others, t.ons), (8, 6));
    assert_eq!(t.orphans, 2, "the two offs with no row pass");
    assert_eq!(exec.dropped_event_count(), 46);
    assert_eq!(exec.lost_event_count(), 46);
}

/// An untagged note-on takes a key whose note-off the port refuses (the key is
/// held, owed and marked refused), and the sender's off for it arrives in the
/// same full block: it is refused too, and ends the sender's note on the key,
/// so the mark goes. Paid in the next block, the row is gone, and a table of
/// one key has room for another note.
#[test]
fn an_untagged_off_of_a_held_key_settles_a_refused_on_too() {
    let script = vec![
        vec![ump_on(60)],
        (0..8)
            .map(ump_cc)
            .chain((100..109).map(ump_off))
            .chain([ump_on(60), ump_off(60)])
            .collect(),
        vec![ump_on(61)],
    ];
    let relay = Relay {
        ledger: Some(NoteLedger::new(1)),
        capacity: 8,
    };
    let (tally, _) = through(script, relay, 5);
    assert_eq!(tally.lock().ons, 2, "notes 60 and 61");
}

// ---------------------------------------------------------------------------
// Exact loss counts, whatever the clip does.
// ---------------------------------------------------------------------------

/// Notes with controllers among them, bunched on a few frames so that a block
/// holds more than the port takes. The notes of a key follow one another, or
/// (`overlap`) start wherever they like and overlap. Returns the events, the
/// note-ons and the controllers.
fn spread_clip(r: &mut SplitMix64, overlap: bool) -> (Vec<TimedMidiEvent>, usize, usize) {
    let bunches = [0u64, 10, 70, 500];
    let mut free = std::collections::HashMap::new();
    let mut events = Vec::new();
    let notes = 200 + r.below(500);
    for _ in 0..notes {
        let key = (r.below(4) as u8, r.below(4) as u8, r.below(24) as u8);
        let next = free.entry(key).or_insert(0u64);
        let bunch = bunches[r.below(4) as usize];
        let start = if overlap { bunch } else { (*next).max(bunch) };
        let end = if r.below(2) == 0 {
            // Many notes end on one frame, past the port's room for offs.
            start.max([100u64, 110, 600][r.below(3) as usize]) + 1
        } else {
            start + 1 + r.below(300)
        };
        *next = end + 1;
        let (g, c, n) = (MidiGroup::of(key.0), MidiChannel::of(key.1), key.2);
        events.push(at(
            start,
            midi::note_on(tutti_ump::Address::new(g, c), n, 0xFFFF),
        ));
        events.push(at(end, midi::note_off(tutti_ump::Address::new(g, c), n, 0)));
    }
    let ccs = 100 + r.below(300);
    for v in 0..ccs {
        let f = bunches[r.below(4) as usize];
        let cc = midi::cc(MidiChannel::FIRST, CCNumber::VOLUME, v as u32);
        events.push(at(f, cc));
    }
    (events, notes as usize, ccs as usize)
}

/// Frames to play a clip through: past its last event, to a block edge.
fn span(events: &[TimedMidiEvent]) -> u64 {
    let last = events
        .iter()
        .map(|e| (e.beat().get() * FRAMES_PER_BEAT).round() as u64)
        .max()
        .unwrap_or(0);
    (last / 64 + 2) * 64
}

/// Plays `events` for `passes` passes of a loop (none: straight through),
/// then stops for 12 blocks.
fn play(exec: &mut Executor, events: &[TimedMidiEvent], passes: u64) {
    let len = span(events);
    let lp = (passes > 1).then(|| LoopRange::new(Beat::new(0.0), beat_of(len)).expect("a loop"));
    for f in 0..len * passes {
        if f % 64 == 0 {
            let t = TransportPosition::new(true, Bpm::of(120.0), beat_of(f % len), lp);
            exec.process(64, &t, &[], &mut []);
        }
    }
    (0..12).for_each(|_| exec.process(64, &stopped(0), &[], &mut []));
}

/// A clip whose notes never overlap on a key, played straight through, looped
/// and replaced, loses exactly what it could not deliver: every note-on the
/// sink did not hear is one loss, and so is its note-off (skipped, counted
/// once), and every controller it did not hear is one. Offs the port refused
/// are given back when they go out. After the stop nothing sounds, every
/// note-on has its note-off, and no off is orphaned.
#[test]
fn the_loss_count_is_exact_through_loops_and_replacements() {
    let (mut refused, mut recovered) = (0, 0);
    for seed in 1..=300u64 {
        let mut r = SplitMix64::new(seed);
        let overlap = r.below(2) == 0;
        let (first, ons1, ccs1) = spread_clip(&mut r, overlap);
        let passes = 1 + r.below(2);
        let (second, ons2, ccs2) = spread_clip(&mut r, overlap);
        let replace = r.below(2) == 0;
        let (mut ed, mut exec) = Editor::new(Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
        ));
        let controls: MidiClipControls = ed
            .insert(NodeKey(1), "clip", MidiClipNode::new(first.clone()))
            .controls;
        let tally = sink(&mut ed, 2);
        connect(&mut ed, 1, 2);
        ed.commit().unwrap();
        play(&mut exec, &first, passes);
        let (mut ons, mut ccs) = (ons1 * passes as usize, ccs1 * passes as usize);
        if replace {
            controls.set(second.iter().copied().collect::<ClipEvents>());
            play(&mut exec, &second, 1);
            ons += ons2;
            ccs += ccs2;
        }
        exec.process(64, &stopped(0), &[], &mut []);
        let t = tally.lock();
        assert!(t.held.is_empty(), "seed {seed}");
        assert_eq!(t.ons, t.offs, "seed {seed}");
        assert_eq!(t.orphans, 0, "seed {seed}");
        let undelivered = (ons - t.ons) as u64;
        let controllers = (ccs - t.others) as u64;
        assert_eq!(
            exec.lost_event_count(),
            2 * undelivered + controllers,
            "seed {seed}"
        );
        refused += exec.dropped_event_count();
        recovered += exec.dropped_event_count() - exec.lost_event_count();
    }
    assert!(refused > 0, "no event was ever refused");
    assert!(recovered > 0, "no refused off was ever given back");
}

/// Block 0 sends `script` through a ledger; block 1 releases it all.
struct SendThenRelease {
    ledger: NoteLedger<()>,
    script: Vec<Ump>,
    block: u32,
}

impl SendThenRelease {
    const SHAPE: Shape = Shape::EMPTY
        .with_events(0, 1)
        .with_event_capacity(std::num::NonZeroU32::new(64).expect("a capacity"));
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for SendThenRelease {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let first = cx.env.offset(0).unwrap();
        let mut sending = self.ledger.block(io.event_out(Self::OUT), first);
        if self.block == 0 {
            for &u in &self.script {
                sending.send(first, u, None);
            }
        } else {
            sending.release_all(first);
        }
        self.block += 1;
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A MIDI 1.0 pedal and bend are returned in MIDI 1.0.** The ledger sends
/// the return in the protocol the message it was told about used: a MIDI 1.0
/// Control Change 64 with value 0 and a MIDI 1.0 Pitch Bend at 0x2000, never
/// a MIDI 2.0 packet (M1 p. 31, Stop; the 14-bit centre is 0x2000, M1 table
/// of Pitch Bend). A pedal value below 64 is up (M1 CC 64: 0 to 63 is off), so a
/// pedal sent at 63 is not released again.
#[test]
fn a_midi_1_pedal_and_bend_are_returned_in_midi_1() {
    use tutti_ump::{Message, U7, U14, Voice1};
    let ch = MidiChannel::of(2);
    let m1_bend = |v: u16| Ump::from(Message::midi1(ch, Voice1::PitchBend(U14::of(v))));
    let m1_pedal = |controller, v: u8| midi::m1_cc(ch, controller, U7::of(v));
    let script = vec![
        m1_pedal(CCNumber::SUSTAIN, 127),
        m1_pedal(CCNumber::SOSTENUTO, 63),
        m1_bend(0x3000),
    ];
    let node = SendThenRelease {
        ledger: NoteLedger::new(16),
        script,
        block: 0,
    };
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    d.block();
    d.block();
    let released: Vec<Ump> = d
        .event_output(SendThenRelease::OUT)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(u) => u,
            _ => unreachable!("the node sends MIDI"),
        })
        .collect();
    assert_eq!(
        released,
        vec![m1_pedal(CCNumber::SUSTAIN, 0), m1_bend(0x2000)]
    );
}
