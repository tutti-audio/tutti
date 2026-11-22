//! A modulation route onto the gain of an edge that a narrower re-insert
//! dropped: the route stays declared and reads unresolved by the rule a
//! target not in the graph already has, and the graph keeps committing.

use crate::common;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{Fade, ForkByClone};
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    ChannelEdge, MainOutput, ModTarget, OutputPort, Problem, Replacement, RouteSpec, Session,
    SessionEvent, TargetName,
};
use tutti_units::{Gain, Depth, Samples};

use common::probe::Pulse;

const BLOCK: usize = 128;

fn settle(s: &mut Session) -> Vec<SessionEvent> {
    let mut all = Vec::new();
    for _ in 0..6 {
        all.extend(s.update());
    }
    all
}

fn render(s: &mut Session) -> Vec<f32> {
    let mut buf = vec![0.0f32; BLOCK * 2];
    for _ in 0..12 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    }
    buf.as_chunks::<2>().0.iter().map(|f| f[1]).collect()
}

/// **A route onto a dropped edge is unresolved, never an error.** A stereo
/// pass-through carries a pulse on its second channel to output 1 under a
/// routed gain. Re-inserted as a mono pass-through, that edge is gone: the
/// route reads as an unresolved target of the edge, no problem is a refusal,
/// no commit is refused, and the other output is untouched.
#[test]
fn a_route_onto_a_dropped_edge_reads_unresolved() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(0.25)));
    let p = g.add(ForkByClone(Pulse {
        from: 0,
        to: u64::MAX,
        level: 0.5,
    }));
    let x = g.add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    g.connect(OutputPort::new(a, 0), x.input(0))
        .unwrap()
        .unfaded();
    g.connect(OutputPort::new(p, 0), x.input(1))
        .unwrap()
        .unfaded();
    g.connect(x.output(0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    g.connect(x.output(1), MainOutput::channel(1))
        .unwrap()
        .unfaded();
    let lost = ChannelEdge::new(OutputPort::new(p, 0), x.input(1));
    let mut m = s.mods_mut();
    m.range(lost, Gain::new(-2.0)..Gain::new(2.0))
        .unwrap();
    let source = m.macro_(0.5);
    m.route(RouteSpec::new(source, lost, Depth::of(0.25)))
        .unwrap();
    settle(&mut s);
    assert_eq!(s.mod_state().problems().next(), None, "the route applies");
    assert!(
        render(&mut s).iter().skip(512).all(|&v| v != 0.0),
        "the routed edge carries the pulse"
    );

    s.graph()
        .replace(
            x,
            Replacement::new(ForkByClone(Through::mono())).fade(Fade::new(Samples::new(0))),
        )
        .expect("the node is there");
    let events = settle(&mut s);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::CommitRefused(_))),
        "the commit is not refused: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::Unwired { edges, .. } if edges.contains(&lost)
    )));
    let problems: Vec<Problem> = s.mod_state().problems().cloned().collect();
    assert_eq!(problems.len(), 1, "one problem, the target: {problems:?}");
    assert!(
        matches!(
            &problems[0],
            Problem::Unresolved {
                target: TargetName::Own(ModTarget::Edge(e)),
                ..
            } if *e == lost
        ),
        "the dropped edge's route is unresolved: {problems:?}"
    );
    assert!(
        render(&mut s).iter().all(|&v| v == 0.0),
        "output 1 lost its source"
    );
}
