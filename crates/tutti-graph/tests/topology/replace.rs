//! `Editor::replace`: swapping a running node's unit with a crossfade. The
//! rules are in `src/fade.rs`; each test here pins one.

use tutti_graph::testing as common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use common::{Kind, TestNode, prepare};

use tutti_buffers::ChannelLayout;
use tutti_fixtures::midi;
use tutti_graph::NodeKey;
use tutti_graph::{
    CommitError, Cx, Editor, EventIn, EventKind, Executor, Fade, FadeLaw, Io, Node, Prepare, Shape,
    Status, TransportPosition, Unforkable, compile,
};
use tutti_graph::{Edge, InPort, OutPort, PortSource};
use tutti_rt::RtScope;
use tutti_ump::{MidiChannel, U7};
use tutti_units::{At, Samples};

const NODE: NodeKey = NodeKey(1);

fn gain(g: f32) -> TestNode {
    TestNode::new(Kind::Gain { gain: g, width: 1 })
}

/// Global input 0 → `NODE` (a one-channel gain, aliased in place) → output
/// 0, committed and applied.
fn gain_graph(g: f32) -> (Editor, Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(NODE, "gain", Unforkable(gain(g)));
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NODE,
            port: 0,
        },
        vec![Edge::Direct(PortSource::Global(0))],
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort {
            node: NODE,
            port: 0,
        })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(
        exec.plan().expect("applied").in_place(NODE).get(0),
        "the fade must run over an in-place op: the outgoing unit reads a slot the incoming one overwrites"
    );
    (ed, exec)
}

/// Render `n` frames of DC 1.0 into the graph; returns output 0.
fn dc(exec: &mut Executor, n: usize) -> Vec<f32> {
    let input = vec![1.0f32; n];
    let mut out = vec![0.0f32; n];
    exec.process(
        n,
        &TransportPosition::default(),
        &[&input[..]],
        &mut [&mut out[..]],
    );
    out
}

/// Blocks of uneven length, so fades start and end mid-block.
const RAGGED: [usize; 7] = [7, 13, 64, 1, 50, 128, 33];

/// No step at either end of a fade: on a DC input, a gain of 1 replaced by a
/// gain of 3 moves by at most one fade step per sample — the steepest slope
/// of the curve over `len + 1` steps (15/8 · 2 for equal amplitude, √10 · π/2
/// for equal power; 5 bounds both) — where a swap moves by 2 in one sample.
#[test]
fn a_fade_has_no_step_at_either_end() {
    for curve in [FadeLaw::EqualAmplitude, FadeLaw::EqualPower] {
        let len = 100;
        let (mut ed, mut exec) = gain_graph(1.0);
        let mut out = dc(&mut exec, 40);
        ed.replace(
            NODE,
            Unforkable(gain(3.0)),
            Fade::new(Samples::new(len)).law(curve),
        )
        .expect("same shape");
        ed.commit().expect("commits");
        for &n in RAGGED.iter().cycle().take(12) {
            out.extend(dc(&mut exec, n));
        }
        // Five times the position step, scaled by the smootherstep's steepest
        // slope (15/8): the level rises by at most 3 and falls by at most 1
        // over a fade, and a law's gain moves at most pi/2 of that.
        let bound = 5.0 * 1.875 / (len + 1) as f32;
        let worst = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= bound,
            "{curve:?}: a step of {worst} (bound {bound})"
        );
        assert_eq!(out[0], 1.0);
        assert_eq!(*out.last().expect("rendered"), 3.0);
    }
}

/// The fade is exactly its length: from the first frame of the block the
/// commit lands on, `len` frames mix both units (strictly between the two
/// levels), and the frame after is the incoming unit alone — across ragged
/// blocks, so the end falls mid-block.
#[test]
fn the_fade_is_exactly_its_length() {
    let len = 100;
    let (mut ed, mut exec) = gain_graph(1.0);
    let mut out = dc(&mut exec, 20);
    let start = out.len();
    ed.replace(NODE, Unforkable(gain(3.0)), Fade::new(Samples::new(len)))
        .expect("same shape");
    ed.commit().expect("commits");
    for &n in RAGGED.iter().cycle().take(10) {
        out.extend(dc(&mut exec, n));
    }
    assert!(out[..start].iter().all(|&y| y == 1.0));
    for (k, &y) in out[start..start + len].iter().enumerate() {
        assert!(y > 1.0 && y < 3.0, "fade frame {k}: {y}");
    }
    assert!(
        out[start + len..].iter().all(|&y| y == 3.0),
        "after the fade: {:?}",
        &out[start + len..start + len + 4]
    );
}

