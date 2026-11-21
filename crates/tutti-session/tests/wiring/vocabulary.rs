//! The wiring vocabulary on typed keys and ports (doc 013, "The session
//! graph's surface"): what `connect` takes and refuses, `Root`, a connection's
//! gain, and `disconnect`.
//!
//! Every figure is analytic: constants of exactly representable values.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{FoldError, InterleavedMut};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    ConnectError, DeclarationError, Edge, GainTargetError, OutPort, Root, Session,
};
use tutti_units::Amplitude;

fn offline() -> Session {
    Session::builder().build().expect("builds")
}

/// The first frame of a committed 64-frame block, per channel.
fn first_frame(s: &mut Session) -> [f32; 2] {
    let _ = s.update();
    let mut block = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    [block[0], block[1]]
}

/// **A main bus connected to the root feeds its channels in order**: a stereo
/// constant (0.25, 0.5) is heard as (0.25, 0.5). A mono one, narrower than the
/// root, is placed by speaker: a lone channel feeds the front pair of a stereo
/// root at gain 1.0, so (0.5) is heard as (0.5, 0.5).
#[test]
fn a_main_bus_feeds_the_roots_channels_in_order() {
    let mut s = offline();
    let stereo = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.5])));
    s.graph().connect(stereo, Root).unwrap().unfaded();
    assert_eq!(first_frame(&mut s), [0.25, 0.5]);

    let mut s = offline();
    let mono = s.graph().add(ForkByClone(Const::mono(0.5)));
    s.graph().connect(mono, Root).unwrap().unfaded();
    assert_eq!(first_frame(&mut s), [0.5, 0.5]);
}

/// **A mono bus into a declared stereo root is placed by speaker** as into an
/// undeclared one: (0.5) is heard as (0.5, 0.5).
#[test]
fn a_mono_bus_feeds_both_channels_of_a_declared_stereo_root() {
    let mut s = offline();
    s.declare_root_topology(Some(ChannelTopology::default_for(ChannelLayout::STEREO)))
        .expect("a stereo root");
    let mono = s.graph().add(ForkByClone(Const::mono(0.5)));
    s.graph().connect(mono, Root).unwrap().unfaded();
    assert_eq!(first_frame(&mut s), [0.5, 0.5]);
}

/// **A narrower bus with no place on the root is refused, never truncated**:
/// a root declared as two unassigned channels names no speaker a mono bus
/// could feed, so the fold fails, the refusal names it, and nothing is stored.
#[test]
fn a_bus_with_no_place_on_the_root_is_refused() {
    let mut s = offline();
    s.declare_root_topology(Some(ChannelTopology::unassigned(ChannelLayout::STEREO)))
        .expect("an unassigned stereo root");
    let mono = s.graph().add(ForkByClone(Const::mono(0.5)));
    let _ = s.update();
    assert!(matches!(
        s.graph().connect(mono, Root).err(),
        Some(ConnectError::RootFold { from, cause: FoldError::Unassigned { .. } })
            if from == mono.erase()
    ));
    assert!(!s.graph_ref().has_uncommitted_edits(), "nothing was stored");
}

/// **A bus wider than the root is refused, never truncated**, and nothing is
/// stored.
#[test]
fn a_bus_wider_than_the_root_is_refused() {
    let mut s = offline();
    let wide = s.graph().add(ForkByClone(Const::frame(&[1.0, 2.0, 3.0])));
    let _ = s.update();
    let root_width = s.graph_ref().outputs();
    assert_eq!(
        s.graph().connect(wide, Root).err(),
        Some(ConnectError::RootNarrow {
            from: wide.erase(),
            from_width: 3,
            root_width,
        })
    );
    assert!(!s.graph_ref().has_uncommitted_edits(), "nothing was stored");
}

/// **A port that is not there is named and refused**: an output port past the
/// node's, an input past it, a root channel past the outputs; none is stored.
#[test]
fn a_port_that_is_not_there_is_refused() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.5)));
    let through = s.graph().add(ForkByClone(Through::mono()));
    let _ = s.update();
    let g = s.graph();
    assert_eq!(
        g.connect(src.out(1), through.input(0)).err(),
        Some(ConnectError::NoSuchOutPort(src.out(1)))
    );
    assert_eq!(
        g.connect(src.out(0), through.input(1)).err(),
        Some(ConnectError::NoSuchInPort(through.input(1)))
    );
    assert_eq!(
        g.connect(src.out(0), Root::output(9)).err(),
        Some(ConnectError::NoSuchInPort(Root::output(9)))
    );
    assert_eq!(
        g.connect(Root::input(3), through.input(0)).err(),
        Some(ConnectError::NoSuchOutPort(Root::input(3)))
    );
    assert!(!g.has_uncommitted_edits(), "nothing was stored");
}

