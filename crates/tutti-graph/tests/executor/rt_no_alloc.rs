//! Regression gate: `Executor::process` never allocates once a plan is
//! applied — through PDC rings, event delays, event fan-in merges, feedback of
//! both kinds, in-place aliasing, the silence skip, scheduled commands
//! landing (on time, late, and still waiting), writers refusing past a declared event capacity, and block lengths that change
//! every call.
//!
//! Each gate runs twice: on the serial executor and on the parallel one
//! (`src/par.rs`), whose every participant — the helper
//! threads as well as the caller — runs its share of each block inside
//! `assert_no_alloc` (`common::TestPool::no_alloc`), so an allocation or a
//! free on any of them aborts. What the parallel gate cannot see: the pool's
//! own idle loop between jobs (the test pool spins; `tutti_graph::WorkerPool`'s
//! park and wake are covered by `tutti-engine`'s `rt_no_alloc_engine`).
//!
//! Applying a commit is inside the gate too (`applying_*`): it moves and
//! swaps what the editor built with the commit. What this cannot cover,
//! stated rather than implied: a node that allocates is the node's bug,
//! which this test catches only for the nodes it runs.

use tutti_graph::testing as common;
use tutti_graph::{Curvature, EventTapOptions, Exponent};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use common::{Kind, TestNode, prepare};

use tutti_buffers::ChannelLayout;
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    Buses, Cx, Editor, EventEdge, EventInputPort, EventKind, EventOutputPort, Fade, FadeLaw, Io,
    Law, Node, ParamKind, ParamRamp, ParamSegment, Prepare, Scheduled, Shape, Status,
    TransportPosition, Unforkable,
};
use tutti_graph::{Edge, FeedbackFrom, InputPort, OutputPort, PortSource};
use tutti_ump::{CCNumber, Message, MidiChannel, NoteNumber, U7, U14, Ump, Voice1};
use tutti_units::{At, Beat, Frame, Latency, Samples, Tail};

/// Run `exec`'s blocks on a [`common::TestPool`] of `workers` participants
/// whose every participant runs its share inside `assert_no_alloc` — the
/// parallel path, helpers included (`1`: serial, no pool). Set before the
/// first commit is applied, so the parallel state is built with the plan,
/// outside the gate, as applying is.
fn use_pool(exec: &mut tutti_graph::Executor, workers: usize) {
    if workers > 1 {
        exec.set_pool(Some(common::TestPool::no_alloc(workers)));
    }
}

/// Applies every queued commit under the gate: applying allocates nothing.
fn apply(exec: &mut tutti_graph::Executor) {
    exec.apply_pending();
}

fn at(node: u64, port: u16) -> InputPort {
    InputPort {
        node: NodeKey(node),
        port,
    }
}

fn from(node: u64, port: u16) -> Edge {
    Edge::Direct(PortSource::Node(OutputPort {
        node: NodeKey(node),
        port,
    }))
}

fn process_is_allocation_free_in_steady_state_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        NodeKey(1),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 5,
            phase: 0,
        })),
    );
    ed.insert(
        NodeKey(2),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 7,
            phase: 3,
        })),
    );
    ed.insert(
        NodeKey(3),
        "evlag",
        Unforkable(TestNode::new(Kind::EventLag { latency: 11 })),
    );
    ed.insert(
        NodeKey(4),
        "consume",
        Unforkable(TestNode::new(Kind::Consumer { inputs: 2 })),
    );
    ed.insert(
        NodeKey(5),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 40 })),
    );
    ed.insert(
        NodeKey(6),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(7),
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 3 })),
    );
    ed.insert(
        NodeKey(8),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(9),
        "gain",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        })),
    );

    let spec = ed.spec_mut();
    let ev = |node, port| EventOutputPort {
        node: NodeKey(node),
        port,
    };
    spec.connect_events(
        EventInputPort {
            node: NodeKey(3),
            port: 0,
        },
        EventEdge::Direct(ev(1, 0)),
    );
    // Fan-in, a PDC-delayed source (3 is 11 frames late), and feedback.
    spec.connect_events(
        EventInputPort {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(ev(3, 0)),
    );
    spec.connect_events(
        EventInputPort {
            node: NodeKey(4),
            port: 0,
        },
        EventEdge::Direct(ev(2, 0)),
    );
    spec.connect_events(
        EventInputPort {
            node: NodeKey(4),
            port: 1,
        },
        EventEdge::feedback(ev(3, 0), Samples::new(256)),
    );
    let t = &mut spec.topology;
    t.edges
        .insert(at(5, 0), vec![Edge::Direct(PortSource::Global(0))]);
    t.edges.insert(at(6, 0), vec![from(4, 0)]);
    t.edges.insert(at(7, 0), vec![from(5, 0)]);
    t.edges.insert(at(7, 1), vec![from(6, 0)]);
    t.edges.insert(
        at(7, 2),
        vec![Edge::Feedback(FeedbackFrom::one_block(
            OutputPort {
                node: NodeKey(9),
                port: 0,
            },
            Samples::new(256),
        ))],
    );
    t.edges.insert(at(8, 0), vec![from(7, 0)]);
    t.edges.insert(at(9, 0), vec![from(8, 0)]);
    t.set_outputs(vec![
        PortSource::Node(OutputPort {
            node: NodeKey(9),
            port: 0,
        }),
        PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        }),
    ]);
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().len() >= 2,
        "the gate must run through PDC rings"
    );

    // Scheduled commands, landing throughout the gated run: into the
    // consumer's fan-in port (merged after its two sources) and its feedback
    // port, on time; one already late; one waiting on a beat the stopped
    // transport never reaches.
    let consumer = |port| EventInputPort {
        node: NodeKey(4),
        port,
    };
    for k in 0..200u64 {
        let kind: Scheduled = if k % 3 == 0 {
            ParamKind::Ramp(ParamRamp::by_id(
                tutti_units::ParamId::new(1),
                0.5,
                Samples::new(16),
            ))
            .into()
        } else {
            EventKind::Midi(midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100))).into()
        };
        ed.schedule(
            At::Frame(Frame::new(2_000 + k * 700)),
            consumer((k % 2) as u16),
            kind,
        )
        .expect("under capacity");
    }

    let input = vec![0.25f32; 256];
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let transport = TransportPosition::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    // Warm up: every size once, so any lazily-sized state is sized.
    for &n in &sizes {
        exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
    }
    // After the warm-up, so frame 0 is past: it lands late, inside the gate.
    ed.schedule(
        At::Frame(Frame::new(0)),
        consumer(0),
        EventKind::Midi(Ump([0; 4])),
    )
    .expect("under capacity");
    ed.schedule(
        At::Beat(Beat::new(4.0)),
        consumer(0),
        EventKind::Midi(Ump([0; 4])),
    )
    .expect("under capacity");
    assert_eq!(ed.commands_outstanding(), 202);

    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for i in 0..2_000 {
        let n = sizes[i % sizes.len()];
        exec.process(n, &transport, &[&input[..]], &mut [&mut l[..], &mut r[..]]);
    }
    assert_eq!(exec.dropped_event_count(), 0);
    assert!(l.iter().any(|&x| x != 0.0), "the gate rendered signal");
    assert_eq!(exec.late_command_count(), 1);
    assert_eq!(
        ed.commands_outstanding(),
        1,
        "every timed command landed inside the gate; the beat still waits"
    );
}

#[test]
fn process_is_allocation_free_in_steady_state() {
    process_is_allocation_free_in_steady_state_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_process_is_allocation_free_in_steady_state() {
    process_is_allocation_free_in_steady_state_on(4);
}

/// A node's declared event-input bound is held without allocating: three
/// emitters fan into a consumer accepting 5 events a block, so its merge cuts
/// every block, and a third emitter wired in by a later commit is held to the
/// bound too.
fn a_bounded_event_merge_is_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    for (key, period) in [(1u64, 5u64), (2, 7), (3, 3)] {
        ed.insert(
            NodeKey(key),
            "emit",
            Unforkable(TestNode::new(Kind::Emitter { period, phase: 0 })),
        );
    }
    ed.insert(
        NodeKey(4),
        "consume",
        Unforkable(TestNode::new(Kind::BoundedConsumer { inputs: 1, cap: 5 })),
    );
    let wire = |ed: &mut Editor, from: u64| {
        ed.spec_mut().connect_events(
            EventInputPort {
                node: NodeKey(4),
                port: 0,
            },
            EventEdge::Direct(EventOutputPort {
                node: NodeKey(from),
                port: 0,
            }),
        );
    };
    wire(&mut ed, 1);
    wire(&mut ed, 2);
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(4),
            port: 0,
        })]);
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let transport = TransportPosition::default();
    let mut out = vec![0.0f32; 256];
    exec.process(256, &transport, &[], &mut [&mut out[..]]);
    // Frames 0..256 hold 52 multiples of 5 and 37 of 7.
    assert_eq!(exec.dropped_event_count(), 52 + 37 - 5);
    let before = exec.dropped_event_count();
    wire(&mut ed, 3);
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    for _ in 0..200 {
        exec.process(256, &transport, &[], &mut [&mut out[..]]);
    }
    assert!(
        exec.dropped_event_count() >= before + 200 * (85 + 51 + 36 - 5),
        "every block lost what the bound cut"
    );
}

#[test]
fn a_bounded_event_merge_is_allocation_free() {
    a_bounded_event_merge_is_allocation_free_on(1);
}

#[test]
fn a_bounded_event_merge_is_allocation_free_in_parallel() {
    a_bounded_event_merge_is_allocation_free_on(4);
}

/// Crossfades never allocate on the audio thread: both units running, the
/// blend, a fade's end (the crossfade and its outgoing unit onto the
/// fade-return ring) and the start of the fade waiting behind it —
/// on an in-place stereo gain and on an event consumer (the general form).
/// Everything a fade does runs inside the gate.
fn crossfades_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    let gain = |g| TestNode::new(Kind::Gain { gain: g, width: 2 });
    ed.insert(NodeKey(1), "gain", Unforkable(gain(1.0)));
    ed.insert(
        NodeKey(2),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 9,
            phase: 0,
        })),
    );
    ed.insert(
        NodeKey(3),
        "consume",
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
    );
    let consumer = EventInputPort {
        node: NodeKey(3),
        port: 0,
    };
    ed.spec_mut().connect_events(
        consumer,
        EventEdge::Direct(EventOutputPort {
            node: NodeKey(2),
            port: 0,
        }),
    );
    let t = &mut ed.spec_mut().topology;
    t.edges
        .insert(at(1, 0), vec![Edge::Direct(PortSource::Global(0))]);
    t.edges
        .insert(at(1, 1), vec![Edge::Direct(PortSource::Global(1))]);
    t.set_outputs(vec![
        PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        }),
        PortSource::Node(OutputPort {
            node: NodeKey(3),
            port: 0,
        }),
    ]);
    ed.commit().expect("commits");

    let input = vec![0.25f32; 256];
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let transport = TransportPosition::default();
    let mut block = |exec: &mut tutti_graph::Executor, n: usize| {
        exec.process(
            n,
            &transport,
            &[&input[..], &input[..]],
            &mut [&mut l[..], &mut r[..]],
        );
    };
    block(&mut exec, 64);
    ed.collect();

    // A long fade, one queued behind it, and one on the event node.
    let fade = |n| Fade::new(Samples::new(n)).law(FadeLaw::EqualPower);
    ed.replace(NodeKey(1), Unforkable(gain(2.0)), fade(3_000))
        .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    ed.replace(NodeKey(1), Unforkable(gain(4.0)), fade(2_000))
        .expect("fits");
    ed.replace(
        NodeKey(3),
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
        fade(500),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 64);
    assert!(ed.collect().is_empty(), "nothing has retired yet");
    assert_eq!(ed.in_flight(), 0, "fades hold no commit");
    assert_eq!(ed.fades_in_flight(), 3);

    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for i in 0..200 {
        block(&mut exec, sizes[i % sizes.len()]);
    }
    // Three outgoing units came back: both fades at key 1 and the one at 3.
    let mut back = ed.collect();
    back.sort();
    assert_eq!(back, vec![NodeKey(1), NodeKey(1), NodeKey(3)]);
    assert_eq!(ed.fades_in_flight(), 0);
}

#[test]
fn crossfades_are_allocation_free() {
    crossfades_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_crossfades_are_allocation_free() {
    crossfades_are_allocation_free_on(4);
}

/// An instrument stand-in: counts the events it receives, and writes three
/// messages for its successor when replaced (`Node::chase`).
struct HeldChaser(Arc<AtomicU64>);

impl Node for HeldChaser {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO))
            .with_events(1, 0)
            .with_chase(4)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.0
            .fetch_add(io.events(0).len() as u64, Ordering::Relaxed);
        io.output(0).fill(0.25);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, out: &mut tutti_graph::Chase<'_>) {
        for i in 0..3 {
            out.push(midi::m1_note_on(MidiChannel::FIRST, 60 + i, U7::of(100)));
        }
    }
}