/// A unit that records where it was dropped: 1 off the audio thread, 2 on it.
struct DropProbe {
    inner: TestNode,
    dropped: Arc<AtomicU8>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        // `check_outside` panics only under `debug_assertions`, so a drop on the audio thread is
        // recorded in debug builds alone; a release run still pins the drop's place and time.
        let on_audio = cfg!(debug_assertions)
            && std::panic::catch_unwind(|| RtScope::check_outside("a node")).is_err();
        self.dropped
            .store(if on_audio { 2 } else { 1 }, Ordering::SeqCst);
    }
}

impl Node for DropProbe {
    fn shape(&self) -> Shape {
        self.inner.shape()
    }
    fn prepare(&mut self, p: &Prepare) {
        self.inner.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.inner.process(cx, io)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// The outgoing unit retires on the control thread: the commit that started
/// the fade comes back at once, and the crossfade comes back on the
/// fade-return ring when it ends, carrying the outgoing unit, which
/// `collect` frees — never the executor.
#[test]
fn the_outgoing_unit_retires_on_the_control_thread() {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let dropped = Arc::new(AtomicU8::new(0));
    ed.insert(
        NODE,
        "gain",
        Unforkable(DropProbe {
            inner: gain(1.0),
            dropped: Arc::clone(&dropped),
        }),
    );
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NODE,
            port: 0,
        },
        vec![Edge::Direct(PortSource::Global(0))],
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort {
            node: NODE,
            port: 0,
        })]);
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    ed.collect();

    ed.replace(
        NODE,
        Unforkable(gain(2.0)),
        Fade::new(Samples::new(100)).law(FadeLaw::EqualPower),
    )
    .expect("same shape");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    assert!(ed.collect().is_empty(), "nothing retires mid-fade");
    assert_eq!(ed.in_flight(), 0, "the fade does not hold its commit");
    assert_eq!(ed.fades_in_flight(), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);

    dc(&mut exec, 64);
    // The fade ended in that block: the box is back in the return ring, and
    // the unit in it is alive until the control thread drains it.
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "not freed by the executor"
    );
    assert_eq!(ed.collect(), vec![NODE]);
    assert_eq!(dropped.load(Ordering::SeqCst), 1, "freed by collect");
    assert_eq!(ed.fades_in_flight(), 0);
}

/// A fade does not hold its commit: ten replaces, each committed on its own
/// with a one-second fade still running, all go through, and so does a
/// re-prepare — `Backpressure` is about commits in the queue, not fades in
/// flight.
#[test]
fn long_fades_do_not_block_commits() {
    let (mut ed, mut exec) = Editor::new(prepare(128));
    let keys: Vec<NodeKey> = (1..=10).map(NodeKey).collect();
    for &k in &keys {
        ed.insert(k, "gain", Unforkable(gain(1.0)));
    }
    ed.commit().expect("commits");
    exec.process(64, &TransportPosition::default(), &[], &mut []);
    let second = Fade::new(Samples::new(48_000));
    for &k in &keys {
        ed.replace(k, Unforkable(gain(2.0)), second).expect("fits");
        ed.commit().expect("a running fade blocks nothing");
        exec.process(64, &TransportPosition::default(), &[], &mut []);
    }
    assert_eq!(ed.fades_in_flight(), 10);
    ed.reprepare(prepare(64)).expect("nor a re-prepare");
}

