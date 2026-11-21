//! The reverbs through the session: their params by address, indexed for the
//! FDN's three decays, and the tail each declares to the graph (the FDN's
//! unbounded where it can freeze).

use tutti_buffers::MaxBlock;
use tutti_nodes::{
    FdnConfig, FdnLayout, FdnNode, FreezeMode, MaxDecay, PlateReverbConfig, PlateReverbNode,
};
use tutti_session::{Session, ValueErrorKind};
use tutti_units::{Mix, Samples, Seconds, Tail};

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// `Session::set` at a decay's index reaches that decay's cell and no other,
/// and the freeze is addressable only where the design can freeze.
#[test]
fn a_write_reaches_the_decay_it_names() {
    let mut s = session();
    let node = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new().layout(
            FdnLayout::new(8, MaxDecay::DEFAULT, FreezeMode::Freezable).unwrap(),
        )));
    s.set(node.decay_at(1).to(Seconds::new(5.0))).unwrap();
    s.set(node.freeze().to(true)).unwrap();
    assert_eq!(s.get(node.decay_at(1)).unwrap(), Seconds::new(5.0), "low");
    assert_eq!(
        s.get(node.decay_at(0)).unwrap(),
        Seconds::new(2.0),
        "mid untouched"
    );
    assert_eq!(
        s.get(node.decay_at(2)).unwrap(),
        Seconds::new(1.2),
        "high untouched"
    );
    assert!(s.get(node.freeze()).unwrap());

    let plain = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
    assert_eq!(
        s.set(plain.freeze().to(true)).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget
    );
    assert_eq!(
        s.get(plain.freeze()).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget,
        "no freeze to read"
    );
}

/// The tail each reverb declares to the graph: finite for a design that cannot
/// freeze (the room at its longest decay), unbounded for one that can.
#[test]
fn the_graph_sees_each_reverbs_ceiling() {
    let mut s = session();
    let plain = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
    let frozen = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new().layout(
            FdnLayout::new(8, MaxDecay::DEFAULT, FreezeMode::Freezable).unwrap(),
        )));
    let _ = s.update();
    let Tail::Finite(n) = s.graph_ref().node_tail(plain).unwrap() else {
        panic!("finite")
    };
    assert!(n.get() as f64 / 48_000.0 > 8.0);
    assert_eq!(s.graph_ref().node_tail(frozen).unwrap(), Tail::Unbounded);
}

/// The plate is a session node: `Session::set` reaches its decay and diffusion cells by
/// the key's typed methods and no other, it has no freeze method on its key, and the graph sees the
/// finite ceiling it declares (the longest decay and pre-delay).
#[test]
fn the_plate_is_a_session_node_with_its_params_and_ceiling() {
    let mut s = session();
    let node = s
        .graph()
        .add::<PlateReverbNode<f32>>(PlateReverbNode::<f32>::from(PlateReverbConfig::new()));
    s.set(node.decay().to(Seconds::new(5.0))).unwrap();
    s.set(node.diffusion().to(Mix::new(0.25))).unwrap();
    assert_eq!(s.get(node.decay()).unwrap(), Seconds::new(5.0));
    assert_eq!(s.get(node.diffusion()).unwrap(), Mix::new(0.25));
    assert_eq!(
        s.get(node.damping()).unwrap(),
        Mix::new(0.0005),
        "untouched"
    );
    let _ = s.update();
    let Tail::Finite(n) = s.graph_ref().node_tail(node).unwrap() else {
        panic!("finite")
    };
    assert!(n.get() as f64 / 48_000.0 > 8.0);
}