/// **A chase held behind a running fade is delivered without allocating**:
/// A fades to B, C waits behind B, and inside the gate B's fade ends, C takes
/// over and its first block carries B's three messages.
/// The count of what C received proves the delivery ran inside the gate.
fn a_held_chase_is_delivered_allocation_free_on(workers: usize) {
    const KEYS: u64 = 6;
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    let counters = || {
        (0..KEYS)
            .map(|_| Arc::new(AtomicU64::new(0)))
            .collect::<Vec<_>>()
    };
    let (a, b, c) = (counters(), counters(), counters());
    for k in 0..KEYS {
        ed.insert(
            NodeKey(k + 1),
            "chaser",
            Unforkable(HeldChaser(Arc::clone(&a[k as usize]))),
        );
    }
    ed.spec_mut().topology.set_outputs((1..=KEYS).map(|k| {
        PortSource::Node(OutputPort {
            node: NodeKey(k),
            port: 0,
        })
    }));
    ed.commit().expect("commits");
    let mut out = vec![vec![0.0f32; 64]; KEYS as usize];
    let transport = TransportPosition::default();
    let mut block = |exec: &mut tutti_graph::Executor| {
        // An array: the gate allows no allocation, a block's slice list included.
        let mut planes = out.iter_mut();
        let mut slices: [&mut [f32]; KEYS as usize] =
            std::array::from_fn(|_| planes.next().expect("one plane a key").as_mut_slice());
        exec.process(64, &transport, &[], &mut slices);
    };
    block(&mut exec);
    let fade = |n| Fade::new(Samples::new(n)).law(FadeLaw::EqualPower);
    for k in 0..KEYS {
        ed.replace(
            NodeKey(k + 1),
            Unforkable(HeldChaser(Arc::clone(&b[k as usize]))),
            fade(256),
        )
        .expect("fits");
    }
    ed.commit().expect("commits");
    block(&mut exec);
    for k in 0..KEYS {
        ed.replace(
            NodeKey(k + 1),
            Unforkable(HeldChaser(Arc::clone(&c[k as usize]))),
            fade(256),
        )
        .expect("fits");
    }
    ed.commit().expect("commits");
    // The commit queuing the third is applied here, outside the gate.
    block(&mut exec);
    for (b, c) in b.iter().zip(&c) {
        assert_eq!(b.load(Ordering::Relaxed), 3, "A's chase reached B");
        assert_eq!(c.load(Ordering::Relaxed), 0, "C waits behind B's fade");
    }
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for _ in 0..16 {
        block(&mut exec);
    }
    for seen in &c {
        assert_eq!(
            seen.load(Ordering::Relaxed),
            3,
            "C took over inside the gate and received B's chase, once"
        );
    }
    assert_eq!(exec.dropped_event_count(), 0);
}

#[test]
fn a_held_chase_is_delivered_allocation_free() {
    a_held_chase_is_delivered_allocation_free_on(1);
}

#[test]
fn parallel_a_held_chase_is_delivered_allocation_free() {
    a_held_chase_is_delivered_allocation_free_on(4);
}

/// **A replace queued behind a running fade is applied without allocating**:
/// B fades in over A, and the commit replacing B with C (a fade that waits
/// behind B's) is applied inside the gate. B's chase of three messages is
/// held by C's crossfade, whose buffer the editor sized when it built the
/// commit; the count of what C later receives proves the chase was held.
#[test]
fn a_replace_queued_behind_a_running_fade_chases_without_allocating() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let (a, b, c) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    ed.insert(NodeKey(1), "chaser", Unforkable(HeldChaser(Arc::clone(&a))));
    ed.spec_mut()
        .topology
        .set_outputs([PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    let mut out = vec![0.0f32; 64];
    let transport = TransportPosition::default();
    let mut block = |exec: &mut tutti_graph::Executor| {
        let mut slices: [&mut [f32]; 1] = [out.as_mut_slice()];
        exec.process(64, &transport, &[], &mut slices);
    };
    block(&mut exec);
    let fade = |n| Fade::new(Samples::new(n)).law(FadeLaw::EqualPower);
    ed.replace(
        NodeKey(1),
        Unforkable(HeldChaser(Arc::clone(&b))),
        fade(256),
    )
    .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec);
    assert_eq!(b.load(Ordering::Relaxed), 3, "A's chase reached B");
    ed.replace(
        NodeKey(1),
        Unforkable(HeldChaser(Arc::clone(&c))),
        fade(256),
    )
    .expect("fits");
    ed.commit().expect("commits");
    // The commit queuing C behind B's running fade is applied inside the gate.
    apply(&mut exec);
    block(&mut exec);
    assert_eq!(c.load(Ordering::Relaxed), 0, "C waits behind B's fade");
    for _ in 0..16 {
        block(&mut exec);
    }
    assert_eq!(
        c.load(Ordering::Relaxed),
        3,
        "C took over and received B's chase, once"
    );
    assert_eq!(exec.dropped_event_count(), 0);
}

/// Pushes `burst` events on the first frame of every 256-frame bar into a
/// port declaring `cap` per block, counting what its writer refuses.
struct Spray {
    cap: u32,
    burst: u32,
    refused: Arc<AtomicU64>,
}