/// A remove in the same commit as a long fade retires at once: its key is
/// reported by the `collect` after the next block, not after the fade.
#[test]
fn a_remove_beside_a_fade_retires_at_once() {
    let (mut ed, mut exec) = gain_graph(1.0);
    let other = NodeKey(2);
    ed.insert(other, "gain", Unforkable(gain(1.0)));
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    ed.collect();
    ed.replace(NODE, Unforkable(gain(2.0)), Fade::new(Samples::new(48_000)))
        .expect("fits");
    ed.remove(other);
    ed.commit().expect("commits");
    dc(&mut exec, 16);
    assert_eq!(ed.collect(), vec![other]);
    assert_eq!(ed.fades_in_flight(), 1, "the fade still runs");
}

/// A replace whose unit differs in shape is refused, naming the key, and
/// changes nothing: other ports, another latency, another in-place
/// acceptance, or another declared event capacity. A node with nothing
/// running is refused too. And the verifier refuses the same fade handed
/// over in a delta built by hand.
#[test]
fn a_shape_mismatch_is_refused() {
    let fade = Fade::new(Samples::new(64));
    let (mut ed, mut exec) = gain_graph(1.0);
    let wider = TestNode::new(Kind::Gain {
        gain: 1.0,
        width: 2,
    });
    assert!(
        ed.replace(NODE, Unforkable(wider), fade)
            .map(|placed| placed.controls)
            .is_err_and(|e| matches!(e, CommitError::FadeShape { node, .. } if node == NODE))
    );
    let not_in_place = TestNode::new(Kind::Thru { width: 1 });
    assert!(
        ed.replace(NODE, Unforkable(not_in_place), fade)
            .map(|placed| placed.controls)
            .is_err_and(|e| matches!(e, CommitError::FadeShape { node, .. } if node == NODE))
    );
    assert_eq!(
        ed.replace(NodeKey(9), Unforkable(gain(1.0)), fade)
            .map(|placed| placed.controls),
        Err(CommitError::NotRunning { node: NodeKey(9) })
    );
    ed.commit().expect("nothing changed");
    assert_eq!(
        exec.plan()
            .expect("applied")
            .unit(NODE)
            .expect("kept")
            .generation,
        0
    );

    // Latency.
    let lagged = NodeKey(2);
    ed.insert(
        lagged,
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
    );
    ed.spec_mut()
        .topology
        .outputs
        .push(vec![PortSource::Node(OutPort {
            node: lagged,
            port: 0,
        })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(
        ed.replace(
            lagged,
            Unforkable(TestNode::new(Kind::Lag { latency: 5 })),
            fade
        )
        .map(|placed| placed.controls)
        .is_err_and(|e| matches!(e, CommitError::FadeShape { node, .. } if node == lagged))
    );
    ed.replace(
        lagged,
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
        fade,
    )
    .expect("the same latency fades");

    // Declared event capacity: the plan sizes the port's buffers from it.
    let burst = |cap| {
        TestNode::new(Kind::Burst {
            period: 200,
            phase: 0,
            burst: 1,
            cap,
        })
    };
    let bursting = NodeKey(3);
    ed.insert(bursting, "burst", Unforkable(burst(2)));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert!(
        ed.replace(bursting, Unforkable(burst(3)), fade)
            .map(|placed| placed.controls)
            .is_err_and(|e| matches!(e, CommitError::FadeShape { node, .. } if node == bursting))
    );
    ed.replace(bursting, Unforkable(burst(2)), fade)
        .expect("the same capacity fades");

    // The verifier, on a delta built by hand: a fade across a latency change.
    let (mut ed, mut exec) = Editor::new(prepare(128));
    ed.insert(
        lagged,
        "lag",
        Unforkable(TestNode::new(Kind::Lag { latency: 3 })),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort {
            node: lagged,
            port: 0,
        })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut spec = ed.spec().clone();
    spec.generations.insert(lagged, 1);
    let later = Kind::Lag { latency: 5 };
    let shape = TestNode::new(later.clone()).shape();
    spec.topology.nodes.insert(
        lagged,
        tutti_graph::NodeSpec::new("lag", shape.audio_in(), shape.audio_out())
            .with_latency(shape.latency.samples())
            .with_tail(shape.tail),
    );
    let shapes: tutti_graph::Shapes = [(lagged, shape)].into_iter().collect();
    let valid = spec.validate().expect("valid");
    let (plan, mut delta) =
        compile(&valid, &shapes, &prepare(128), ed.base().map(|p| &**p)).expect("compiles");
    delta.fades = vec![(lagged, fade)];
    let units: BTreeMap<NodeKey, Box<dyn Node>> =
        [(lagged, Box::new(TestNode::new(later)) as Box<dyn Node>)]
            .into_iter()
            .collect();
    match ed.package(plan, delta, units) {
        Err(CommitError::Fade(e)) => assert!(e.0.contains("node 2"), "{e}"),
        other => panic!("a fade across a latency change was packaged: {other:?}"),
    }
}

/// A replace while a fade runs waits for it, then fades from its incoming
/// unit — from the block after the running fade ends; a third replace
/// while one waits supersedes the waiting unit, which never runs and
/// retires on the control thread. Checked sample for sample against the
/// rule written out here: gains 1 → 2 over 100 frames, then 2 → 8 over 50.
#[test]
fn a_replace_during_a_fade_waits_for_it() {
    let (mut ed, mut exec) = gain_graph(1.0);
    let fade = |len| Fade::new(Samples::new(len));
    ed.replace(NODE, Unforkable(gain(2.0)), fade(100))
        .expect("fits");
    ed.commit().expect("commits");
    let mut out = dc(&mut exec, 25);

    let never = Arc::new(AtomicUsize::new(0));
    let waiting = TestNode::counted(
        Kind::Gain {
            gain: 4.0,
            width: 1,
        },
        Arc::clone(&never),
    );
    ed.replace(NODE, Unforkable(waiting), fade(50))
        .expect("fits");
    ed.commit().expect("commits");
    out.extend(dc(&mut exec, 25));
    ed.replace(NODE, Unforkable(gain(8.0)), fade(50))
        .expect("fits");
    ed.commit().expect("commits");
    for _ in 0..8 {
        out.extend(dc(&mut exec, 25));
    }
    assert_eq!(
        never.load(Ordering::Relaxed),
        0,
        "a superseded unit never runs"
    );

    let expect: Vec<f32> = (0..out.len())
        .map(|f| match f {
            0..100 => {
                let (o, i) = FadeLaw::EqualAmplitude.gains(Samples::new(f), Samples::new(100));
                let (o, i) = (o.get() as f32, i.get() as f32);
                2.0 * i + 1.0 * o
            }
            100..150 => {
                let (o, i) = FadeLaw::EqualAmplitude.gains(Samples::new(f - 100), Samples::new(50));
                let (o, i) = (o.get() as f32, i.get() as f32);
                8.0 * i + 2.0 * o
            }
            _ => 8.0,
        })
        .collect();
    assert_eq!(out, expect);

    // Every unit that left came back to the control thread: the first
    // outgoing unit, the superseded one, and the second outgoing one.
    let mut back = ed.collect();
    back.sort();
    assert_eq!(back, vec![NODE; 3]);
    assert_eq!(ed.fades_in_flight(), 0, "every crossfade came back");
}

/// Counts the events it is handed; outputs the count.
struct EventCount(Arc<AtomicUsize>);

impl Node for EventCount {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.0.fetch_add(io.events(0).len(), Ordering::Relaxed);
        let n = self.0.load(Ordering::Relaxed) as f32;
        io.output(0).fill(n);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// During a fade, new events go to the incoming unit only: the outgoing one
/// finishes what it was sounding and hears nothing new.
#[test]
fn events_go_to_the_incoming_unit_only() {
    // Events arrive both ways: from an emitter's edge (one every 16 frames)
    // and as scheduled commands.
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let (old, new) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let emitter = NodeKey(2);
    ed.insert(
        emitter,
        "emit",
        Unforkable(TestNode::new(Kind::Emitter {
            period: 16,
            phase: 0,
        })),
    );
    ed.insert(NODE, "count", Unforkable(EventCount(Arc::clone(&old))));
    let to = EventIn {
        node: NODE,
        port: 0,
    };
    ed.spec_mut().connect_events(
        to,
        tutti_graph::EventEdge::Direct(tutti_graph::EventOut {
            node: emitter,
            port: 0,
        }),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![PortSource::Node(OutPort {
            node: NODE,
            port: 0,
        })]);
    ed.commit().expect("commits");
    let note = EventKind::Midi(midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100)));
    let mut out = vec![0.0f32; 64];
    let mut block = |exec: &mut Executor| {
        exec.process(64, &TransportPosition::default(), &[], &mut [&mut out[..]]);
    };
    ed.schedule(At::NextBlock, to, note).expect("room");
    block(&mut exec);
    assert_eq!(old.load(Ordering::Relaxed), 4 + 1);

    ed.replace(
        NODE,
        Unforkable(EventCount(Arc::clone(&new))),
        Fade::new(Samples::new(200)).law(FadeLaw::EqualPower),
    )
    .expect("same shape");
    ed.commit().expect("commits");
    for _ in 0..3 {
        ed.schedule(At::NextBlock, to, note).expect("room");
        block(&mut exec);
    }
    assert_eq!(
        old.load(Ordering::Relaxed),
        5,
        "the outgoing unit heard nothing new"
    );
    assert_eq!(new.load(Ordering::Relaxed), 3 * (4 + 1));
}

/// A hard edit at a fading key cuts the fade: removing the node, or an
/// `insert` over it, retires every unit there — the running one, the one
/// fading out and one waiting — and every one is reported by `collect`. A
/// re-prepare cuts it too, keeping the newest unit and reporting the two it
/// cut.
#[test]
fn a_hard_edit_cuts_a_fade() {
    let fade = Fade::new(Samples::new(1000));
    for cut in ["remove", "insert"] {
        let (mut ed, mut exec) = gain_graph(1.0);
        ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
        ed.commit().expect("commits");
        dc(&mut exec, 64);
        ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
        ed.commit().expect("commits");
        dc(&mut exec, 64);
        assert_eq!(ed.fades_in_flight(), 2, "one running, one waiting");
        match cut {
            "remove" => {
                ed.remove(NODE);
            }
            _ => {
                ed.insert(NODE, "gain", Unforkable(gain(8.0)));
            }
        }
        ed.commit().expect("commits");
        let out = dc(&mut exec, 64);
        let back = ed.collect();
        assert_eq!(ed.fades_in_flight(), 0, "{cut}: every crossfade came back");
        assert_eq!(back.len(), 3, "{cut}: three units retired: {back:?}");
        if cut == "insert" {
            assert!(out.iter().all(|&y| y == 8.0), "a swap, not a fade");
        }
    }

    let (mut ed, mut exec) = gain_graph(1.0);
    ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.reprepare(prepare(64)).expect("re-prepares");
    dc(&mut exec, 64);
    assert_eq!(
        ed.collect(),
        vec![NODE; 2],
        "the re-prepare's cut units are reported"
    );
    let out = dc(&mut exec, 64);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
    ed.collect();
    assert_eq!(ed.in_flight(), 0);
    assert_eq!(ed.fades_in_flight(), 0);
}

/// `set_latency` at a key that is mid-fade is a hard edit for the fade: the
/// next commit cuts it, keeping the newest unit (here the one waiting), and
/// the two units it retires — the one fading out and the one displaced —
/// are reported by `collect`. Both units of a fade must share the latency
/// the plan compensates, so a fade cannot run across the change.
#[test]
fn set_latency_cuts_a_fade() {
    let fade = Fade::new(Samples::new(1000));
    let (mut ed, mut exec) = gain_graph(1.0);
    ed.replace(NODE, Unforkable(gain(2.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    ed.replace(NODE, Unforkable(gain(4.0)), fade).expect("fits");
    ed.commit().expect("commits");
    dc(&mut exec, 64);
    assert!(ed.collect().is_empty());
    assert_eq!(ed.fades_in_flight(), 2, "one running, one waiting");

    ed.set_latency(NODE, tutti_units::Latency::new(Samples::new(3)))
        .expect("sets");
    ed.commit().expect("commits");
    let out = dc(&mut exec, 64);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
    assert_eq!(ed.collect(), vec![NODE; 2], "the cut units are reported");
    assert_eq!(ed.fades_in_flight(), 0);
}

/// The reference derives the same rule from the graph value alone: a node
/// whose declared latency changes with no new generation has its fade cut,
/// keeping the newest unit.
#[test]
fn the_reference_cuts_a_fade_on_a_latency_change() {
    use tutti_graph::NodeSpec;
    use tutti_graph::Topology;
    use tutti_graph::{GraphSpec, Reference};

    let spec_at = |generation: u32, latency: usize| {
        let mut t = Topology {
            inputs: ChannelLayout::MONO,
            ..Topology::default()
        };
        t.nodes.insert(
            NODE,
            NodeSpec::new("gain", ChannelLayout::MONO, ChannelLayout::MONO)
                .with_latency(Samples::new(latency)),
        );
        t.edges.insert(
            InPort {
                node: NODE,
                port: 0,
            },
            vec![Edge::Direct(PortSource::Global(0))],
        );
        t.set_outputs(vec![PortSource::Node(OutPort {
            node: NODE,
            port: 0,
        })]);
        let mut spec = GraphSpec::new(t);
        spec.generations.insert(NODE, generation);
        spec.validate().expect("valid")
    };
    let unit = |g: f32| -> BTreeMap<NodeKey, Box<dyn Node>> {
        [(NODE, Box::new(gain(g)) as Box<dyn Node>)]
            .into_iter()
            .collect()
    };
    let fade = Fade::new(Samples::new(1000));
    let fades: BTreeMap<NodeKey, Fade> = [(NODE, fade)].into_iter().collect();
    let mut r = Reference::new(prepare(128));
    let render = |r: &mut Reference| {
        let input = vec![1.0f32; 64];
        let mut out = vec![0.0f32; 64];
        r.process(
            64,
            &TransportPosition::default(),
            &[&input[..]],
            &mut [&mut out[..]],
        );
        out
    };
    r.set_graph(&spec_at(0, 0), unit(1.0));
    render(&mut r);
    r.set_graph_with_fades(&spec_at(1, 0), unit(2.0), &fades);
    render(&mut r);
    r.set_graph_with_fades(&spec_at(2, 0), unit(4.0), &fades);
    render(&mut r);
    r.set_graph_with_fades(&spec_at(2, 3), unit(8.0), &fades);
    let out = render(&mut r);
    assert!(
        out.iter().all(|&y| y == 4.0),
        "the newest unit: {:?}",
        &out[..4]
    );
}

/// **A replace gives the key the incoming unit's own kind**, not the kind
/// the key was inserted under: a key that held one type of node and now holds
/// another must not name the old type in the spec (the kind is what
/// diagnostics print), whether the replace fades or swaps.
#[test]
fn a_replace_takes_the_incoming_units_kind() {
    // The shape that does not fit is placed by a cut: a fade over it is refused.
    let faded = Fade::new(Samples::new(100));
    let cut = Fade::new(Samples::ZERO);
    for (incoming, fade) in [
        (gain(3.0), faded),
        (TestNode::new(Kind::Lag { latency: 3 }), cut),
    ] {
        let (mut ed, _exec) = gain_graph(1.0);
        assert_eq!(ed.spec().topology.nodes[&NODE].kind, "gain");
        ed.replace_or_swap(NODE, Unforkable(incoming), fade)
            .expect("not poisoned, not re-preparing");
        assert_eq!(
            ed.spec().topology.nodes[&NODE].kind,
            std::any::type_name::<TestNode>(),
            "the spec names the unit that runs at the key"
        );
    }
}

/// **`replace_or_swap` fades where `replace` would, and swaps only a cut or
/// a node that is not running**: a unit that fits the running one fades in;
/// one that does not (here: a latency the plan was not compiled for) is
/// refused under a fade and lands whole on the next commit under a zero fade,
/// as an insert at the key does; so does a replace of a node that is not
/// running yet.
#[test]
fn replace_or_swap_fades_when_it_fits_and_swaps_when_it_does_not() {
    let fade = Fade::new(Samples::new(100));

    let (mut ed, mut exec) = gain_graph(1.0);
    dc(&mut exec, 20);
    ed.replace_or_swap(NODE, Unforkable(gain(3.0)), fade)
        .expect("not poisoned, not re-preparing");
    ed.commit().expect("commits");
    assert_eq!(ed.fades_in_flight(), 1, "a fitting unit fades");
    let out = dc(&mut exec, 10);
    assert!(
        out[0] > 1.0 && out[0] < 3.0,
        "the first frame after the commit is a blend, got {}",
        out[0]
    );

    let (mut ed, mut exec) = gain_graph(1.0);
    dc(&mut exec, 20);
    let lagged = || Unforkable(TestNode::new(Kind::Lag { latency: 3 }));
    assert!(
        ed.replace_or_swap(NODE, lagged(), fade)
            .is_err_and(|e| matches!(e, CommitError::FadeShape { node, .. } if node == NODE)),
        "a fade over a shape change is refused"
    );
    assert_eq!(
        ed.spec().topology.nodes[&NODE].kind,
        "gain",
        "a refusal changes nothing"
    );
    ed.replace_or_swap(NODE, lagged(), Fade::new(Samples::ZERO))
        .expect("a cut over a shape change is a swap");
    ed.commit().expect("commits");
    assert_eq!(ed.fades_in_flight(), 0, "nothing to fade from");
    let out = dc(&mut exec, 10);
    assert_eq!(
        &out[..4],
        &[0.0, 0.0, 0.0, 1.0],
        "the lag alone from the first frame: a swap, not a blend with the gain"
    );

    // Not running yet: inserted and replaced before any commit.
    let (mut ed, _exec) = Editor::new(prepare(128));
    ed.insert(NodeKey(5), "gain", Unforkable(gain(1.0)));
    ed.replace_or_swap(NodeKey(5), Unforkable(gain(2.0)), fade)
        .expect("swaps");
    ed.commit().expect("commits");
    assert_eq!(ed.fades_in_flight(), 0);
    assert_eq!(
        ed.spec().topology.nodes[&NodeKey(5)].kind,
        std::any::type_name::<TestNode>(),
        "the swapped-in unit's kind (a replace names the unit that runs)"
    );
}

/// **`remove` cuts a node that `remove_declicked` was fading out**: the node
/// leaves the spec at once, with no leaving mark left behind.
#[test]
fn a_remove_after_a_declicked_remove_cuts_the_leaving_node() {
    let (mut ed, mut exec) = gain_graph(1.0);
    dc(&mut exec, 20);
    assert!(!ed.remove_declicked(NODE), "its output edge fades out");
    assert!(ed.is_leaving(NODE));
    assert!(ed.spec().topology.nodes.contains_key(&NODE));
    ed.remove(NODE);
    assert!(!ed.is_leaving(NODE), "cut, not fading");
    assert!(!ed.spec().topology.nodes.contains_key(&NODE));
    ed.commit().expect("commits");
    exec.apply_pending();
    assert_eq!(ed.collect(), vec![NODE], "the unit came back");
}

/// **`fade_fits` answers what `replace_or_swap` does**: true for a shape that
/// fits (the swap fades), false for another (a fade is refused), `None` for a
/// node that is not there.
#[test]
fn fade_fits_agrees_with_replace_or_swap() {
    use tutti_graph::Node;
    let fade = Fade::new(Samples::new(100));
    let (mut ed, mut exec) = gain_graph(1.0);
    dc(&mut exec, 20);
    let fitting = gain(3.0);
    let other = TestNode::new(Kind::Lag { latency: 3 });
    assert_eq!(ed.fade_fits(NODE, &fitting.shape()), Some(true));
    assert_eq!(ed.fade_fits(NODE, &other.shape()), Some(false));
    assert_eq!(ed.fade_fits(NodeKey(9), &fitting.shape()), None);
    assert!(ed.replace_or_swap(NODE, Unforkable(other), fade).is_err());
    ed.replace_or_swap(NODE, Unforkable(fitting), fade)
        .expect("a fitting shape fades");
    ed.commit().expect("commits");
    assert_eq!(ed.fades_in_flight(), 1);
}
