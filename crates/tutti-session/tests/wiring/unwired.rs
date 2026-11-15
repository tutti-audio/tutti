//! A node re-inserted with a narrower shape (a plugin whose ports changed,
//! a node re-declared at a new width): the wiring to the ports it lost is
//! dropped and reported once, by the update that commits the re-insert, in
//! the app's own port values; the wiring to the ports it kept still carries
//! signal; and the commit is not refused.
//!
//! Figures are analytic: constants of exactly representable levels summed at
//! a known gain.

use tutti_buffers::InterleavedMut;
use tutti_graph::{Cx, FadeLaw, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{Edge, EventEdge, InPort, OutPort, Root, Session, SessionEvent};
use tutti_types::{Amplitude, ChannelLayout, Seconds};

const BLOCK: usize = 64;
/// The sources' levels, and the gain of the wire that survives.
const A: f32 = 0.25;
const B: f32 = 0.5;
const GAIN: f32 = 0.5;

/// `W` channels copied through, with `EV` event inputs and outputs.
#[derive(Clone)]
struct Pass<const W: u16, const EV: u16>;

impl<const W: u16, const EV: u16> Node for Pass<W, EV> {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::from_count(W)).with_events(EV, EV)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..usize::from(W) {
            outs.get(c).copy_from_slice(ins.get(c));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// No audio, `IN` event inputs and `OUT` event outputs.
#[derive(Clone)]
struct Ports<const IN: u16, const OUT: u16>;

impl<const IN: u16, const OUT: u16> Node for Ports<IN, OUT> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(IN, OUT)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Commits and renders two blocks, so a hard wire has settled; returns the
/// last block's channel `ch`.
fn channel(s: &mut Session, ch: usize) -> Vec<f32> {
    let mut block = vec![f32::NAN; BLOCK * 2];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
    block.iter().skip(ch).step_by(2).copied().collect()
}

fn unwired(events: &[SessionEvent]) -> Vec<(NodeKey, Vec<Edge>, Vec<EventEdge>)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Unwired {
                node,
                edges,
                event_edges,
            } => Some((*node, edges.clone(), event_edges.clone())),
            _ => None,
        })
        .collect()
}

fn refused(events: &[SessionEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, SessionEvent::CommitRefused(_)))
}

/// **A narrower re-insert reports exactly the wiring it dropped, once, and
/// keeps the rest working.** `x` is two channels wide with two event ports
/// each way; its channel 0 carries `A` at gain `GAIN` to output 0 and its
/// channel 1 carries `B` to output 1. Re-inserted one channel and one event
/// port wide, the edges on channel 1 and event port 1 are the dropped ones,
/// output 0 still hears `GAIN * A`, output 1 hears nothing, and the commit
/// goes through.
#[test]
fn a_narrower_reinsert_reports_what_it_dropped_and_keeps_the_rest() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let (a, _) = g.add(ForkByClone(Const::mono(A)));
    let (b, _) = g.add(ForkByClone(Const::mono(B)));
    let (x, _) = g.add(ForkByClone(Pass::<2, 2>));
    let (src, _) = g.add(ForkByClone(Ports::<0, 2>));
    let (sink, _) = g.add(ForkByClone(Ports::<2, 0>));
    for (from, to) in [
        (OutPort::new(a, 0), x.input(0)),
        (OutPort::new(b, 0), x.input(1)),
        (x.out(0), Root::output(0)),
        (x.out(1), Root::output(1)),
    ] {
        g.connect(from, to).unwrap().hard();
    }
    for (from, to) in [
        (src.events_out_at(0), x.events_in_at(0)),
        (src.events_out_at(1), x.events_in_at(1)),
        (x.events_out_at(0), sink.events_in_at(0)),
        (x.events_out_at(1), sink.events_in_at(1)),
    ] {
        g.connect(from, to).unwrap();
    }
    let kept = Edge::new(OutPort::new(a, 0), x.input(0));
    kept.set_gain(g, Amplitude::new(GAIN)).expect("an edge");
    let events = s.update();
    assert!(!refused(&events));
    assert!(unwired(&events).is_empty(), "nothing was dropped yet");
    assert!(
        channel(&mut s, 0).iter().all(|&v| v == GAIN * A),
        "output 0 hears A at its gain before the re-insert"
    );
    assert!(channel(&mut s, 1).iter().all(|&v| v == B));

    s.graph()
        .replace(
            x,
            ForkByClone(Pass::<1, 1>),
            Seconds(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(!refused(&events), "the commit is not refused");
    let [(node, mut edges, mut event_edges)] = <[_; 1]>::try_from(unwired(&events))
        .unwrap_or_else(|v| panic!("one Unwired per re-inserted key, got {v:?}"));
    assert_eq!(node, x.erase());
    edges.sort();
    event_edges.sort();
    let mut want_edges = vec![
        Edge::new(OutPort::new(b, 0), x.input(1)),
        Edge::new(x.out(1), Root::output(1)),
    ];
    want_edges.sort();
    let mut want_events = vec![
        EventEdge::new(src.events_out_at(1), x.events_in_at(1)),
        EventEdge::new(x.events_out_at(1), sink.events_in_at(1)),
    ];
    want_events.sort();
    assert_eq!(edges, want_edges);
    assert_eq!(event_edges, want_events);

    assert!(
        channel(&mut s, 0).iter().all(|&v| v == GAIN * A),
        "the surviving wire still carries A at its gain"
    );
    assert!(
        channel(&mut s, 1).iter().all(|&v| v == 0.0),
        "output 1 lost its source"
    );
    assert_eq!(
        kept.gain(s.graph()),
        Some(Amplitude::new(GAIN)),
        "the surviving edge keeps its gain"
    );
    assert!(
        s.graph().sources(InPort::new(x, 0)).is_ok(),
        "the kept port is still wired"
    );
    assert_eq!(
        s.graph().sources(x.events_in_at(0)).unwrap(),
        [src.events_out_at(0)]
    );

    // Reported once.
    let events = s.update();
    assert!(unwired(&events).is_empty());
    assert!(!refused(&events));
}

/// **A wider re-insert, and a re-insert at the same width, report nothing.**
#[test]
fn a_wider_reinsert_reports_nothing() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let (a, _) = g.add(ForkByClone(Const::mono(A)));
    let (x, _) = g.add(ForkByClone(Pass::<1, 1>));
    g.connect(OutPort::new(a, 0), x.input(0)).unwrap().hard();
    g.connect(x.out(0), Root::output(0)).unwrap().hard();
    let _ = s.update();
    s.graph()
        .replace(
            x,
            ForkByClone(Pass::<1, 1>),
            Seconds(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(unwired(&events).is_empty(), "same width");
    assert!(!refused(&events));
    s.graph()
        .replace(
            x,
            ForkByClone(Pass::<2, 2>),
            Seconds(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(unwired(&events).is_empty(), "wider");
    assert!(!refused(&events));
    assert!(channel(&mut s, 0).iter().all(|&v| v == A));
}