impl Node for Spray {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(tutti_graph::testing::cap_of(self.cap))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for at in cx.env.offsets() {
            if cx.env.frame_at(at).get().is_multiple_of(256) {
                for i in 0..self.burst {
                    let e = tutti_graph::Event::midi(
                        at,
                        Message::midi1(
                            MidiChannel::FIRST,
                            Voice1::note_on(NoteNumber::of(60), U7::of(100 | i as u8)),
                        ),
                    );
                    if io.event_out(0).push(e).is_err() {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Declares 141 frames of latency and an event output it never writes: it
/// puts PDC delays on its sink's other event edges.
struct LateEvents;

impl Node for LateEvents {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_latency(Latency::new(Samples::new(141)))
            .with_tail(Tail::Finite(Samples::new(141)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Counts the events on each of its two event inputs.
struct Tally([Arc<AtomicU64>; 2]);

impl Node for Tally {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(2, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for (p, n) in self.0.iter().enumerate() {
            n.fetch_add(io.events(p).len() as u64, Ordering::Relaxed);
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Declared event capacities (`Shape::event_capacity`) never allocate on
/// the audio thread: two ports declaring 4 events per block, each handed a
/// burst of 6 every bar (the writer refusing 2 inside the gate), fan into
/// one input through a 141-frame PDC delay, and one also feeds back, with
/// the executor's default capacity at **1** — so every buffer on the way
/// must have been sized from the declarations. Nothing but the writers
/// refuses: the executor's drop count is exactly what the nodes were told.
fn declared_event_capacities_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::with_event_capacity(prepare(256), 1);
    use_pool(&mut exec, workers);
    let refused = Arc::new(AtomicU64::new(0));
    let seen = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    for k in [1, 2] {
        ed.insert(
            NodeKey(k),
            "spray",
            Unforkable(Spray {
                cap: 4,
                burst: 6,
                refused: Arc::clone(&refused),
            }),
        );
    }
    ed.insert(NodeKey(3), "late", Unforkable(LateEvents));
    ed.insert(NodeKey(4), "tally", Unforkable(Tally(seen.clone())));
    let ev = |node| EventOutputPort {
        node: NodeKey(node),
        port: 0,
    };
    let tally = |port| EventInputPort {
        node: NodeKey(4),
        port,
    };
    let spec = ed.spec_mut();
    for from in [1, 2, 3] {
        spec.connect_events(tally(0), EventEdge::Direct(ev(from)));
    }
    spec.connect_events(tally(1), EventEdge::feedback(ev(1), Samples::new(300)));
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let plan = exec.plan().expect("applied");
    assert!(
        plan.delays().len() >= 2,
        "both sprays reach the tally through a PDC delay"
    );

    let transport = TransportPosition::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    for &n in &sizes {
        exec.process(n, &transport, &[], &mut []);
    }
    let before = refused.load(Ordering::Relaxed);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for i in 0..2_000 {
        exec.process(sizes[i % sizes.len()], &transport, &[], &mut []);
    }
    let refused = refused.load(Ordering::Relaxed);
    assert!(refused > before, "writers refused inside the gate");
    assert_eq!(
        exec.dropped_event_count(),
        refused,
        "only the writers refused anything"
    );
    assert!(seen[0].load(Ordering::Relaxed) > 0 && seen[1].load(Ordering::Relaxed) > 0);
}

#[test]
fn declared_event_capacities_are_allocation_free() {
    declared_event_capacities_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_declared_event_capacities_are_allocation_free() {
    declared_event_capacities_are_allocation_free_on(4);
}

/// More places lose events than a block's log holds (64) and the control side
/// never drains the ring (256 entries): 80 writers, each two events over its
/// port, 100 blocks. The log fills every block, the ring fills in the fourth,
/// and nothing allocates on any participant, serial or parallel. What no entry
/// names is counted instead, so entries plus the unreported count are exactly
/// the total.
fn losses_past_the_log_and_the_ring_are_allocation_free_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    let refused = Arc::new(AtomicU64::new(0));
    for k in 1..=80u64 {
        ed.insert(
            NodeKey(k),
            "spray",
            Unforkable(Spray {
                cap: 4,
                burst: 6,
                refused: Arc::clone(&refused),
            }),
        );
    }
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    let transport = TransportPosition::default();
    // No warm-up block: the first one is the first to fill the log, and a log
    // that grows would show there.
    for _ in 0..101 {
        exec.process(256, &transport, &[], &mut []);
    }
    let total = exec.dropped_event_count();
    assert_eq!(total, 101 * 80 * 2);
    let named: u64 = ed.take_event_losses().map(|l| u64::from(l.count)).sum();
    let unreported = ed.take_unreported_event_losses();
    assert!(unreported > 0, "the log and the ring were full");
    assert_eq!(named + unreported, total, "nothing is lost silently");
}

#[test]
fn losses_past_the_log_and_the_ring_do_not_allocate() {
    losses_past_the_log_and_the_ring_are_allocation_free_on(1);
}

#[test]
fn parallel_losses_past_the_log_and_the_ring_do_not_allocate() {
    losses_past_the_log_and_the_ring_are_allocation_free_on(4);
}

/// Declares `Cutoff` and `Q` (bases 1.0 and 2.0); writes the sum of what it
/// reads for them.
struct ParamSink;

impl Node for ParamSink {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_params(&[tutti_units::UnitParam::Cutoff, tutti_units::UnitParam::Q])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let a = io.param(0).frames().map_or(1.0, |v| v[i]);
            let b = io.param(1).frames().map_or(2.0, |v| v[i]);
            io.output(0)[i] = a + b;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        [1.0, 2.0].get(port).copied()
    }
}

/// A ramp or a curve segment on `Cutoff` every 37 frames (every shape
/// class, a resumed one).
struct RampEvery;

impl Node for RampEvery {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_param_events()
            .with_event_capacity(tutti_graph::testing::cap_of(16))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for i in 0..io.frames() {
            let f = start + i as u64;
            if f.is_multiple_of(37) {
                let o = io.offset(i).expect("inside");
                let event = match (f / 37) % 3 {
                    // A linear ramp, from wherever the source is.
                    0 => tutti_graph::ParamEvent::ramp(
                        o,
                        ParamRamp::new(
                            tutti_units::ParamKey::<tutti_units::Hz>::CUTOFF,
                            tutti_units::Hz::new((f % 5) as f64),
                            Samples::new(20),
                        ),
                    ),
                    // A curve segment spanning the next event: interrupted.
                    k => {
                        let shape = [
                            Law::PowerIn(Exponent::of(2.0)),
                            Law::Exp(Curvature::of(3.0)),
                            Law::PowerInOut(Exponent::of(1.5)),
                            Law::Hold,
                        ][(f / 111) as usize % 4];
                        let seg = ParamSegment::new(
                            tutti_units::ParamKey::<tutti_units::Hz>::CUTOFF,
                            tutti_units::Hz::new((f % 3) as f64),
                            tutti_units::Hz::new((f % 7) as f64),
                            Samples::new(50),
                        )
                        .law(shape);
                        let seg = if k == 2 {
                            seg.resumed(Samples::new(3), Samples::new(41))
                        } else {
                            seg
                        };
                        tutti_graph::ParamEvent::segment(o, seg)
                    }
                };
                let _ = io.event_out(0).push_param(event);
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Compiler-owned param modulation never allocates on the audio thread:
/// the fused step over audio sources (one delayed by PDC behind a latent
/// sibling), an event source of ramps and curve segments (every shape
/// class) and LUT shapings, the declick of a connection
/// made and one broken, blocks of every length, and ranges moved between
/// blocks without a commit. The declicks the commits start, and every block
/// that reads a moved range, run inside the gate.
fn modulated_params_are_allocation_free_on(workers: usize) {
    use tutti_graph::ParamIn;
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(NodeKey(1), "sink", Unforkable(ParamSink));
    ed.insert(
        NodeKey(2),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 31 })),
    );
    ed.insert(NodeKey(4), "ramps", Unforkable(RampEvery));
    {
        let t = &mut ed.spec_mut().topology;
        t.edges
            .insert(at(2, 0), vec![Edge::Direct(PortSource::Global(0))]);
        t.edges.insert(at(1, 0), vec![from(2, 0)]);
        t.set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    }
    let cut = ParamIn {
        node: NodeKey(1),
        param: tutti_units::UnitParam::Cutoff.into(),
    };
    let q = ParamIn {
        node: NodeKey(1),
        param: tutti_units::UnitParam::Q.into(),
    };
    // The ramps are the early path: the sink sits behind the lag's PDC, so
    // the plan delays its event source.
    let ramps = EventOutputPort {
        node: NodeKey(4),
        port: 0,
    };
    ed.spec_mut().connect_param(cut, ramps);
    ed.commit().expect("commits");

    let input: Vec<f32> = (0..256).map(|i| (i % 13) as f32 / 13.0).collect();
    let mut out = vec![0.0f32; 256];
    let transport = TransportPosition::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    let mut run = |exec: &mut tutti_graph::Executor| {
        for i in 0..200 {
            let n = sizes[i % sizes.len()];
            exec.process(n, &transport, &[&input[..n]], &mut [&mut out[..n]]);
        }
    };
    apply(&mut exec);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    run(&mut exec);

    // A connection made (Q, behind the lag's PDC: the ramps are the early
    // path) and one broken: both declick inside the gate.
    ed.collect();
    ed.spec_mut().connect_param(q, ramps);
    ed.spec_mut().disconnect_param(cut, ramps);
    ed.commit().expect("commits");
    apply(&mut exec);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    run(&mut exec);

    // Ranges moved between blocks (`Editor::set_param_range`, on this
    // control thread, outside the gate): every block that reads a moved
    // range runs inside it. No commit, so nothing to apply.
    ed.collect();
    let ranges = [
        tutti_graph::ParamRange::new(0.0, 0.5),
        tutti_graph::ParamRange::new(2.0, -1.0),
        tutti_graph::ParamRange::UNBOUNDED,
    ];
    for i in 0..60 {
        ed.set_param_range(cut, ranges[i % 3]).expect("a range");
        ed.set_param_range(q, ranges[(i + 1) % 3]).expect("a range");
        let n = sizes[i % sizes.len()];
        exec.process(n, &transport, &[&input[..n]], &mut [&mut out[..n]]);
    }
    assert_eq!(ed.in_flight(), 0, "a range move sends nothing");
    let plan = exec.plan().expect("a plan");
    assert!(
        plan.delays()
            .iter()
            .any(|d| matches!(d.key, tutti_graph::DelayKey::ParamEvent { .. })),
        "a param source was delayed by PDC inside the gate"
    );
}

#[test]
fn modulated_params_are_allocation_free() {
    modulated_params_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants, every one of them
/// (the helpers too) inside the gate.
#[test]
fn parallel_modulated_params_are_allocation_free() {
    modulated_params_are_allocation_free_on(4);
}

/// Counts the param events on each of its two event inputs; one audio input,
/// no outputs.
struct ParamTally([Arc<AtomicU64>; 2]);

impl Node for ParamTally {
    fn shape(&self) -> Shape {
        Shape::sink_unassigned(ChannelLayout::MONO)
            .with_events(2, 0)
            .with_param_events()
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for (p, n) in self.0.iter().enumerate() {
            n.fetch_add(io.events(p).params().len() as u64, Ordering::Relaxed);
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Param events never allocate on the audio thread in the buffer beside the
/// events: two sources fan into one event input (a merge of param lists),
/// each early behind a latent audio path so the plan delays it (a FIFO of
/// param events), and a third arrives through a feedback edge (a FIFO read
/// before it is written). Blocks of every length, and the events counted at the
/// consumer to show the lists carried them.
fn param_events_are_allocation_free_on(workers: usize) {
    let counts = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        NodeKey(1),
        "consumer",
        Unforkable(ParamTally([Arc::clone(&counts[0]), Arc::clone(&counts[1])])),
    );
    ed.insert(
        NodeKey(2),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 31 })),
    );
    for k in [4, 5, 6] {
        ed.insert(NodeKey(k), "ramps", Unforkable(RampEvery));
    }
    {
        let t = &mut ed.spec_mut().topology;
        t.edges
            .insert(at(2, 0), vec![Edge::Direct(PortSource::Global(0))]);
        t.edges.insert(at(1, 0), vec![from(2, 0)]);
        t.set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(2),
            port: 0,
        })]);
    }
    let port = |p| EventInputPort {
        node: NodeKey(1),
        port: p,
    };
    let out = |k| EventOutputPort {
        node: NodeKey(k),
        port: 0,
    };
    ed.spec_mut()
        .connect_events(port(0), EventEdge::Direct(out(4)));
    ed.spec_mut()
        .connect_events(port(0), EventEdge::Direct(out(5)));
    ed.spec_mut().connect_events(
        port(1),
        EventEdge::Feedback {
            from: out(6),
            delay: Samples::new(256),
        },
    );
    ed.commit().expect("commits");

    let input: Vec<f32> = (0..256).map(|i| (i % 13) as f32 / 13.0).collect();
    let mut audio = vec![0.0f32; 256];
    let transport = TransportPosition::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    apply(&mut exec);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for i in 0..400 {
        let n = sizes[i % sizes.len()];
        exec.process(n, &transport, &[&input[..n]], &mut [&mut audio[..n]]);
    }
    let plan = exec.plan().expect("a plan");
    let event_delays = plan
        .delays()
        .iter()
        .filter(|d| matches!(d.key, tutti_graph::DelayKey::Event { .. }))
        .count();
    assert_eq!(event_delays, 2, "both fan-in sources are delayed by PDC");
    assert!(
        counts[0].load(Ordering::Relaxed) > 100 && counts[1].load(Ordering::Relaxed) > 100,
        "the consumer read param events on both inputs: {counts:?}"
    );
    assert_eq!(exec.dropped_event_count(), 0, "nothing was refused");
}

/// Scheduled param commands never allocate on the audio thread as they land:
/// a ramp or a segment into each of a consumer's two event inputs on
/// frames spread over blocks of every length, one already late, and one on a
/// beat a stopped transport never reaches. Each lands in the param list the
/// consumer counts, so the merge into the overlay ran.
fn scheduled_params_are_allocation_free_on(workers: usize) {
    let counts = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(
        NodeKey(1),
        "consumer",
        Unforkable(ParamTally([Arc::clone(&counts[0]), Arc::clone(&counts[1])])),
    );
    ed.insert(
        NodeKey(2),
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 31 })),
    );
    // Heavy tasks beside the consumer's chain, which the pool spreads.
    for k in [4, 5, 6] {
        ed.insert(NodeKey(k), "ramps", Unforkable(RampEvery));
    }
    {
        let t = &mut ed.spec_mut().topology;
        t.edges
            .insert(at(2, 0), vec![Edge::Direct(PortSource::Global(0))]);
        t.edges.insert(at(1, 0), vec![from(2, 0)]);
        t.set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(2),
            port: 0,
        })]);
    }
    ed.commit().expect("commits");
    apply(&mut exec);
    let consumer = |port| EventInputPort {
        node: NodeKey(1),
        port,
    };
    let id = tutti_units::ParamId::new(1);
    for k in 0..200u64 {
        let kind: ParamKind = if k % 2 == 0 {
            ParamKind::Ramp(ParamRamp::by_id(id, 0.5, Samples::new(16)))
        } else {
            ParamKind::Segment(ParamSegment::by_id(id, 0.0, 1.0, Samples::new(50)))
        };
        ed.schedule(
            At::Frame(Frame::new(2_000 + k * 700)),
            consumer((k % 2) as u16),
            kind,
        )
        .expect("under capacity");
    }
    let input: Vec<f32> = (0..256).map(|i| (i % 13) as f32 / 13.0).collect();
    let mut audio = vec![0.0f32; 256];
    let transport = TransportPosition::default();
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    for &n in &sizes {
        exec.process(n, &transport, &[&input[..n]], &mut [&mut audio[..n]]);
    }
    // After the warm-up, so frame 0 is past: it lands late, inside the gate.
    ed.schedule(
        At::Frame(Frame::new(0)),
        consumer(0),
        ParamKind::Ramp(ParamRamp::by_id(id, 0.25, Samples::new(0))),
    )
    .expect("under capacity");
    ed.schedule(
        At::Beat(Beat::new(4.0)),
        consumer(1),
        ParamKind::Ramp(ParamRamp::by_id(id, 0.25, Samples::new(0))),
    )
    .expect("under capacity");
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for i in 0..2_000 {
        let n = sizes[i % sizes.len()];
        exec.process(n, &transport, &[&input[..n]], &mut [&mut audio[..n]]);
    }
    let landed: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    assert_eq!(
        landed, 201,
        "200 on time and the late one reached the consumer"
    );
    assert_eq!(exec.late_command_count(), 1);
    assert_eq!(exec.dropped_event_count(), 0);
    assert_eq!(ed.commands_outstanding(), 1, "the beat still waits");
}

#[test]
fn scheduled_params_are_allocation_free() {
    scheduled_params_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants.
#[test]
fn parallel_scheduled_params_are_allocation_free() {
    scheduled_params_are_allocation_free_on(4);
}

#[test]
fn param_events_are_allocation_free() {
    param_events_are_allocation_free_on(1);
}

/// The same on the parallel executor, four participants.
#[test]
fn parallel_param_events_are_allocation_free() {
    param_events_are_allocation_free_on(4);
}

/// Declares `Azimuth` (base 0) and writes what it reads.
struct AzimuthSink;

impl Node for AzimuthSink {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_params(&[tutti_units::UnitParam::Azimuth])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            io.output(0)[i] = io.param(0).frames().map_or(0.0, |v| v[i]);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        (port == 0).then_some(0.0)
    }
}

/// An `Azimuth` segment every 29 frames across the seam, both ways, over
/// every shape class.
struct SeamEvery;

