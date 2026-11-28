//! A port holds a set of sources and the graph sums it (doc 013, "Audio
//! fan-in sums", point 7): `sources`, `set_sources`, `add_source` and
//! `remove_source` on a node's input, the same four on a main output
//! channel, and the output tap that hears a summed channel whole.
//!
//! Every figure is analytic: constants of exactly representable values, whose
//! sum is exact in `f32`.

use crate::offline::offline;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{InputPort, MainOutput, OutputPort, Session};
use tutti_units::Samples;

const A: f32 = 0.25;
const B: f32 = 0.5;
const C: f32 = 0.125;

/// Commits and renders one 64-frame block; returns output channel 0.
fn left(s: &mut Session) -> Vec<f32> {
    let _ = s.update();
    let mut block = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    block.iter().step_by(2).copied().collect()
}

/// A session with constants `a`, `b`, `c` (`A`, `B`, `C`) and a pass-through
/// `sink` feeding output channel 0, committed once so `has_uncommitted_edits` is false.
struct Rig {
    s: Session,
    a: tutti_graph::NodeKey,
    b: tutti_graph::NodeKey,
    c: tutti_graph::NodeKey,
    sink: tutti_graph::NodeKey,
}

fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(A)));
    let b = g.add(ForkByClone(Const::mono(B)));
    let c = g.add(ForkByClone(Const::mono(C)));
    let sink = g.add(ForkByClone(Through::mono()));
    g.set_sources(MainOutput::channel(0), [OutputPort::new(sink, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
    Rig {
        s,
        a: a.erase(),
        b: b.erase(),
        c: c.erase(),
        sink: sink.erase(),
    }
}

fn node(k: tutti_graph::NodeKey) -> OutputPort {
    OutputPort::new(k, 0)
}

/// A second source on a port is a second addend, in source order.
#[test]
fn add_source_adds_an_addend() {
    let mut r = rig();
    r.s.graph()
        .connect(node(r.b), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.a), node(r.b)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
}

/// Removing a source leaves the others, down to one (aliased) and none
/// (silence).
#[test]
fn remove_source_goes_back_to_one() {
    let mut r = rig();
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    r.s.graph()
        .connect(node(r.b), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
    r.s.graph()
        .disconnect(node(r.b), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.a)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A));
    r.s.graph()
        .disconnect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(
        r.s.graph_ref()
            .sources(InputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    assert!(left(&mut r.s).iter().all(|&x| x == 0.0));
}

/// `set_sources` means exactly these, whatever order they are listed in, and
/// `set_source` means exactly one.
#[test]
fn set_sources_replaces_the_set() {
    let mut r = rig();
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    r.s.graph()
        .connect(node(r.b), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.c)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.c)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == C));

    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.b), node(r.a)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.a), node(r.b)],
        "listed in any order, held in the graph's"
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));

    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.c)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.c)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == C));
}

/// A source listed twice is one source (not a weight of 2), and adding one
/// the port holds changes nothing.
#[test]
fn a_source_twice_is_one_source() {
    let mut r = rig();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.a), node(r.a)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.a)]
    );
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(InputPort::new(r.sink, 0)).unwrap(),
        [node(r.a)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A));
}

/// A change to nothing marks nothing edited, so nothing recompiles; a real
/// change does.
#[test]
fn an_unchanged_set_marks_nothing_edited() {
    let mut r = rig();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.a), node(r.b)])
        .unwrap()
        .unfaded();
    assert!(r.s.graph_ref().has_uncommitted_edits(), "a change");
    let _ = r.s.update();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.b), node(r.a)])
        .unwrap()
        .unfaded();
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "the same set, reordered"
    );
    r.s.graph()
        .connect(node(r.a), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "a source it holds"
    );
    r.s.graph()
        .disconnect(node(r.c), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "a source it lacks"
    );
    r.s.graph()
        .connect(node(r.c), InputPort::new(r.sink, 0))
        .unwrap()
        .unfaded();
    assert!(r.s.graph_ref().has_uncommitted_edits(), "a new source");
}

