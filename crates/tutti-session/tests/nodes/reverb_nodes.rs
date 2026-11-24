//! The reverbs through the session: their params by address, indexed for the
//! FDN's three decays, and the tail each declares to the graph (the FDN's
//! unbounded, since it can freeze).

use tutti_buffers::MaxBlock;
use tutti_nodes::{FdnConfig, FdnNode, PlateReverbConfig, PlateReverbNode};
use tutti_session::Session;
use tutti_units::{Mix, Samples, Seconds, Tail};

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// `Session::set` at a decay's index reaches that decay's cell and no other,
/// and the freeze is addressable on every FDN.
#[test]
fn a_write_reaches_the_decay_it_names() {
    let mut s = session();
    let node = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
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
}

/// The tail the FDN declares to the graph is unbounded: the freeze can hold it.
#[test]
fn the_graph_sees_the_fdns_unbounded_ceiling() {
    let mut s = session();
    let node = s
        .graph()
        .add::<FdnNode<f32>>(FdnNode::<f32>::from(FdnConfig::new()));
    let _ = s.update();
    assert_eq!(s.graph_ref().node_tail(node).unwrap(), Tail::Unbounded);
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