impl Node for SeamEvery {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_param_events()
            .with_event_capacity(tutti_graph::testing::cap_of(16))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for i in 0..io.frames() {
            let f = start + i as u64;
            if f.is_multiple_of(29) {
                let o = io.offset(i).expect("inside");
                let (from, to) = if (f / 29).is_multiple_of(2) {
                    (170.0, -170.0)
                } else {
                    (-175.0, 175.0)
                };
                let shape = [Law::Linear, Law::PowerIn(Exponent::of(2.0)), Law::Hold]
                    [(f / 87) as usize % 3];
                let seg =
                    ParamSegment::unit(tutti_units::UnitParam::Azimuth, from, to, Samples::new(40))
                        .law(shape);
                let _ = io
                    .event_out(0)
                    .push_param(tutti_graph::ParamEvent::segment(o, seg));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A bearing's port, wrapping and short-arc interpolating, with its range
/// moved between blocks (an arc across ±180, the whole circle, unbounded),
/// allocates nothing.
#[test]
fn an_azimuth_param_port_is_allocation_free() {
    use tutti_graph::{ParamIn, ParamRange};
    use tutti_units::{ArcDegrees, Azimuth};
    let (mut ed, mut exec) = Editor::new(prepare(256));
    ed.insert(NodeKey(1), "sink", Unforkable(AzimuthSink));
    ed.insert(NodeKey(4), "segs", Unforkable(SeamEvery));
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    let azimuth = ParamIn {
        node: NodeKey(1),
        param: tutti_units::UnitParam::Azimuth.into(),
    };
    ed.spec_mut().connect_param(
        azimuth,
        EventOutputPort {
            node: NodeKey(4),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let transport = TransportPosition::default();
    let mut out = vec![0.0f32; 256];
    let ranges = [
        ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(30.0)),
        ParamRange::new(-180.0, 180.0),
        ParamRange::UNBOUNDED,
    ];
    let sizes = [256usize, 1, 7, 64, 100, 255, 33];
    for i in 0..60 {
        ed.set_param_range(azimuth, ranges[i % ranges.len()])
            .expect("a bearing's range");
        for &n in &sizes {
            exec.process(n, &transport, &[], &mut [&mut out[..n]]);
        }
    }
    assert!(
        out.iter().all(|v| (-180.0..180.0).contains(v)),
        "the bearings are wrapped"
    );
}

/// A range published by another thread while blocks render allocates and
/// frees nothing on the audio thread: the control thread replaces a port's
/// range as fast as it can (each publish allocates and retires on its own
/// thread), every block lends once inside the gate, and the retired ranges
/// are freed by the control side's `reclaim`, none left once the blocks stop.
#[test]
fn a_range_published_while_blocks_render_is_allocation_free() {
    use std::sync::atomic::AtomicBool;
    use tutti_graph::{ParamIn, ParamRange};
    use tutti_units::{ArcDegrees, Azimuth};
    let (mut ed, mut exec) = Editor::new(prepare(256));
    ed.insert(NodeKey(1), "sink", Unforkable(AzimuthSink));
    ed.insert(NodeKey(4), "segs", Unforkable(SeamEvery));
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    let azimuth = ParamIn {
        node: NodeKey(1),
        param: tutti_units::UnitParam::Azimuth.into(),
    };
    ed.spec_mut().connect_param(
        azimuth,
        EventOutputPort {
            node: NodeKey(4),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let transport = TransportPosition::default();
    let mut out = vec![0.0f32; 256];
    let stop = AtomicBool::new(false);
    let ed = std::thread::scope(|scope| {
        let control = scope.spawn(|| {
            let mut ed = ed;
            let mut k = 0u32;
            while !stop.load(Ordering::Acquire) {
                let half = 10.0 + f64::from(k % 160);
                ed.set_param_range(
                    azimuth,
                    ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(half)),
                )
                .expect("a bearing's range");
                if k.is_multiple_of(8) {
                    let _ = ed.reclaim();
                }
                k += 1;
            }
            ed
        });
        for _ in 0..4_000 {
            exec.process(64, &transport, &[], &mut [&mut out[..64]]);
        }
        stop.store(true, Ordering::Release);
        control.join().expect("the control thread ran")
    });
    assert_eq!(ed.reclaim(), 0, "no block is left holding a retired range");
}

/// Removing a node whose range was moved frees nothing on the audio thread:
/// the unit holds a reader that keeps its range cell alive, and the commit
/// that drops the node hands the unit back, so the cell and its range are
/// freed by the control side's `collect`, never inside a block.
#[test]
fn removing_a_node_with_a_moved_range_frees_nothing_on_the_audio_thread() {
    use tutti_graph::{ParamIn, ParamRange};
    use tutti_units::{ArcDegrees, Azimuth};
    let (mut ed, mut exec) = Editor::new(prepare(256));
    ed.insert(NodeKey(1), "sink", Unforkable(AzimuthSink));
    ed.insert(NodeKey(4), "segs", Unforkable(SeamEvery));
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(1),
            port: 0,
        })]);
    let azimuth = ParamIn {
        node: NodeKey(1),
        param: tutti_units::UnitParam::Azimuth.into(),
    };
    ed.spec_mut().connect_param(
        azimuth,
        EventOutputPort {
            node: NodeKey(4),
            port: 0,
        },
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let transport = TransportPosition::default();
    let mut out = vec![0.0f32; 256];
    for half in [30.0, 60.0, 90.0] {
        ed.set_param_range(
            azimuth,
            ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(half)),
        )
        .expect("a bearing's range");
        exec.process(64, &transport, &[], &mut [&mut out[..64]]);
    }
    ed.remove(NodeKey(1));
    ed.remove(NodeKey(4));
    ed.spec_mut().topology.set_outputs(Vec::new());
    ed.commit().expect("commits");
    for _ in 0..8 {
        exec.process(64, &transport, &[], &mut [&mut out[..64]]);
    }
    let mut gone = ed.collect();
    gone.sort_unstable_by_key(|k| k.0);
    assert_eq!(gone, [NodeKey(1), NodeKey(4)], "both units came back");
}

/// `contract::Direct`, the by-hand driver an instrument's allocation gate
/// runs its node through, allocates nothing per block while it plays a full
/// block of events into a node's event input, and while it collects a
/// node's event output (both lists are reserved at `new`), under a rolling
/// transport.
#[test]
fn direct_driving_events_does_not_allocate() {
    use tutti_graph::contract::{Direct, Emitter, ImpulseSource};
    use tutti_graph::{Event, Offset};
    use tutti_units::{Bpm, SampleRate};

    let rate = SampleRate::SR_48K;
    let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
    let mut pulse = Direct::new(ImpulseSource::new(Latency::ZERO), rate, 64);
    pulse.set_transport(rolling);
    let events: Vec<Event> = (0..64)
        .map(|k| {
            Event::midi(
                Offset::of(Samples::new(k), Samples::new(64)),
                Message::midi1(
                    MidiChannel::FIRST,
                    Voice1::note_on(NoteNumber::of(60), U7::of(100)),
                ),
            )
        })
        .collect();
    let note = EventKind::Midi(midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100)));
    let mut emitter = Direct::new(Emitter::new(Frame::new(70), note), rate, 64);
    emitter.set_transport(rolling);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            pulse.events(0, &events);
            pulse.block();
            emitter.block();
        }
    });
    assert!(
        pulse.output().plane(0).iter().any(|&s| s != 0.0),
        "the events played"
    );
}

/// Reporting a tail and parking on it never allocates: pulses into ringers
/// that narrow their ceiling every block, through the serial executor and a
/// pool whose every participant runs inside the gate.
fn reporting_nodes_are_allocation_free_on(workers: usize) {
    use common::{Burst, RingingNode};
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    let mut outputs = Vec::new();
    for i in 0..4u64 {
        let (src, fx) = (NodeKey(10 + i), NodeKey(20 + i));
        ed.insert(src, "pulse", Unforkable(Burst::new(100 + 40 * i as usize)));
        ed.insert(
            fx,
            "ringer",
            Unforkable(RingingNode::new(
                Tail::Finite(Samples::new(4_000)),
                200,
                200,
            )),
        );
        ed.spec_mut()
            .topology
            .edges
            .insert(at(fx.0, 0), vec![from(src.0, 0)]);
        outputs.push(PortSource::Node(OutputPort { node: fx, port: 0 }));
    }
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    let [mut a, mut b, mut c, mut d] = std::array::from_fn(|_| vec![0.0f32; 64]);
    let mut heard = false;
    for _ in 0..200 {
        exec.process(
            64,
            &TransportPosition::default(),
            &[],
            &mut [&mut a[..], &mut b[..], &mut c[..], &mut d[..]],
        );
        heard |= [&a, &b, &c, &d].iter().any(|o| o.iter().any(|&x| x != 0.0));
    }
    assert!(heard, "the gate rendered signal");
}

#[test]
fn reporting_nodes_are_allocation_free() {
    reporting_nodes_are_allocation_free_on(1);
}

#[test]
fn parallel_reporting_nodes_are_allocation_free() {
    reporting_nodes_are_allocation_free_on(4);
}

/// Audio fan-in never allocates: `Op::Sum` over 8, 64 and 100 sources (one
/// op, one full op, and a chain of two), at a node's port and at a global
/// output channel, with every source live, every source silent, and sources
/// that fall silent one by one (a pulse each), so the silent-addend skip, the
/// all-silent fill and the in-place form all run inside the gate, on the
/// parallel executor's helpers too.
fn fan_in_is_allocation_free_on(workers: usize) {
    use common::Burst;
    for n in [8u64, 64, 100] {
        for source in ["live", "silent", "pulses"] {
            for at_port in [false, true] {
                let (mut ed, mut exec) = Editor::new(prepare(256));
                use_pool(&mut exec, workers);
                for k in 1..=n {
                    let name = "src";
                    match source {
                        "live" => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(TestNode::new(Kind::Const {
                                value: 0.01 * k as f32,
                                width: 1,
                            })),
                        ),
                        "silent" => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(TestNode::new(Kind::Const {
                                value: 0.0,
                                width: 1,
                            })),
                        ),
                        _ => ed.insert(
                            NodeKey(k),
                            name,
                            Unforkable(Burst::new(300 + 17 * k as usize)),
                        ),
                    };
                }
                let sink = NodeKey(n + 1);
                if at_port {
                    ed.insert(
                        sink,
                        "thru",
                        Unforkable(TestNode::new(Kind::Thru { width: 1 })),
                    );
                    for k in 1..=n {
                        ed.spec_mut().topology.connect(at(sink.0, 0), from(k, 0));
                    }
                    ed.spec_mut()
                        .topology
                        .set_outputs(vec![PortSource::Node(OutputPort {
                            node: sink,
                            port: 0,
                        })]);
                } else {
                    ed.spec_mut().topology.outputs = vec![
                        (1..=n)
                            .map(|k| {
                                PortSource::Node(OutputPort {
                                    node: NodeKey(k),
                                    port: 0,
                                })
                            })
                            .collect(),
                    ];
                }
                ed.commit().expect("commits");
                apply(&mut exec);
                ed.collect();
                assert_eq!(exec.is_parallel(), workers > 1);
                let mut out = vec![0.0f32; 128];
                let mut heard = false;
                for _ in 0..40 {
                    exec.process(128, &TransportPosition::default(), &[], &mut [&mut out[..]]);
                    heard |= out.iter().any(|&x| x != 0.0);
                }
                assert_eq!(
                    heard,
                    source != "silent",
                    "n = {n}, {source}, port {at_port}"
                );
            }
        }
    }
}

#[test]
fn fan_in_is_allocation_free() {
    fan_in_is_allocation_free_on(1);
}

#[test]
fn parallel_fan_in_is_allocation_free() {
    fan_in_is_allocation_free_on(4);
}

/// A weighted fan-in never allocates, ramping: 64 lanes (one full op) and 100
/// (a chain of two), at a node's port and at a global output channel, every
/// lane weighted, every one moved before every block (so each block ramps
/// every lane), with live and pulsing sources (the silent-addend skip runs
/// with weights), on the parallel executor's helpers too. The moves are the
/// control thread's; every block renders inside the gate.
///
/// With `driven`, every lane is also driven by one of four lane nodes (two of
/// them declaring latency, so the lanes behind them get event rings): each
/// lane runs the param port's step, with its clamp and its values, per block.
fn weighted_fan_in_is_allocation_free_on(workers: usize, driven: bool) {
    use common::Burst;
    use tutti_graph::{EdgeGain, EdgeId, PortSink};
    use tutti_units::Amplitude;
    for n in [64u64, 100] {
        for at_port in [false, true] {
            let (mut ed, mut exec) = Editor::new(prepare(256));
            use_pool(&mut exec, workers);
            for k in 1..=n {
                if k % 2 == 0 {
                    ed.insert(
                        NodeKey(k),
                        "src",
                        Unforkable(TestNode::new(Kind::Const {
                            value: 0.01 * k as f32,
                            width: 1,
                        })),
                    );
                } else {
                    ed.insert(
                        NodeKey(k),
                        "pulse",
                        Unforkable(Burst::new(300 + 17 * k as usize)),
                    );
                }
            }
            let sink = NodeKey(n + 1);
            let edge_of = |k: u64| EdgeId {
                sink: if at_port {
                    PortSink::Port(at(sink.0, 0))
                } else {
                    PortSink::Output(0)
                },
                from: from(k, 0),
            };
            if at_port {
                ed.insert(
                    sink,
                    "thru",
                    Unforkable(TestNode::new(Kind::Thru { width: 1 })),
                );
                for k in 1..=n {
                    ed.spec_mut().topology.connect(at(sink.0, 0), from(k, 0));
                }
                ed.spec_mut()
                    .topology
                    .set_outputs(vec![PortSource::Node(OutputPort {
                        node: sink,
                        port: 0,
                    })]);
            } else {
                ed.spec_mut().topology.outputs = vec![
                    (1..=n)
                        .map(|k| {
                            PortSource::Node(OutputPort {
                                node: NodeKey(k),
                                port: 0,
                            })
                        })
                        .collect(),
                ];
            }
            for k in 1..=n {
                ed.spec_mut()
                    .set_edge_gain(edge_of(k), EdgeGain::new(Amplitude::new(0.5)));
            }
            if driven {
                for (i, latency) in [0usize, 0, 40, 150].into_iter().enumerate() {
                    ed.insert(
                        NodeKey(5000 + i as u64),
                        "lane",
                        Unforkable(TestNode::new(Kind::GainDriver {
                            period: 8 + 9 * i as u64,
                            latency,
                        })),
                    );
                }
                for k in 1..=n {
                    ed.spec_mut().connect_edge_gain(
                        edge_of(k),
                        EventOutputPort {
                            node: NodeKey(5000 + k % 4),
                            port: 0,
                        },
                    );
                }
            }
            ed.commit().expect("commits");
            apply(&mut exec);
            ed.collect();
            assert_eq!(exec.is_parallel(), workers > 1);
            let mut out = vec![0.0f32; 128];
            let mut heard = false;
            for block in 0..40u64 {
                for k in 1..=n {
                    let w = ((block + k) % 7) as f32 * 0.25 - 0.5;
                    ed.set_edge_gain(edge_of(k), Amplitude::new(f64::from(w)))
                        .expect("an edge with an entry");
                }
                exec.process(128, &TransportPosition::default(), &[], &mut [&mut out[..]]);
                heard |= out.iter().any(|&x| x != 0.0);
            }
            assert!(heard, "n = {n}, port {at_port}");
        }
    }
}