/// Silence is the empty set: an empty `set_sources` clears
/// the port, and nothing stores a `PortSource::Zero`.
#[test]
fn silence_is_the_empty_set() {
    let mut r = rig();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.a), node(r.b)])
        .unwrap()
        .unfaded();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [])
        .unwrap()
        .unfaded();
    assert!(
        r.s.graph_ref()
            .sources(InputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [node(r.a)])
        .unwrap()
        .unfaded();
    r.s.graph()
        .set_sources(InputPort::new(r.sink, 0), [])
        .unwrap()
        .unfaded();
    assert!(
        r.s.graph_ref()
            .sources(InputPort::new(r.sink, 0))
            .unwrap()
            .is_empty()
    );
    assert!(left(&mut r.s).iter().all(|&x| x == 0.0));
}

#[test]
fn a_node_cannot_be_its_own_source() {
    let mut r = rig();
    assert_eq!(
        r.s.graph()
            .connect(node(r.sink), InputPort::new(r.sink, 0))
            .err(),
        Some(tutti_session::ConnectError::Itself(r.sink))
    );
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "nothing was stored"
    );
}

/// A main output channel is a sink too: its sources sum.
#[test]
fn an_output_channel_sums_its_sources() {
    let mut r = rig();
    r.s.graph()
        .set_sources(MainOutput::channel(0), [node(r.b)])
        .unwrap()
        .unfaded();
    r.s.graph()
        .connect(node(r.a), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
        [node(r.a), node(r.b)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));

    r.s.graph()
        .disconnect(node(r.b), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
        [node(r.a)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A));

    r.s.graph()
        .set_sources(MainOutput::channel(0), [node(r.c), node(r.a)])
        .unwrap()
        .unfaded();
    assert!(left(&mut r.s).iter().all(|&x| x == A + C));
    r.s.graph()
        .set_sources(MainOutput::channel(0), [node(r.c)])
        .unwrap()
        .unfaded();
    assert_eq!(
        r.s.graph_ref().sources(MainOutput::channel(0)).unwrap(),
        [node(r.c)]
    );
    assert!(left(&mut r.s).iter().all(|&x| x == C));
}

/// The set-setters of an output mark nothing edited when nothing changes.
#[test]
fn an_unchanged_output_set_marks_nothing_edited() {
    let mut r = rig();
    r.s.graph()
        .set_sources(MainOutput::channel(0), [node(r.sink)])
        .unwrap()
        .unfaded();
    assert!(!r.s.graph_ref().has_uncommitted_edits());
    r.s.graph()
        .connect(node(r.sink), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    assert!(!r.s.graph_ref().has_uncommitted_edits());
    r.s.graph()
        .disconnect(node(r.a), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    assert!(!r.s.graph_ref().has_uncommitted_edits());
}

/// The output tap hears a fan-in channel whole, before and after the fan-in
/// is wired, and follows it as it changes.
#[test]
fn the_output_tap_hears_the_sum() {
    let mut r = rig();
    r.s.graph()
        .set_sources(MainOutput::channel(0), [node(r.a), node(r.b)])
        .unwrap()
        .unfaded();
    let mut tap =
        r.s.graph()
            .taps()
            .open_main_output(Samples::new(1_024))
            .expect("the sources exist");
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
    r.s.graph()
        .connect(node(r.c), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    assert!(left(&mut r.s).iter().all(|&x| x == A + B + C));

    let mut got = vec![f32::NAN; 128 * 2];
    assert_eq!(
        tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap())
            .unwrap(),
        Samples::new(128)
    );
    let lane: Vec<f32> = got.iter().step_by(2).copied().collect();
    assert!(lane[..64].iter().all(|&x| x == A + B), "{lane:?}");
    assert!(lane[64..].iter().all(|&x| x == A + B + C), "{lane:?}");
    assert!(got.iter().skip(1).step_by(2).all(|&x| x == 0.0));
}
