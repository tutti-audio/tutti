//! The reverbs through the session: their params by address, indexed for the
//! FDN's three decays, and the tail each declares to the graph (the FDN's
//! unbounded where it can freeze).

use tutti_buffers::MaxBlock;
use tutti_nodes::{
    FdnConfig, FdnLayout, FdnNode, FreezeMode, MaxDecay, PlateReverbConfig, PlateReverbNode,
};
use tutti_session::{Session, ValueErrorKind};
use tutti_units::{Mix, Samples, Seconds, Tail, UnitAddr, UnitParam};

fn session() -> (Session, tutti_session::Background) {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// `Session::set` at a decay's index reaches that decay's cell and no other,
/// and the freeze is addressable only where the design can freeze.
#[test]
fn a_write_reaches_the_decay_it_names() {
    let (mut s, _bg) = session();
    let (node, cells) = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new().layout(
            FdnLayout::new(8, MaxDecay::DEFAULT, FreezeMode::Freezable).unwrap(),
        )));
    let at = |i| UnitAddr::new(UnitParam::Decay, i);
    s.set(node.decay_at(1).to(Seconds::new(5.0))).unwrap();
    s.set(node.freeze().to(true)).unwrap();
    assert_eq!(cells.get(at(1)), Some(5.0), "low");
    assert_eq!(cells.get(at(0)), Some(2.0), "mid untouched");
    assert_eq!(cells.get(at(2)), Some(1.2), "high untouched");
    assert_eq!(cells.get(UnitParam::Freeze), Some(1.0));

    let (plain, cells) = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
    assert_eq!(
        s.set(plain.freeze().to(true)).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget
    );
    assert_eq!(cells.get(UnitParam::Freeze), None, "no freeze to write");
}

/// The tail each reverb declares to the graph: finite for a design that cannot
/// freeze (the room at its longest decay), unbounded for one that can.
#[test]
fn the_graph_sees_each_reverbs_ceiling() {
    let (mut s, _bg) = session();
    let (plain, _) = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
    let (frozen, _) = s
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
/// the key's typed methods and no other, it has no freeze to address, and the graph sees the
/// finite ceiling it declares (the longest decay and pre-delay).
#[test]
fn the_plate_is_a_session_node_with_its_params_and_ceiling() {
    let (mut s, _bg) = session();
    let (node, cells) = s
        .graph()
        .add::<PlateReverbNode<f32>>(PlateReverbNode::<f32>::from(PlateReverbConfig::new()));
    s.set(node.decay().to(Seconds::new(5.0))).unwrap();
    s.set(node.diffusion().to(Mix::new(0.25))).unwrap();
    assert_eq!(cells.get(UnitParam::Decay), Some(5.0));
    assert_eq!(cells.get(UnitParam::Diffusion), Some(0.25));
    assert_eq!(cells.get(UnitParam::Damping), Some(0.0005), "untouched");
    assert_eq!(cells.get(UnitParam::Freeze), None, "no freeze to write");
    let _ = s.update();
    let Tail::Finite(n) = s.graph_ref().node_tail(node).unwrap() else {
        panic!("finite")
    };
    assert!(n.get() as f64 / 48_000.0 > 8.0);
}