#[test]
fn weighted_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(1, false);
}

#[test]
fn parallel_weighted_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(4, false);
}

#[test]
fn driven_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(1, true);
}

#[test]
fn parallel_driven_fan_in_is_allocation_free() {
    weighted_fan_in_is_allocation_free_on(4, true);
}

/// A node of any audio width, declared latency and event ports that does
/// nothing allocating, `reset` included: audio is half the input (`0.25`
/// past the inputs), events are forwarded (or, with `tick`, made every
/// `period` frames).
struct Wire {
    ins: usize,
    outs: usize,
    latency: usize,
    ein: u16,
    eout: u16,
    tick: Option<(u64, u64)>,
    state: f32,
}

impl Wire {
    fn new(ins: usize, outs: usize, latency: usize) -> Self {
        Self {
            ins,
            outs,
            latency,
            ein: 0,
            eout: 0,
            tick: None,
            state: 0.0,
        }
    }

    fn events(mut self, ein: u16, eout: u16) -> Self {
        (self.ein, self.eout) = (ein, eout);
        self
    }

    fn ticks(period: u64, phase: u64) -> Self {
        let mut w = Self::new(0, 0, 0).events(0, 1);
        w.tick = Some((period, phase));
        w
    }
}

impl Node for Wire {
    fn shape(&self) -> Shape {
        let ch = |n| ChannelLayout::try_from(n).expect("a width");
        // The first `outs` inputs are its main bus, which its bypass and mix
        // pass by index; a wider input's rest are ports.
        let main = ch(self.ins.min(self.outs));
        Shape::buses(Buses::unassigned(main), Buses::unassigned(main))
            .with_ports(ch(self.ins), ch(self.outs))
            .with_latency(Latency::new(Samples::new(self.latency)))
            .with_events(self.ein, self.eout)
            .with_tail(Tail::None)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some((period, phase)) = self.tick {
            for at in cx.env.offsets() {
                let f = cx.env.frame_at(at).get() + phase;
                if f.is_multiple_of(period) {
                    // A note-on, never ended: keys are held on every port the
                    // events reach, for a bypass to close.
                    let note = 60 + (f / period) % 4;
                    let on = Ump([0x2090_0064 | ((note as u32) << 8), 0, 0, 0]);
                    let _ = io.event_out(0).push(tutti_graph::Event::midi(at, on));
                }
            }
        }
        if self.ein > 0 {
            for q in 0..self.eout as usize {
                for &e in io.events(q % self.ein as usize) {
                    let _ = io.event_out(q).push(e);
                }
            }
        }
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for c in 0..self.outs {
            for i in 0..n {
                outs.get(c)[i] = if c < self.ins {
                    0.5 * ins.get(c)[i] + self.state
                } else {
                    0.25
                };
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.state = 0.0;
    }
}

/// The graph the mode and mix gates run: latent nodes (lines to feed and to
/// run), event nodes (held-key tables to observe and to close), a node with
/// no audio input, and sums, committed and running on `workers` participants.
fn mode_graph(workers: usize) -> (Editor, tutti_graph::Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let node = |ed: &mut Editor, k: u64, n: Wire| {
        ed.insert(NodeKey(k), "node", Unforkable(n));
    };
    // (The test nodes of `common` allocate in `reset`, which this gate runs
    // on every un-bypass: a node that allocates is the node's bug.)
    node(&mut ed, 1, Wire::ticks(5, 0));
    node(&mut ed, 2, Wire::ticks(7, 3));
    node(&mut ed, 3, Wire::new(0, 0, 11).events(1, 1));
    node(&mut ed, 4, Wire::new(0, 1, 0).events(1, 0));
    node(&mut ed, 5, Wire::new(1, 1, 40));
    node(&mut ed, 6, Wire::new(1, 1, 0));
    node(&mut ed, 7, Wire::new(1, 1, 150));
    node(&mut ed, 8, Wire::new(2, 1, 0));
    node(&mut ed, 9, Wire::new(1, 1, 0));
    let ev = |node, port| EventOutputPort {
        node: NodeKey(node),
        port,
    };
    let spec = ed.spec_mut();
    spec.events.insert(
        EventInputPort {
            node: NodeKey(3),
            port: 0,
        },
        vec![EventEdge::Direct(ev(1, 0)), EventEdge::Direct(ev(2, 0))],
    );
    spec.events.insert(
        EventInputPort {
            node: NodeKey(4),
            port: 0,
        },
        vec![EventEdge::Direct(ev(3, 0))],
    );
    let t = &mut spec.topology;
    t.connect(at(5, 0), Edge::Direct(PortSource::Global(0)));
    t.connect(at(6, 0), from(5, 0));
    t.connect(at(7, 0), from(6, 0));
    t.connect(at(8, 0), from(4, 0));
    t.connect(at(8, 1), from(7, 0));
    t.connect(at(9, 0), from(8, 0));
    t.set_outputs(vec![PortSource::Node(OutputPort {
        node: NodeKey(9),
        port: 0,
    })]);
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
    (ed, exec)
}

/// Toggling modes and rendering under `RtScope`, on `workers`
/// participants: every node of a
/// graph with latent nodes (lines to feed and to run), event nodes (held-key
/// tables to observe and to close), a node with no audio input, and sums, has
/// its mode moved every few blocks, so every block runs some node in a fade,
/// parked, un-bypassing (reset, warm-up) or muted. Blocks of four lengths.
fn toggling_modes_are_allocation_free_on(workers: usize) {
    use tutti_graph::NodeMode;
    let (mut ed, mut exec) = mode_graph(workers);
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mut heard = false;
    for block in 0..120usize {
        for k in 1..=9u64 {
            // Each node on its own rhythm, so fades overlap and restart.
            let mode = match (block / (1 + k as usize % 5) + k as usize) % 3 {
                0 => NodeMode::Active,
                1 => NodeMode::Bypassed,
                _ => NodeMode::Muted,
            };
            ed.set_mode(NodeKey(k), mode).expect("a node");
        }
        let n = lens[block % lens.len()];
        exec.process(
            n,
            &TransportPosition::default(),
            &[&input[..n]],
            &mut [&mut out[..n]],
        );
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn toggling_modes_are_allocation_free() {
    toggling_modes_are_allocation_free_on(1);
}

#[test]
fn parallel_toggling_modes_are_allocation_free() {
    toggling_modes_are_allocation_free_on(4);
}

/// A node that declares its own bypass is called in `Bypassed`, and toggling
/// its mode and rendering under `RtScope` allocates nothing: the
/// executor neither blends nor parks it, so no fade, reset or park runs. Its
/// calls are counted: one per block, bypassed or not.
fn own_bypass_toggling_is_allocation_free_on(workers: usize) {
    use tutti_graph::NodeMode;
    use tutti_graph::Unforkable;
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let kind = Kind::OwnBypass {
        inner: Box::new(Kind::Gain {
            gain: 2.0,
            width: 1,
        }),
    };
    ed.insert(
        NodeKey(1),
        "own",
        Unforkable(TestNode::counted(kind, Arc::clone(&calls))),
    );
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(1);
    let t = &mut ed.spec_mut().topology;
    t.connect(at(1, 0), Edge::Direct(PortSource::Global(0)));
    t.set_outputs(vec![PortSource::Node(OutputPort {
        node: NodeKey(1),
        port: 0,
    })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let input = vec![0.5f32; 256];
    let mut out = vec![0.0f32; 256];
    for block in 0..60usize {
        let mode = [NodeMode::Active, NodeMode::Bypassed][(block / 3) % 2];
        ed.set_mode(NodeKey(1), mode).expect("a node");
        exec.process(
            64,
            &TransportPosition::default(),
            &[&input[..64]],
            &mut [&mut out[..64]],
        );
        assert!(
            out[..64].iter().all(|&x| x == 1.0),
            "block {block} in {mode:?}: the node's own output, never blended"
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 60, "called every block");
}

#[test]
fn own_bypass_toggling_is_allocation_free() {
    own_bypass_toggling_is_allocation_free_on(1);
}

#[test]
fn parallel_own_bypass_toggling_is_allocation_free() {
    own_bypass_toggling_is_allocation_free_on(4);
}

/// Moving mixes and rendering under `RtScope`, on `workers`
/// participants: the graph of
/// the mode gate with every node's mix moved every few blocks (the ends and
/// between, so a node runs steady WET, steady DRY, a steady blend and a ramp)
/// and a third of the nodes' modes moved with them, so a mix also meets a
/// fade, a warm-up and a parked node. Blocks of four lengths.
fn moving_mixes_are_allocation_free_on(workers: usize) {
    use tutti_graph::NodeMode;
    use tutti_units::Mix;
    let (mut ed, mut exec) = mode_graph(workers);
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mixes = [1.0f64, 0.0, 0.5, 0.25, 1.0, 0.75, 0.0, 1.0];
    let mut heard = false;
    for block in 0..120usize {
        for k in 1..=9u64 {
            let at = block / (1 + k as usize % 3) + k as usize;
            ed.set_mix(NodeKey(k), Mix::new(mixes[at % mixes.len()]))
                .expect("a node");
            if k % 3 == 0 {
                let mode = match (block / 7 + k as usize) % 3 {
                    0 => NodeMode::Active,
                    1 => NodeMode::Bypassed,
                    _ => NodeMode::Muted,
                };
                ed.set_mode(NodeKey(k), mode).expect("a node");
            }
        }
        let n = lens[block % lens.len()];
        exec.process(
            n,
            &TransportPosition::default(),
            &[&input[..n]],
            &mut [&mut out[..n]],
        );
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn moving_mixes_are_allocation_free() {
    moving_mixes_are_allocation_free_on(1);
}

#[test]
fn parallel_moving_mixes_are_allocation_free() {
    moving_mixes_are_allocation_free_on(4);
}

/// Driven mixes and rendering under `RtScope`, on `workers`
/// participants: the graph
/// of the mode gate, with a commit that then drives the mix of five of its
/// kept nodes from lanes of four periods and latencies (two of them late
/// enough to give a node an event ring), and a mix, a range and a third of the
/// modes moved every few blocks, so a lane meets a steady blend, a fade, a
/// warm-up, a parked node and a lane that has just been let go (a source
/// disconnected halfway, its fade running out). Blocks of four lengths.
fn driven_mixes_are_allocation_free_on(workers: usize) {
    use tutti_graph::{NodeMix, NodeMode, ParamIn, ParamRange};
    use tutti_units::Mix;
    let (mut ed, mut exec) = mode_graph(workers);
    for (i, latency) in [0usize, 0, 40, 150].into_iter().enumerate() {
        ed.insert(
            NodeKey(5000 + i as u64),
            "lane",
            Unforkable(TestNode::new(Kind::MixDriver {
                period: 8 + 9 * i as u64,
                latency,
            })),
        );
    }
    let port = |k: u64| ParamIn {
        node: NodeKey(k),
        param: NodeMix::ADDR,
    };
    let lane = |k: u64| EventOutputPort {
        node: NodeKey(5000 + k % 4),
        port: 0,
    };
    for k in 5..=9u64 {
        ed.spec_mut().connect_param(port(k), lane(k));
    }
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let input: Vec<f32> = (0..256).map(|i| ((i % 19) as f32 - 9.0) * 0.1).collect();
    let mut out = vec![0.0f32; 256];
    let lens = [64usize, 100, 256, 7];
    let mixes = [1.0f64, 0.0, 0.5, 0.25, 1.0, 0.75, 0.0, 1.0];
    let ranges = [(0.0f64, 1.0f64), (0.25, 0.75), (-2.0, 2.0)];
    let mut heard = false;
    for block in 0..120usize {
        if block == 60 {
            // Let go of two sources: their lanes fade to the base.
            for k in [6u64, 8] {
                ed.spec_mut().disconnect_param(port(k), lane(k));
            }
            ed.commit().expect("commits");
            apply(&mut exec);
            ed.collect();
        }
        for k in 1..=9u64 {
            let at = block / (1 + k as usize % 3) + k as usize;
            ed.set_mix(NodeKey(k), Mix::new(mixes[at % mixes.len()]))
                .expect("a node");
            if k >= 5 && block % 11 == k as usize {
                let (min, max) = ranges[(block / 11 + k as usize) % ranges.len()];
                ed.set_param_range(port(k), ParamRange::new(min, max))
                    .expect("a node");
            }
            if k % 3 == 0 {
                let mode = match (block / 7 + k as usize) % 3 {
                    0 => NodeMode::Active,
                    1 => NodeMode::Bypassed,
                    _ => NodeMode::Muted,
                };
                ed.set_mode(NodeKey(k), mode).expect("a node");
            }
        }
        let n = lens[block % lens.len()];
        exec.process(
            n,
            &TransportPosition::default(),
            &[&input[..n]],
            &mut [&mut out[..n]],
        );
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}

#[test]
fn driven_mixes_are_allocation_free() {
    driven_mixes_are_allocation_free_on(1);
}

#[test]
fn parallel_driven_mixes_are_allocation_free() {
    driven_mixes_are_allocation_free_on(4);
}

/// Edge fades under `RtScope`, on `workers`
/// participants: eight constants summed at the output, each edge fading in or
/// out over several blocks of different lengths, the ended fades reported on
/// the ring (a push of a plain value), then a block rendered after the editor
/// settled them (outside the gate: settling edits the spec) and recommitted.
fn fading_edges_are_allocation_free_on(workers: usize) {
    use tutti_graph::{EdgeFade, EdgeGain, EdgeId, PortSink};
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    for k in 1..=8u64 {
        ed.insert(
            NodeKey(k),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: k as f32,
                width: 1,
            })),
        );
    }
    let edge = |k: u64| EdgeId {
        sink: PortSink::Output(0),
        from: from(k, 0),
    };
    ed.spec_mut().topology.outputs = vec![
        (1..=8u64)
            .map(|k| {
                PortSource::Node(OutputPort {
                    node: NodeKey(k),
                    port: 0,
                })
            })
            .collect(),
    ];
    for k in 1..=8u64 {
        ed.spec_mut()
            .set_edge_gain(edge(k), EdgeGain::new(tutti_units::Amplitude::new(0.5)));
    }
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    for k in 1..=8u64 {
        let fade = if k % 2 == 0 {
            EdgeFade::In
        } else {
            EdgeFade::Out
        };
        ed.set_edge_fade(edge(k), fade).expect("edge");
    }
    // Weights that move under the fades, set between blocks.
    let lens = [64usize, 17, 64, 5, 33];
    let mut out = vec![0.0f32; 64];
    let mut heard = false;
    for block in 0..14 {
        if block % 3 == 0 {
            for k in 1..=8u64 {
                let w = 0.25 * ((block + k as usize) % 5) as f32;
                ed.set_edge_gain(edge(k), tutti_units::Amplitude::new(f64::from(w)))
                    .expect("edge");
            }
        }
        let n = lens[block % lens.len()];
        exec.process(n, &TransportPosition::default(), &[], &mut [&mut out[..n]]);
        heard |= out[..n].iter().any(|&x| x != 0.0);
    }
    assert!(heard);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    ed.collect();
    assert!(ed.finish_fades(), "the ended fades were reported");
}

#[test]
fn fading_edges_are_allocation_free() {
    fading_edges_are_allocation_free_on(1);
}

#[test]
fn parallel_fading_edges_are_allocation_free() {
    fading_edges_are_allocation_free_on(4);
}

/// A node with state to hand over: it writes `STATE` messages when replaced
/// (inside the gate, which the executor's chase buffer must satisfy) and
/// counts every event it is sent.
struct Chaser {
    seen: Arc<AtomicU64>,
}

const STATE: u8 = 4;

impl Node for Chaser {
    fn shape(&self) -> Shape {
        Shape::buses(Buses::NONE, Buses::unassigned(ChannelLayout::MONO))
            .with_events(1, 0)
            .with_chase(u32::from(STATE))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.seen
            .fetch_add(io.events(0).len() as u64, Ordering::Relaxed);
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn chase(&self, out: &mut tutti_graph::Chase<'_>) {
        for i in 0..STATE {
            let controller = tutti_ump::CCNumber::of(i);
            assert!(out.push(midi::m1_cc(MidiChannel::FIRST, controller, U7::of(1))));
        }
    }
}

/// A swap's block never allocates: the replaced node writes its state into
/// the executor's buffer inside the gate, and the block that delivers it to
/// the successor (and every block after) runs inside it. Applying the commit
/// itself allocates, as it always does, and runs outside.
fn a_swap_is_allocation_free_on(workers: usize) {
    const NODES: u64 = 4;
    let (mut ed, mut exec) = Editor::new(prepare(256));
    use_pool(&mut exec, workers);
    let counts = |_| (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let seen: Vec<_> = (0..NODES).map(counts).collect();
    for (k, (first, _)) in (1..).zip(&seen) {
        ed.insert(
            NodeKey(k),
            "chaser",
            Unforkable(Chaser {
                seen: Arc::clone(first),
            }),
        );
    }
    ed.spec_mut().topology.set_outputs((1..=NODES).map(|k| {
        PortSource::Node(OutputPort {
            node: NodeKey(k),
            port: 0,
        })
    }));
    ed.commit().expect("commits");
    let transport = TransportPosition::default();
    let mut outs: [Vec<f32>; NODES as usize] = std::array::from_fn(|_| vec![0.0f32; 256]);
    let mut block = |exec: &mut tutti_graph::Executor, n: usize| {
        let [a, b, c, d] = &mut outs;
        exec.process(
            n,
            &transport,
            &[],
            &mut [&mut a[..n], &mut b[..n], &mut c[..n], &mut d[..n]],
        );
    };
    block(&mut exec, 256);
    for (k, (_, second)) in (1..).zip(&seen) {
        ed.replace(
            NodeKey(k),
            Unforkable(Chaser {
                seen: Arc::clone(second),
            }),
            Fade::new(Samples::new(0)).law(FadeLaw::EqualPower),
        )
        .expect("fits");
    }
    ed.commit().expect("commits");
    apply(&mut exec);
    assert_eq!(
        exec.is_parallel(),
        workers > 1,
        "the gate runs the mode it names"
    );
    for n in [256usize, 1, 7, 64] {
        block(&mut exec, n);
    }
    for (first, second) in &seen {
        assert_eq!(second.load(Ordering::Relaxed), u64::from(STATE));
        assert_eq!(first.load(Ordering::Relaxed), 0);
    }
    assert_eq!(exec.dropped_event_count(), 0);
}

#[test]
fn a_swap_is_allocation_free() {
    a_swap_is_allocation_free_on(1);
}

#[test]
fn parallel_swap_is_allocation_free() {
    a_swap_is_allocation_free_on(4);
}

/// An edge fading in, out and back in, and a weight ramping beside it, render
/// under `RtScope`: the fade kernel (`Weight::Fade`) and the ramp run
/// on every block, across commits (which allocate, outside the gate).
#[test]
fn edge_fades_are_allocation_free() {
    use tutti_graph::{EdgeFade, EdgeGain, EdgeId, PortSink};
    use tutti_units::Amplitude;
    const BLOCK: usize = 64;
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    for n in 1..=2u64 {
        let k = Kind::Const {
            value: n as f32,
            width: 1,
        };
        ed.insert(NodeKey(n), "const", Unforkable(TestNode::new(k)));
    }
    let src = |n: u64| {
        PortSource::Node(OutputPort {
            node: NodeKey(n),
            port: 0,
        })
    };
    ed.spec_mut().topology.outputs = vec![vec![src(1), src(2)]];
    let id = |n: u64| EdgeId {
        sink: PortSink::Output(0),
        from: Edge::Direct(src(n)),
    };
    ed.spec_mut()
        .set_edge_gain(id(1), EdgeGain::new(Amplitude::new(0.5)));
    ed.spec_mut()
        .set_edge_gain(id(2), EdgeGain::new(Amplitude::new(0.25)));
    ed.set_edge_fade(id(1), EdgeFade::In).expect("the edge");
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let mut out = vec![0.0f32; BLOCK];
    let mut heard = false;
    for block in 0..40usize {
        match block % 10 {
            3 => ed.set_edge_fade(id(1), EdgeFade::Out).expect("the edge"),
            6 => ed.set_edge_fade(id(1), EdgeFade::In).expect("the edge"),
            8 => ed
                .set_edge_gain(id(2), Amplitude::new(0.75))
                .expect("the edge"),
            _ => {}
        }
        exec.process(
            BLOCK,
            &TransportPosition::default(),
            &[],
            &mut [&mut out[..]],
        );
        heard |= out.iter().any(|&x| x != 0.0);
        ed.collect();
    }
    assert!(heard);
}

/// Adds a chain `lag -> sum <- smooth` fed by the global input: the sum's
/// second path is delayed by a PDC ring of the lag's latency. Keys start at
/// `base`; returns the sum's key.
fn pdc_pair(ed: &mut Editor, base: u64, latency: usize) -> NodeKey {
    let (lag, smooth, sum) = (NodeKey(base), NodeKey(base + 1), NodeKey(base + 2));
    ed.insert(lag, "lag", Unforkable(TestNode::new(Kind::Lag { latency })));
    ed.insert(smooth, "smooth", Unforkable(TestNode::new(Kind::Smooth)));
    ed.insert(
        sum,
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 2 })),
    );
    let t = &mut ed.spec_mut().topology;
    t.edges
        .insert(at(base, 0), vec![Edge::Direct(PortSource::Global(0))]);
    t.edges
        .insert(at(base + 1, 0), vec![Edge::Direct(PortSource::Global(0))]);
    t.edges.insert(at(base + 2, 0), vec![from(base, 0)]);
    t.edges.insert(at(base + 2, 1), vec![from(base + 1, 0)]);
    sum
}

/// Writes `keys` MIDI 1.0 note-ons, 400 a block, and holds them: key `k` is
/// note `k % 128` on channel `k / 128`.
struct Holder {
    keys: usize,
    sent: usize,
    per_block: usize,
}

impl Holder {
    fn new(keys: usize) -> Self {
        Self::paced(keys, 400)
    }

    /// `per_block` keys a block, the most its port takes.
    fn paced(keys: usize, per_block: usize) -> Self {
        Self {
            keys,
            sent: 0,
            per_block,
        }
    }
}

/// Counts the MIDI 1.0 note offs sent to its event inputs.
struct OffCount {
    offs: Arc<AtomicUsize>,
    inputs: u16,
}

impl Node for OffCount {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(self.inputs, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for port in 0..self.inputs {
            for e in io.events(usize::from(port)) {
                if let EventKind::Midi(Ump(w)) = e.kind
                    && w[0] >> 28 == 2
                    && (w[0] >> 20) & 0xf == 8
                {
                    self.offs.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

impl Node for Holder {
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(0, 1)
            .with_event_capacity(tutti_graph::testing::cap_of(self.per_block.max(1) as u32))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let next = (self.sent + self.per_block).min(self.keys);
        for k in self.sent..next {
            let channel = MidiChannel::of((k / 128) as u8);
            let on = midi::m1_note_on(channel, (k % 128) as u8, U7::of(100));
            let _ = io
                .event_out(0)
                .push(tutti_graph::Event::midi(tutti_graph::Offset::ZERO, on));
        }
        self.sent = next;
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn ev(node: u64, port: u16) -> EventOutputPort {
    EventOutputPort {
        node: NodeKey(node),
        port,
    }
}

fn sink(node: u64, port: u16) -> EventInputPort {
    EventInputPort {
        node: NodeKey(node),
        port,
    }
}

/// An event consumer `base + 3` with four inputs: a lagging emitter's, an
/// emitter's direct (which gets an event PDC FIFO), and two note holders'.
/// Keys start at `base` (emitters at `base`, `base + 1`; the lag at
/// `base + 2`; holders at `base + 4`, `base + 5`).
fn event_pdc(ed: &mut Editor, base: u64) {
    let nodes = [
        Kind::Emitter {
            period: 5,
            phase: 0,
        },
        Kind::Emitter {
            period: 7,
            phase: 3,
        },
        Kind::EventLag { latency: 11 },
        Kind::Consumer { inputs: 4 },
    ];
    for (i, kind) in nodes.into_iter().enumerate() {
        ed.insert(
            NodeKey(base + i as u64),
            "n",
            Unforkable(TestNode::new(kind)),
        );
    }
    for i in 4..6 {
        ed.insert(NodeKey(base + i), "holder", Unforkable(Holder::new(30)));
    }
    let spec = ed.spec_mut();
    spec.connect_events(sink(base + 2, 0), EventEdge::Direct(ev(base + 1, 0)));
    spec.connect_events(sink(base + 3, 0), EventEdge::Direct(ev(base + 2, 0)));
    spec.connect_events(sink(base + 3, 1), EventEdge::Direct(ev(base, 0)));
    spec.connect_events(sink(base + 3, 2), EventEdge::Direct(ev(base + 4, 0)));
    spec.connect_events(sink(base + 3, 3), EventEdge::Direct(ev(base + 5, 0)));
}

/// **Applying a commit allocates and frees nothing**: the new arena, every
/// ring and FIFO, and the state they carry across are built with the commit
/// (`exec/stage.rs`), and applying moves them. Each commit below is applied
/// inside the gate, then blocks render inside it. Together they cover every
/// path the claim names:
///
/// - nodes with an audio PDC ring and an event FIFO join; the arena grows
///   and the store with it;
/// - an event FIFO whose bound grows (it moves into one built for it) and
///   one whose bound shrinks (it is retuned in place and its slot widened);
/// - audio and event feedback, then a feedback edge whose delay changes (the
///   old FIFO flushes);
/// - held keys closed on a cut edge, and on a hard replace whose edge stays
///   behind a delay;
/// - a tap, an event tap and a meter carried across commits, and a tap
///   opened again, which wins over the carried one;
/// - weighted lanes, then one a source drives, carried across a commit;
/// - crossfades, a fade queued behind another, and a chase handed on;
/// - a re-prepare's suspend and resume, at the same rate and at another.
fn applying_a_commit_allocates_nothing_on(workers: usize) {
    use tutti_buffers::MaxBlock;
    use tutti_graph::{
        EdgeGain, EdgeId, EventCapacity, EventTapId, MeterId, MeterSpec, PortKind, PortSink,
        TapAlign, TapId, TapOptions,
    };
    use tutti_units::{Amplitude, SampleRate};

    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let first = pdc_pair(&mut ed, 1, 40);
    let out_of = |node: NodeKey| PortSource::Node(OutputPort { node, port: 0 });
    ed.spec_mut().topology.set_outputs(vec![out_of(first)]);
    let cap = Samples::new(1 << 16);
    let mut tap = ed
        .open_tap(TapId::of(1), vec![out_of(first)], TapOptions::new(cap))
        .expect("the source exists");
    let meter = ed
        .open_meter(MeterId::of(1), MeterSpec::Outputs)
        .expect("opens");
    meter.enable();

    let input = vec![0.25f32; 64];
    let mut outs = vec![vec![0.0f32; 64]; 4];
    let transport = TransportPosition::default();
    let mut blocks = |exec: &mut tutti_graph::Executor, n: usize, frames: usize| {
        for _ in 0..n {
            let wide = exec.plan().map_or(1, |p| p.global_outputs()).min(4);
            let mut slices: Vec<&mut [f32]> = Vec::new();
            slices.extend(outs.iter_mut().take(wide).map(|o| &mut o[..]));
            // The slices of a block are made before the gate.
            exec.process(frames, &transport, &[&input[..frames]], &mut slices);
        }
    };
    let slots = |exec: &tutti_graph::Executor| exec.plan().expect("applied").slots(PortKind::Audio);
    // From the wide plan on, every apply leaves the executor in the mode the
    // pool names.
    let applied = |exec: &mut tutti_graph::Executor| {
        apply(exec);
        assert_eq!(
            exec.is_parallel(),
            workers > 1,
            "the gate runs the mode it names"
        );
    };

    // The first plan: the empty state gives way to a staged one.
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    blocks(&mut exec, 8, 64);

    // Nodes with an audio PDC ring and an event FIFO join; a second tap, an
    // event tap and the held keys are in them.
    let second = pdc_pair(&mut ed, 10, 25);
    event_pdc(&mut ed, 20);
    ed.spec_mut()
        .topology
        .set_outputs(vec![out_of(first), out_of(second)]);
    let events = ed
        .open_event_tap(
            EventTapId::of(1),
            ev(20, 0),
            EventTapOptions::new(EventCapacity::of(256)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    ed.commit().expect("commits");
    let delays = exec.plan().expect("applied").delays().len();
    apply(&mut exec);
    assert!(exec.plan().expect("applied").delays().len() > delays);
    blocks(&mut exec, 16, 64);
    ed.collect();

    // The arena and the store grow: eight chains feed one wide sum.
    let (before, units) = (slots(&exec), exec.plan().expect("applied").units().len());
    let sums: Vec<NodeKey> = (0..8)
        .map(|i| pdc_pair(&mut ed, 100 + 3 * i, 8 + i as usize))
        .collect();
    let wide = NodeKey(200);
    ed.insert(
        wide,
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 8 })),
    );
    for (i, sum) in sums.iter().enumerate() {
        ed.spec_mut()
            .topology
            .edges
            .insert(at(200, i as u16), vec![from(sum.0, 0)]);
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![out_of(first), out_of(second), out_of(wide)]);
    ed.commit().expect("commits");
    applied(&mut exec);
    assert!(slots(&exec) > before, "the arena grew");
    assert!(exec.plan().expect("applied").units().len() > 2 * units);
    blocks(&mut exec, 16, 64);
    ed.collect();

    // An event FIFO whose bound grows, and one whose bound shrinks; a tap
    // opened again wins over the one carried.
    let lag = NodeKey(22);
    ed.set_latency(lag, Latency::new(Samples::new(400)))
        .expect("fits");
    let reopened = ed
        .open_tap(TapId::of(1), vec![out_of(first)], TapOptions::new(cap))
        .expect("the source exists");
    drop(std::mem::replace(&mut tap, reopened));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();
    ed.set_latency(lag, Latency::new(Samples::new(3)))
        .expect("fits");
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();

    // Audio and event feedback.
    let (smooth, gain, mix) = (NodeKey(300), NodeKey(301), NodeKey(302));
    ed.insert(smooth, "smooth", Unforkable(TestNode::new(Kind::Smooth)));
    ed.insert(
        gain,
        "gain",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        })),
    );
    ed.insert(
        mix,
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 2 })),
    );
    let feedback = |delay: usize| {
        vec![Edge::Feedback(FeedbackFrom::one_block(
            OutputPort {
                node: gain,
                port: 0,
            },
            Samples::new(delay),
        ))]
    };
    {
        let t = &mut ed.spec_mut().topology;
        t.edges
            .insert(at(300, 0), vec![Edge::Direct(PortSource::Global(0))]);
        t.edges.insert(at(302, 0), vec![from(300, 0)]);
        t.edges.insert(at(302, 1), feedback(128));
        t.edges.insert(at(301, 0), vec![from(302, 0)]);
    }
    for (k, kind) in [
        Kind::Emitter {
            period: 5,
            phase: 0,
        },
        Kind::EventLag { latency: 11 },
        Kind::Consumer { inputs: 2 },
    ]
    .into_iter()
    .enumerate()
    {
        ed.insert(
            NodeKey(310 + k as u64),
            "n",
            Unforkable(TestNode::new(kind)),
        );
    }
    let efb = |delay: usize| EventEdge::feedback(ev(311, 0), Samples::new(delay));
    ed.spec_mut()
        .connect_events(sink(311, 0), EventEdge::Direct(ev(310, 0)));
    ed.spec_mut()
        .connect_events(sink(312, 0), EventEdge::Direct(ev(311, 0)));
    ed.spec_mut().connect_events(sink(312, 1), efb(128));
    ed.spec_mut().topology.set_outputs(vec![
        out_of(first),
        out_of(second),
        out_of(wide),
        out_of(gain),
    ]);
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();
    // The feedback delays change: new keys, and the old FIFOs flush.
    ed.spec_mut()
        .topology
        .edges
        .insert(at(302, 1), feedback(192));
    ed.spec_mut().disconnect_events(sink(312, 1), ev(311, 0));
    ed.spec_mut().connect_events(sink(312, 1), efb(192));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();

    // Weighted lanes on the output edges, then one a source drives.
    let edge = |node: NodeKey| EdgeId {
        sink: PortSink::Output(0),
        from: from(node.0, 0),
    };
    ed.spec_mut()
        .set_edge_gain(edge(first), EdgeGain::new(Amplitude::new(0.5)));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 8, 64);
    ed.collect();
    ed.insert(
        NodeKey(400),
        "lane",
        Unforkable(TestNode::new(Kind::GainDriver {
            period: 8,
            latency: 0,
        })),
    );
    ed.spec_mut().connect_edge_gain(edge(first), ev(400, 0));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();
    // The driven lane and the tap, event tap and meter ride a plain commit.
    ed.insert(
        NodeKey(401),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 8, 64);
    ed.collect();

    // Held keys close: the edge from one holder is cut, and the other holder
    // is replaced hard while its edge stays behind its delay.
    ed.spec_mut().disconnect_events(sink(23, 2), ev(24, 0));
    ed.replace(
        NodeKey(25),
        Unforkable(Holder::new(0)),
        Fade::new(Samples::new(0)).law(FadeLaw::EqualPower),
    )
    .expect("fits");
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 24, 64);
    ed.collect();

    // Closes owed across commits: a holder of more keys than one block of the
    // sink takes is cut (the apply merges its keys into a table and leaves
    // the rest owed), a commit follows before any block (the table moves
    // over), and the blocks drain it.
    ed.insert(NodeKey(600), "holder", Unforkable(Holder::new(1_500)));
    ed.insert(
        NodeKey(601),
        "consume",
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
    );
    ed.spec_mut()
        .connect_events(sink(601, 0), EventEdge::Direct(ev(600, 0)));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 6, 64);
    ed.collect();
    ed.spec_mut().disconnect_events(sink(601, 0), ev(600, 0));
    ed.commit().expect("commits");
    applied(&mut exec);
    ed.insert(
        NodeKey(602),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 12, 64);
    ed.collect();

    // A FIFO that owes closes. A holder behind an event delay is replaced
    // hard: what the FIFO has no room for stays owed and pours in block by
    // block (the tally shows the drain is still going after the apply). Then
    // its edge is cut while the table is not empty: the FIFO vanishes, and
    // what it owes moves to its sink.
    let offs = Arc::new(AtomicUsize::new(0));
    let tally = |offs: &Arc<AtomicUsize>, inputs| OffCount {
        offs: Arc::clone(offs),
        inputs,
    };
    ed.insert(NodeKey(700), "holder", Unforkable(Holder::paced(1_024, 64)));
    ed.insert(NodeKey(701), "tally", Unforkable(tally(&offs, 2)));
    ed.insert(
        NodeKey(702),
        "lag",
        Unforkable(TestNode::new(Kind::EventLag { latency: 11 })),
    );
    ed.insert(
        NodeKey(703),
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 5,
            phase: 0,
        })),
    );
    {
        let spec = ed.spec_mut();
        spec.connect_events(sink(702, 0), EventEdge::Direct(ev(703, 0)));
        spec.connect_events(sink(701, 0), EventEdge::Direct(ev(700, 0)));
        spec.connect_events(sink(701, 1), EventEdge::Direct(ev(702, 0)));
    }
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 20, 64);
    ed.collect();
    assert_eq!(offs.load(Ordering::Relaxed), 0, "nothing closed yet");
    ed.replace(
        NodeKey(700),
        Unforkable(Holder::paced(0, 64)),
        Fade::new(Samples::new(0)).law(FadeLaw::EqualPower),
    )
    .expect("fits");
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 2, 64);
    let mid = offs.load(Ordering::Relaxed);
    assert!(
        0 < mid && mid < 1_024,
        "the FIFO's table is still draining: {mid} of 1024 closed"
    );
    ed.collect();
    ed.spec_mut().disconnect_events(sink(701, 0), ev(700, 0));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 40, 64);
    ed.collect();
    assert_eq!(
        offs.load(Ordering::Relaxed),
        1_024,
        "every key closed, once, whether the FIFO poured it or handed it on"
    );
    // Two holders cut from one sink port in one commit: two tables owed to
    // it, each more than the sink's block takes.
    let both = Arc::new(AtomicUsize::new(0));
    for k in [710, 711] {
        ed.insert(NodeKey(k), "holder", Unforkable(Holder::paced(700, 64)));
    }
    ed.insert(NodeKey(712), "tally", Unforkable(tally(&both, 1)));
    for k in [710, 711] {
        ed.spec_mut()
            .connect_events(sink(712, 0), EventEdge::Direct(ev(k, 0)));
    }
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 14, 64);
    ed.collect();
    for k in [710, 711] {
        ed.spec_mut().disconnect_events(sink(712, 0), ev(k, 0));
    }
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 30, 64);
    ed.collect();
    assert_eq!(
        both.load(Ordering::Relaxed),
        1_400,
        "each cut edge's keys close, once each"
    );

    // Crossfades: a fade, one queued behind it, and a chase handed on.
    let chaser = |n: u64| HeldChaser(Arc::new(AtomicU64::new(n)));
    ed.insert(NodeKey(500), "chaser", Unforkable(chaser(0)));
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 4, 64);
    ed.collect();
    let fade = |n| Fade::new(Samples::new(n)).law(FadeLaw::EqualPower);
    ed.replace(NodeKey(500), Unforkable(chaser(1)), fade(1_000))
        .expect("fits");
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 2, 64);
    ed.replace(NodeKey(500), Unforkable(chaser(2)), fade(1_000))
        .expect("fits");
    ed.commit().expect("commits");
    applied(&mut exec);
    blocks(&mut exec, 40, 64);
    ed.collect();

    // A re-prepare: suspend and resume, at the same rate and at another.
    for next in [
        prepare(64),
        tutti_graph::Prepare::new(SampleRate::SR_44K1, MaxBlock::of(Samples::new(64))),
    ] {
        ed.reprepare(next).expect("accepted");
        applied(&mut exec);
        blocks(&mut exec, 2, 64);
        ed.collect();
        applied(&mut exec);
        blocks(&mut exec, 24, 64);
        ed.collect();
    }

    assert_eq!(exec.dropped_event_count(), 0);
    drop((tap, events, meter));
}

