//! `Graph::connect` and `Graph::chain`: a whole main bus per call, adding
//! sources and refusing a width change.
//!
//! Every figure is analytic: constants of exactly representable values.

use crate::offline::offline;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{ConnectError, MainOutput, OutputPort, Session};

/// The first frame of a committed 64-frame block, per channel.
fn first_frame(s: &mut Session) -> [f32; 2] {
    let _ = s.update();
    let mut block = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    [block[0], block[1]]
}

fn feed_main_output(s: &mut Session, node: NodeKey) {
    for c in 0..2 {
        s.graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(node, c)])
            .unwrap()
            .unfaded();
    }
}

/// **`connect` wires every channel**: a stereo constant of (0.25, 0.5) through
/// a stereo pass-through is heard as (0.25, 0.5), with one call and no
/// per-channel wiring.
#[test]
fn connect_wires_each_channel_to_its_own() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.5])));
    let through = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    s.graph()
        .connect(src, through)
        .expect("both are stereo")
        .unfaded();
    feed_main_output(&mut s, through.erase());
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);
}

/// **`connect` along a path wires each node to the next**: source, two
/// pass-throughs.
#[test]
fn connect_wires_each_node_to_the_next() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.5])));
    let a = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let b = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    s.graph().connect(src, a).expect("all stereo");
    s.graph().connect(a, b).expect("all stereo");
    feed_main_output(&mut s, b.erase());
    // The links fade in over 256 frames: the fifth 64-frame block
    // starts past it.
    for _ in 0..4 {
        first_frame(&mut s);
    }
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);
}

/// **`connect` adds**: a second connect into the same node sums with the
/// first; it replaces nothing.
#[test]
fn connect_adds_to_what_feeds_the_input() {
    let mut s = offline();
    let one = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.125])));
    let two = s.graph().add(ForkByClone(Const::frame(&[0.5, 0.25])));
    let sink = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    s.graph().connect(one, sink).unwrap().unfaded();
    s.graph().connect(two, sink).unwrap().unfaded();
    feed_main_output(&mut s, sink.erase());
    assert_eq!(first_frame(&mut s), [0.75, 0.375]);
}

/// **Mismatched main-bus widths are refused, and nothing is stored**: a mono
/// source into a stereo input is `ConnectError::Width`, never a truncation or
/// a zero fill; a connect refuses at its bad link and wires nothing.
#[test]
fn a_width_mismatch_is_refused_and_stores_nothing() {
    let mut s = offline();
    let mono = s.graph().add(ForkByClone(Const::mono(0.5)));
    let stereo = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let more = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let narrow = s.graph().add(ForkByClone(Through::mono()));
    let _ = s.update();
    let err = s.graph().connect(mono, stereo).unwrap_err();
    assert_eq!(
        err,
        ConnectError::Width {
            from: mono.erase(),
            from_width: 1,
            to: stereo.erase(),
            to_width: 2
        }
    );
    assert_eq!(
        s.graph().connect(more, narrow).unwrap_err(),
        ConnectError::Width {
            from: more.erase(),
            from_width: 2,
            to: narrow.erase(),
            to_width: 1
        }
    );
    assert!(!s.graph().has_uncommitted_edits(), "nothing was wired");
}

/// **A node cannot feed itself, and an absent node is named.**
#[test]
fn a_node_that_is_not_there_or_feeds_itself_is_refused() {
    let mut s = offline();
    let a = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    assert_eq!(
        s.graph().connect(a, a).err(),
        Some(ConnectError::Itself(a.erase()))
    );
    let gone = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    s.remove(gone).unwrap();
    assert_eq!(
        s.graph().connect(a, gone).err(),
        Some(ConnectError::NoSuchNode(gone.erase()))
    );
}

/// **`topology` shows a wiring call at once**: before any commit, the edge a
/// `connect` wrote is the source's output port at the sink's input port, and
/// an unwired port has none.
#[test]
fn topology_shows_the_edge_a_connect_wrote() {
    use tutti_graph::{Edge, InputPort, OutputPort as RawOut, PortSource};

    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.5])));
    let through = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    assert!(
        s.graph().testing().topology().edges.is_empty(),
        "nothing feeds the pass-through yet"
    );
    s.graph().connect(src, through).expect("both are stereo");
    for c in 0..2 {
        assert_eq!(
            s.graph()
                .testing()
                .topology()
                .edges
                .get(&InputPort::new(through.erase(), c)),
            Some(&vec![Edge::Direct(PortSource::Node(RawOut::new(
                src.erase(),
                c
            )))]),
            "channel {c} is fed by the source's channel {c}"
        );
    }
    assert!(s.graph().has_uncommitted_edits(), "and no commit has run");
}
