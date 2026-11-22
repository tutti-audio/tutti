//! A node's params are addressed as the node goes in, so a modulation lane's
//! knob is the node's own cell.
//!
//! The insertion paths are `Graph::add` and `Graph::replace`; these pin
//! them: a lane finds a native param's knob
//! through the node's `ParamSet`, which the insertion keeps by key
//! (`SessionNode::params`, `set_node_params`). They stand at the insertion
//! paths rather than at a fixture that addresses by hand, because a path that
//! forgot to would leave every hand-built suite green. The nodes are the
//! suites' own `ProbeUnit` of `Drive` (a host's `ParamNode`) and an engine
//! node, `DistortionNode`.

use crate::common;

use atomic_float::AtomicF64;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_nodes::{DistortionConfig, DistortionNode, ShapeKind};
use tutti_rt::ParamCell;
use tutti_session::{MainOutput, OutputPort, ParamRef, Replacement, RouteSpec, Session};
use tutti_units::{Depth, Drive, UnitParam};

use common::probe::ProbeUnit;

const BASE: f64 = 5.0;

fn session() -> Session {
    Session::builder()
        .outputs(ChannelLayout::MONO)
        .build()
        .expect("builds")
}

/// A macro on `node`'s drive, `0..=10`, and updates until it applies.
fn modulate_drive(s: &mut Session, node: NodeKey) {
    let drive = ParamRef::unit(node, UnitParam::Drive);
    let mut m = s.mods_mut();
    m.range_raw(drive, 0.0, 10.0).unwrap();
    let offset = m.macro_(1.0);
    m.route(RouteSpec::new(offset, drive, Depth::of(0.1)))
        .unwrap();
    settle(s);
}

fn settle(s: &mut Session) {
    for _ in 0..4 {
        let _ = s.update();
    }
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );
}

/// The knob of `node`'s drive lane, `None` with no lane or no knob.
fn knob(s: &Session, node: NodeKey) -> Option<f64> {
    s.mod_state()
        .lane(ParamRef::unit(node, UnitParam::Drive).key())
        .and_then(|(_, lane)| lane.value())
}

/// The lane's knob reads `cell`: what it holds, and a write into it.
fn assert_knob_is(s: &Session, node: NodeKey, cell: impl Into<ParamCell>) {
    let cell = cell.into();
    assert_eq!(knob(s, node), Some(cell.load(Ordering::Acquire)));
    cell.store(7.5, Ordering::Release);
    assert_eq!(knob(s, node), Some(7.5), "the knob is the node's cell");
}

#[test]
fn add_addresses_a_hosts_param_node() {
    let mut s = session();
    let unit = ProbeUnit::of(UnitParam::Drive, BASE);
    let cell = unit.knob();
    let node = s.graph().add(unit);
    modulate_drive(&mut s, node.erase());
    assert_knob_is(&s, node.erase(), cell);
}

#[test]
fn add_addresses_an_engine_node() {
    let mut s = session();
    let node = DistortionNode::<f32>::from(
        DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh).drive(Drive::new(BASE)),
    );
    let cell: Arc<AtomicF64> = tutti_graph::ParamNode::param_set(&node)
        .cell(UnitParam::Drive)
        .expect("a distortion has a drive")
        .into();
    let key = s.graph().add(node);
    modulate_drive(&mut s, key.erase());
    assert_knob_is(&s, key.erase(), cell);
}

/// After a crossfade the lane's knob is the **incoming** node's cell: a
/// write into it moves the knob, and one into the outgoing node's does not.
#[test]
fn a_crossfade_hands_the_lane_the_incoming_nodes_cell() {
    let mut s = session();
    let outgoing = ProbeUnit::of(UnitParam::Drive, BASE);
    let old = outgoing.knob();
    let node = s.graph().add(outgoing);
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(node, 0)])
        .unwrap();
    modulate_drive(&mut s, node.erase());
    assert_eq!(knob(&s, node.erase()), Some(BASE), "precondition: the lane");

    let incoming = ProbeUnit::of(UnitParam::Drive, 3.0);
    let new = incoming.knob();
    s.graph()
        .replace(node, Replacement::new(incoming))
        .expect("the crossfade starts");
    settle(&mut s);
    old.store(9.0, Ordering::Release);
    assert_eq!(knob(&s, node.erase()), Some(3.0), "not the outgoing cell");
    assert_knob_is(&s, node.erase(), new);
}