#[test]
fn applying_a_commit_allocates_nothing() {
    applying_a_commit_allocates_nothing_on(1);
}

/// The same with a pool of four: the parallel state is built with the commit
/// too.
#[test]
fn applying_a_commit_allocates_nothing_in_parallel() {
    applying_a_commit_allocates_nothing_on(4);
}

/// Writes, on its first block, a MIDI 1.0 note on, every hold pedal down and
/// the bend off centre on channel 0, and holds them.
struct Latch {
    sent: bool,
}

impl Node for Latch {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        if !std::mem::replace(&mut self.sent, true) {
            let ch = MidiChannel::of(0);
            let cc = |n| midi::m1_cc(ch, CCNumber::of(n), U7::of(127));
            let held = [
                midi::m1_note_on(ch, 60, U7::of(100)),
                cc(64),
                cc(66),
                cc(67),
                cc(69),
                Message::midi1(ch, Voice1::PitchBend(U14::of(0))).into(),
            ];
            for ump in held {
                let _ = io
                    .event_out(0)
                    .push(tutti_graph::Event::midi(tutti_graph::Offset::ZERO, ump));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// Counts what closes a [`Latch`]: MIDI 1.0 note offs, hold pedals at 0 and
/// bends at their centre (MIDI 1.0 Pitch Bend, 0x2000).
struct Closed {
    offs: Arc<AtomicUsize>,
    pedals_up: Arc<AtomicUsize>,
    bends_centred: Arc<AtomicUsize>,
}

impl Node for Closed {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(0) {
            let EventKind::Midi(Ump(w)) = e.kind else {
                continue;
            };
            match (w[0] >> 28, (w[0] >> 20) & 0xf) {
                (2, 0x8) => &self.offs,
                (2, 0xb) if w[0] & 0x7f == 0 => &self.pedals_up,
                (2, 0xe) if w[0] & 0x7f7f == 0x0040 => &self.bends_centred,
                _ => continue,
            }
            .fetch_add(1, Ordering::Relaxed);
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// **Closing what a source holds allocates nothing, controllers included**: a
/// source holds a key, the four hold pedals and an off-centre bend. Its edge is
/// cut (the close at the commit), and a source replaced twice behind a fade (the
/// second replace waits, then takes over, and the unit it displaces owes its
/// closes to the blocks): each close reaches the sink as the key's Note Off,
/// four pedal-ups (CC 64, 66, 67, 69 at 0) and a centred bend, with no
/// allocation in the commit or the blocks.
fn closing_pedals_and_bends_allocates_nothing_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    let (offs, pedals_up, bends_centred) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let closed = || Closed {
        offs: Arc::clone(&offs),
        pedals_up: Arc::clone(&pedals_up),
        bends_centred: Arc::clone(&bends_centred),
    };
    let input = vec![0.25f32; 64];
    let mut outs = vec![vec![0.0f32; 64]; 1];
    let transport = TransportPosition::default();
    let mut blocks = |exec: &mut tutti_graph::Executor, n: usize| {
        for _ in 0..n {
            let mut slices: Vec<&mut [f32]> = outs.iter_mut().map(|o| &mut o[..]).collect();
            exec.process(64, &transport, &[&input[..]], &mut slices);
        }
    };
    let tallies = || [&offs, &pedals_up, &bends_centred].map(|n| n.load(Ordering::Relaxed));

    // Cut: the commit that removes the edge closes what its source holds.
    ed.insert(NodeKey(1), "latch", Unforkable(Latch { sent: false }));
    ed.insert(NodeKey(2), "closed", Unforkable(closed()));
    ed.spec_mut()
        .connect_events(sink(2, 0), EventEdge::Direct(ev(1, 0)));
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    blocks(&mut exec, 4);
    assert_eq!(tallies(), [0, 0, 0], "nothing closes while the edge stands");
    ed.spec_mut().disconnect_events(sink(2, 0), ev(1, 0));
    ed.commit().expect("commits");
    apply(&mut exec);
    blocks(&mut exec, 4);
    ed.collect();
    assert_eq!(
        tallies(),
        [1, 4, 1],
        "a cut edge closes the key, pedals and bend"
    );

    // Promote: a second replace waits behind the first's fade and takes over;
    // the unit it displaces owes the keys it held.
    ed.spec_mut()
        .connect_events(sink(2, 0), EventEdge::Direct(ev(1, 0)));
    ed.commit().expect("commits");
    apply(&mut exec);
    blocks(&mut exec, 4);
    ed.collect();
    let fade = Fade::new(Samples::new(128)).law(FadeLaw::EqualPower);
    ed.replace(NodeKey(1), Unforkable(Latch { sent: false }), fade)
        .expect("fits");
    ed.commit().expect("commits");
    apply(&mut exec);
    blocks(&mut exec, 1);
    ed.replace(NodeKey(1), Unforkable(Latch { sent: false }), fade)
        .expect("fits");
    ed.commit().expect("commits");
    apply(&mut exec);
    blocks(&mut exec, 16);
    ed.collect();
    assert_eq!(
        tallies(),
        [3, 12, 3],
        "each unit that stopped closed its key, pedals and bend"
    );
    assert_eq!(exec.dropped_event_count(), 0);
}

#[test]
fn closing_pedals_and_bends_allocates_nothing() {
    closing_pedals_and_bends_allocates_nothing_on(1);
}

#[test]
fn parallel_closing_pedals_and_bends_allocates_nothing() {
    closing_pedals_and_bends_allocates_nothing_on(4);
}

/// **Retiring a unit that holds keys, and a re-prepare across it, allocate
/// nothing**: the unit's tables leave through the commit's preallocated lists
/// (the departed keys, the spare tables, the retired unit), its keys close at
/// the sink exactly once, and a re-prepare parks the tables of a unit that
/// holds keys and hands them back without closing them.
fn retiring_and_re_preparing_a_key_holding_unit_allocates_nothing_on(workers: usize) {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    use_pool(&mut exec, workers);
    let (offs, pedals_up, bends_centred) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let input = vec![0.25f32; 64];
    let mut outs = vec![vec![0.0f32; 64]; 1];
    let transport = TransportPosition::default();
    let mut blocks = |exec: &mut tutti_graph::Executor, n: usize| {
        for _ in 0..n {
            let mut slices: Vec<&mut [f32]> = outs.iter_mut().map(|o| &mut o[..]).collect();
            exec.process(64, &transport, &[&input[..]], &mut slices);
        }
    };
    let tallies = || [&offs, &pedals_up, &bends_centred].map(|n| n.load(Ordering::Relaxed));
    ed.insert(NodeKey(2), "closed", {
        Unforkable(Closed {
            offs: Arc::clone(&offs),
            pedals_up: Arc::clone(&pedals_up),
            bends_centred: Arc::clone(&bends_centred),
        })
    });
    ed.insert(NodeKey(1), "latch", Unforkable(Latch { sent: false }));
    ed.spec_mut()
        .connect_events(sink(2, 0), EventEdge::Direct(ev(1, 0)));
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    blocks(&mut exec, 4);

    // A re-prepare while the unit holds keys: the tables wait, then return.
    ed.reprepare(prepare(64)).expect("accepted");
    apply(&mut exec);
    blocks(&mut exec, 2);
    ed.collect();
    apply(&mut exec);
    blocks(&mut exec, 4);
    ed.collect();
    assert_eq!(tallies(), [0, 0, 0], "a re-prepare closes nothing");

    // The unit is removed holding them: its keys close once.
    ed.remove(NodeKey(1));
    ed.commit().expect("commits");
    apply(&mut exec);
    blocks(&mut exec, 4);
    assert_eq!(ed.collect(), vec![NodeKey(1)], "the unit came back");
    assert_eq!(
        tallies(),
        [1, 4, 1],
        "the retired unit's key, pedals and bend close once"
    );
    assert_eq!(exec.dropped_event_count(), 0);
}

#[test]
fn retiring_and_re_preparing_a_key_holding_unit_allocates_nothing() {
    retiring_and_re_preparing_a_key_holding_unit_allocates_nothing_on(1);
}

#[test]
fn parallel_retiring_and_re_preparing_a_key_holding_unit_allocates_nothing() {
    retiring_and_re_preparing_a_key_holding_unit_allocates_nothing_on(4);
}

/// **A pool set while a commit is in flight**: the commit was staged for no
/// pool, `set_pool` re-stages it with the parallel state for this one, and
/// applying it allocates and frees nothing; then the pool goes and the commit
/// is re-staged serial. This covers the re-staged path. The race, a commit
/// staged for the old pool by another thread while `set_pool` runs, cannot be
/// reproduced deterministically; its answer (the unused parallel state goes
/// back in the commit box) is the same hand-back.
#[test]
fn a_pool_set_across_a_commit_in_flight_allocates_nothing() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let sums: Vec<NodeKey> = (0..8)
        .map(|i| pdc_pair(&mut ed, 100 + 3 * i, 8 + i as usize))
        .collect();
    ed.insert(
        NodeKey(200),
        "sum",
        Unforkable(TestNode::new(Kind::Sum { inputs: 8 })),
    );
    for (i, sum) in sums.iter().enumerate() {
        ed.spec_mut()
            .topology
            .edges
            .insert(at(200, i as u16), vec![from(sum.0, 0)]);
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutputPort {
            node: NodeKey(200),
            port: 0,
        })]);
    ed.commit().expect("commits");
    use_pool(&mut exec, 4);
    apply(&mut exec);
    assert!(exec.is_parallel(), "the commit in flight met the pool");
    ed.collect();
    // And back: the commit staged for a pool meets none.
    ed.insert(
        NodeKey(900),
        "smooth",
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.commit().expect("commits");
    exec.set_pool(None);
    apply(&mut exec);
    assert!(!exec.is_parallel());
    ed.collect();
}

/// An output tap opened again at another width is swapped in by a commit that
/// allocates and frees nothing on the audio side, and the blocks after it
/// write the new tap alone. The old reader holds the frames written before
/// the commit and ends once the executor has handed the commit back.
#[test]
fn an_output_tap_opened_again_at_another_width_swaps_without_allocating() {
    use tutti_buffers::{InterleavedMut, Pull, Source as _};
    use tutti_graph::TapId;

    let (mut ed, mut exec) = Editor::new(prepare(64));
    let node = NodeKey(1);
    ed.insert(
        node,
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.25,
            width: 3,
        })),
    );
    let port = |p| PortSource::Node(OutputPort { node, port: p });
    ed.spec_mut()
        .topology
        .set_outputs(vec![port(0), port(1), port(2)]);
    let mut wide = ed
        .open_heard_tap(TapId::of(1), 3, Samples::new(1024))
        .expect("the outputs exist");
    ed.commit().expect("commits");
    apply(&mut exec);
    ed.collect();
    let (mut a, mut b, mut c) = (vec![0.0f32; 64], vec![0.0f32; 64], vec![0.0f32; 64]);
    let mut run = |exec: &mut tutti_graph::Executor, blocks: usize| {
        for _ in 0..blocks {
            exec.process(
                64,
                &TransportPosition::default(),
                &[],
                &mut [&mut a[..], &mut b[..], &mut c[..]],
            );
        }
    };
    run(&mut exec, 4);

    // The main output narrows to one channel and its tap is opened again.
    ed.spec_mut().topology.set_outputs(vec![port(0)]);
    let mut narrow = ed
        .open_heard_tap(TapId::of(1), 1, Samples::new(1024))
        .expect("the outputs exist");
    ed.commit().expect("commits");
    apply(&mut exec);
    run(&mut exec, 4);
    ed.collect();

    assert!(
        wide.source_gone(),
        "the old reader ends once the commit is applied"
    );
    assert_eq!(
        wide.available().get(),
        4 * 64,
        "what was written before the commit"
    );
    let mut frames = vec![0.0f32; 4 * 64 * 3];
    assert_eq!(
        wide.pop_into(InterleavedMut::new(&mut frames, wide.layout()).unwrap())
            .unwrap()
            .get(),
        4 * 64
    );
    assert!(frames.iter().all(|&x| x == 0.25));
    let mut empty = vec![0.0f32; 3];
    assert!(matches!(
        wide.pull(InterleavedMut::new(&mut empty, wide.layout()).unwrap()),
        Pull::Ended
    ));
    assert_eq!(
        narrow.available().get(),
        4 * 64,
        "the new tap is fed from the commit's block"
    );
    let mut one = vec![0.0f32; 4 * 64];
    assert_eq!(
        narrow
            .pop_into(InterleavedMut::new(&mut one, narrow.layout()).unwrap())
            .unwrap()
            .get(),
        4 * 64
    );
    assert!(one.iter().all(|&x| x == 0.25));
    assert_eq!(wide.take_lost().get() + narrow.take_lost().get(), 0);
}