/// **A connection's gain weights what the call added**: `0.5` of 0.5 is heard,
/// the edge's target reads back its gain, and a gain that is not a number takes the edge
/// the call added out again.
#[test]
fn a_connections_gain_weights_the_edge_it_added() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.5)));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .unfaded()
        .gain(Amplitude::new(0.5))
        .unwrap();
    assert_eq!(first_frame(&mut s), [0.25, 0.0]);
    let edge = Edge::new(src.out(0), Root::output(0));
    let gain = s
        .graph_ref()
        .gain_target(edge.source(), edge.sink())
        .expect("a lane");
    assert_eq!(s.get(gain).unwrap(), Amplitude::new(0.5));
    assert!(edge.clear_gain(s.graph()));
    assert_eq!(
        s.graph_ref().gain_target(edge.source(), edge.sink()),
        Err(GainTargetError::NoLane(edge))
    );
    assert_eq!(first_frame(&mut s), [0.5, 0.0]);

    let other = s.graph().add(ForkByClone(Const::mono(0.125)));
    assert_eq!(
        s.graph()
            .connect(other.out(0), Root::output(1))
            .unwrap()
            .unfaded()
            .gain(Amplitude::new(f64::NAN))
            .err(),
        Some(DeclarationError::NotFinite)
    );
    assert!(
        s.graph_ref().sources(Root::output(1)).unwrap().is_empty(),
        "the edge the refused gain came with is gone"
    );
}

/// **`disconnect` takes out exactly the edge named**, says whether it was
/// there, and an empty `set_sources` takes out the rest.
#[test]
fn disconnect_removes_one_edge_and_an_empty_set_sources_the_rest() {
    let mut s = offline();
    let a = s.graph().add(ForkByClone(Const::mono(0.5)));
    let b = s.graph().add(ForkByClone(Const::mono(0.25)));
    let sink = Root::output(0);
    s.graph().connect(a.out(0), sink).unwrap().unfaded();
    s.graph().connect(b.out(0), sink).unwrap().unfaded();
    assert_eq!(first_frame(&mut s), [0.75, 0.0]);

    assert!(
        s.graph()
            .disconnect(a.out(0), sink)
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(
        !s.graph()
            .disconnect(a.out(0), sink)
            .unwrap()
            .unfaded()
            .was_connected(),
        "already gone"
    );
    assert_eq!(s.graph_ref().sources(sink).unwrap(), [b.out(0)]);
    assert_eq!(first_frame(&mut s), [0.25, 0.0]);

    s.graph().set_sources(sink, []).unwrap();
    assert!(s.graph_ref().sources(sink).unwrap().is_empty());
    // The edge fades out over 256 frames: the fifth 64-frame block starts
    // past it.
    for _ in 0..4 {
        first_frame(&mut s);
    }
    assert_eq!(first_frame(&mut s), [0.0, 0.0]);
}

/// **A main bus disconnects as it connected**: channel by channel, leaving a
/// channel wired by hand alone.
#[test]
fn a_main_bus_disconnects_what_it_connected() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::frame(&[0.25, 0.5])));
    let through = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    s.graph().connect(src, through).unwrap().unfaded();
    assert_eq!(
        s.graph_ref().sources(through.input(1)).unwrap(),
        [src.out(1)],
        "channel by channel"
    );
    assert!(
        s.graph()
            .disconnect(src, through)
            .unwrap()
            .unfaded()
            .was_connected()
    );
    assert!(s.graph_ref().sources(through.input(0)).unwrap().is_empty());
    assert!(s.graph_ref().sources(through.input(1)).unwrap().is_empty());
    assert!(
        !s.graph()
            .disconnect(src, through)
            .unwrap()
            .unfaded()
            .was_connected(),
        "nothing left to take out"
    );
}

/// **A key and its erasure name the same node**: connecting both ways adds one
/// edge, and the second call changes nothing.
#[test]
fn a_key_and_its_erasure_are_one_node() {
    let mut s = offline();
    let src = s.graph().add(ForkByClone(Const::mono(0.5)));
    s.graph().connect(src, Root).unwrap().unfaded();
    let _ = s.update();
    s.graph().connect(src.erase(), Root).unwrap().unfaded();
    s.graph()
        .connect(OutPort::new(src.erase(), 0), Root::output(0))
        .unwrap()
        .unfaded();
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "the same edge, three spellings"
    );
    assert_eq!(
        s.graph_ref().sources(Root::output(0)).unwrap(),
        [src.out(0)]
    );
}
