//! A node re-inserted with a narrower shape (a plugin whose ports changed,
//! a node re-declared at a new width): the wiring to the ports it lost is
//! dropped and reported once, by the update that commits the re-insert, in
//! the app's own port values; the wiring to the ports it kept still carries
//! signal; and the commit is not refused.
//!
//! Figures are analytic: constants of exactly representable levels summed at
//! a known gain.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology, Speaker};
use tutti_graph::{
    Bus, BusRole, BusSource, Buses, Cx, FadeLaw, ForkByClone, Io, Node, NodeKey, Prepare, Shape,
    Status,
};
use tutti_nodes::testing::Const;
use tutti_session::{
    BusSink, Edge, EventEdge, Feeder, InPort, OutPort, Root, Session, SessionEvent,
};
use tutti_units::{Amplitude, Seconds};

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
                ..
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
    let a = g.add(ForkByClone(Const::mono(A)));
    let b = g.add(ForkByClone(Const::mono(B)));
    let x = g.add(ForkByClone(Pass::<2, 2>));
    let src = g.add(ForkByClone(Ports::<0, 2>));
    let sink = g.add(ForkByClone(Ports::<2, 0>));
    for (from, to) in [
        (OutPort::new(a, 0), x.input(0)),
        (OutPort::new(b, 0), x.input(1)),
        (x.out(0), Root::output(0)),
        (x.out(1), Root::output(1)),
    ] {
        g.connect(from, to).unwrap().unfaded();
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
    // The gain lane is the wire's declaration: the connection is made again
    // with it (the edge is held already, so only its gain is set).
    g.connect(kept.source(), kept.sink())
        .unwrap()
        .gain(Amplitude::new(f64::from(GAIN)))
        .unwrap();
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
            Seconds::new(0.0),
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
    let gain = s
        .graph_ref()
        .gain_target(kept.source(), kept.sink())
        .expect("a lane");
    assert_eq!(
        s.get(gain).unwrap(),
        Amplitude::new(f64::from(GAIN)),
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
    let a = g.add(ForkByClone(Const::mono(A)));
    let x = g.add(ForkByClone(Pass::<1, 1>));
    g.connect(OutPort::new(a, 0), x.input(0)).unwrap().unfaded();
    g.connect(x.out(0), Root::output(0)).unwrap().unfaded();
    let events = s.update();
    assert!(!refused(&events));
    s.graph()
        .replace(
            x,
            ForkByClone(Pass::<1, 1>),
            Seconds::new(0.0),
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
            Seconds::new(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(unwired(&events).is_empty(), "wider");
    assert!(!refused(&events));
    assert!(channel(&mut s, 0).iter().all(|&v| v == A));
}

/// **A feed naming a lost event output is not written back.** Two feeders feed
/// one sink from two outputs of one source. The source is re-inserted with
/// one output: the lost output's edge is reported, and a later `set_feed` by
/// a third feeder on the same sink commits (the lost source is not rewritten
/// from the first feeder's entry) and leaves exactly the edges whose ports
/// exist.
#[test]
fn a_feed_naming_a_lost_event_output_is_not_written_back() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let src = g.add(ForkByClone(Ports::<0, 2>));
    let other = g.add(ForkByClone(Ports::<0, 1>));
    let sink = g.add(ForkByClone(Ports::<1, 0>));
    g.set_feed(sink, Feeder::Sequencer, vec![src.events_out_at(0)]);
    g.set_feed(sink, Feeder::Routes, vec![src.events_out_at(1)]);
    let events = s.update();
    assert!(!refused(&events));

    s.graph()
        .replace(
            src,
            ForkByClone(Ports::<0, 1>),
            Seconds::new(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(!refused(&events));
    let [(_, _, event_edges)] = <[_; 1]>::try_from(unwired(&events)).expect("one Unwired");
    assert_eq!(
        event_edges,
        [EventEdge::new(src.events_out_at(1), sink.events_in())]
    );

    s.graph()
        .set_feed(sink, Feeder::LiveInput, vec![other.events_out_at(0)]);
    let events = s.update();
    assert!(
        !refused(&events),
        "the lost output is not written back: {events:?}"
    );
    assert_eq!(
        s.graph().sources(sink.events_in()).unwrap(),
        [src.events_out_at(0), other.events_out_at(0)],
        "the feeds whose ports exist still deliver"
    );
}

/// A node with a main stereo bus, and with `aux` a second stereo bus, on each side.
#[derive(Clone)]
struct Bused {
    aux: bool,
}

impl Node for Bused {
    fn shape(&self) -> Shape {
        let stereo = BusTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
        let side = |role| {
            let main = Buses::NONE.with(Bus::main(stereo)).unwrap();
            if self.aux {
                main.with(Bus::new(stereo, role)).unwrap()
            } else {
                main
            }
        };
        Shape::buses(side(BusRole::Sidechain), side(BusRole::Aux))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// **A bus edge into or out of a lost bus is reported as the app wired it.**
#[test]
fn a_narrower_reinsert_reports_the_bus_edges_it_dropped() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let a = g.add(ForkByClone(Bused { aux: true }));
    let y = g.add(ForkByClone(Bused { aux: true }));
    let t = g.add(ForkByClone(Bused { aux: true }));
    g.connect(a.bus_out(0), y.bus_in(0)).unwrap().unfaded();
    g.connect(a.bus_out(1), y.bus_in(1)).unwrap().unfaded();
    g.connect(y.bus_out(0), t.bus_in(0)).unwrap().unfaded();
    g.connect(y.bus_out(1), t.bus_in(1)).unwrap().unfaded();
    let events = s.update();
    assert!(!refused(&events), "{events:?}");

    s.graph()
        .replace(
            y,
            ForkByClone(Bused { aux: false }),
            Seconds::new(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(!refused(&events), "{events:?}");
    let [bus_edges] = <[_; 1]>::try_from(
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Unwired { bus_edges, .. } => Some(bus_edges.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
    )
    .expect("one Unwired");
    let mut want: Vec<(BusSource, BusSink)> = vec![
        (a.bus_out(1), BusSink::Node(y.bus_in(1))),
        (y.bus_out(1), BusSink::Node(t.bus_in(1))),
    ];
    assert_eq!(bus_edges.len(), want.len());
    for w in want.drain(..) {
        assert!(bus_edges.contains(&w), "{w:?} in {bus_edges:?}");
    }
}

/// **A root feed from a lost bus is reported as `(source, Root)`.**
#[test]
fn a_narrower_reinsert_reports_the_root_feed_it_dropped() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let y = g.add(ForkByClone(Bused { aux: true }));
    s.declare_root_topology(Some(ChannelTopology::default_for(ChannelLayout::STEREO)))
        .expect("stereo");
    let g = s.graph();
    g.connect(y.bus_out(0), Root).unwrap().unfaded();
    g.connect(y.bus_out(1), Root).unwrap().unfaded();
    let events = s.update();
    assert!(!refused(&events), "{events:?}");

    s.graph()
        .replace(
            y,
            ForkByClone(Bused { aux: false }),
            Seconds::new(0.0),
            FadeLaw::EqualAmplitude,
        )
        .expect("the node is there");
    let events = s.update();
    assert!(!refused(&events), "{events:?}");
    let reported: Vec<Vec<(BusSource, BusSink)>> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Unwired { bus_edges, .. } => Some(bus_edges.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reported, [vec![(y.bus_out(1), BusSink::Root)]]);
}

/// **A commit that is refused reports nothing, and the record waits for the
/// commit that lands.** After a narrowing re-insert a cycle makes every commit
/// fail: the updates report the refusal and no `Unwired`; once the cycle is
/// cut, the update that lands the commit reports the dropped edge, once.
#[test]
fn unwired_waits_for_the_commit_that_lands() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(A)));
    let x = g.add(ForkByClone(Pass::<2, 1>));
    let u = g.add(ForkByClone(Pass::<1, 1>));
    let v = g.add(ForkByClone(Pass::<1, 1>));
    g.connect(OutPort::new(a, 0), x.input(1)).unwrap().unfaded();
    let _ = s.update();

    let g = s.graph();
    g.replace(
        x,
        ForkByClone(Pass::<1, 1>),
        Seconds::new(0.0),
        FadeLaw::EqualAmplitude,
    )
    .expect("the node is there");
    g.connect(u.out(0), v.input(0)).unwrap().unfaded();
    g.connect(v.out(0), u.input(0)).unwrap().unfaded();
    let events = s.update();
    assert!(refused(&events), "the cycle is refused: {events:?}");
    assert!(unwired(&events).is_empty(), "nothing landed yet");
    let events = s.update();
    assert!(unwired(&events).is_empty(), "still refused, still nothing");

    assert!(
        s.graph()
            .disconnect(v.out(0), u.input(0))
            .unwrap()
            .was_connected()
    );
    let events = s.update();
    assert!(!refused(&events), "{events:?}");
    let [(node, edges, _)] = <[_; 1]>::try_from(unwired(&events)).expect("reported once");
    assert_eq!(node, x.erase());
    assert_eq!(edges, [Edge::new(OutPort::new(a, 0), x.input(1))]);
    assert!(unwired(&s.update()).is_empty());
}
