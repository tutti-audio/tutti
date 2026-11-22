//! A modulation route onto the **gain of an edge** (doc 013, "Edge gain",
//! G2): `Mods::route(source, ChannelEdge, depth)` gives the edge's sink sum a
//! driven lane, as a route onto a param gives a port one.
//!
//! The figures are analytic. Two constant sources sum at main output 0;
//! a macro (a constant source) is routed onto the edge of the first at a
//! depth over a range, and the render is the weighted fold
//! `(0.0 + w * a) + b` with `w = depth * macro * span` (a bipolar linear
//! route on a lane with no knob: its base is nothing, and the lane's value is
//! the route's whole figure; the range clamps it).

use crate::common;
use crate::common::render;
use crate::offline::offline_48k;

use tutti_graph::ForkByClone;
use tutti_session::{
    ChannelEdge, GainTarget, MainOutput, ModTarget, OutputPort, ParamRef, RangeTarget, RouteSpec,
    Session,
};
use tutti_units::{Amplitude, Depth, UnitParam};

use common::probe::Pulse;

const BLOCK: usize = 128;
/// The sources' levels.
const A: f32 = 0.5;
const B: f32 = 0.25;

fn settle(s: &mut Session) {
    for _ in 0..6 {
        let _ = s.update();
    }
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "the declarations apply"
    );
}

/// The gain target of `edge`, which has a lane.
fn gain(s: &Session, edge: ChannelEdge) -> GainTarget {
    s.graph_ref()
        .gain_target(edge.source(), edge.sink())
        .expect("a lane")
}

/// Two always-on sources summed at output 0; the edge of the first, which has
/// a gain lane at unity (declared with the wire).
fn mixer(s: &mut Session) -> ChannelEdge {
    let level = |level| {
        ForkByClone(Pulse {
            from: 0,
            to: u64::MAX,
            level,
        })
    };
    let a = s.graph().add(level(A));
    let b = s.graph().add(level(B));
    s.graph()
        .connect(OutputPort::new(a, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded()
        .gain(Amplitude::UNITY)
        .unwrap();
    s.graph()
        .connect(OutputPort::new(b, 0), MainOutput::channel(0))
        .unwrap()
        .unfaded();
    ChannelEdge::new(OutputPort::new(a, 0), MainOutput::channel(0))
}

/// A route onto an edge's gain modulates the sum by the analytic amount. The
/// lane's value is its knob (the gain's base, unity) plus the route's offset,
/// `depth * macro * span`, clamped to the range: with a range of `-2..=2`
/// (span 4), macro 0.5 at depth 0.25 gives `w = 1.0 + 0.5 = 1.5`, so the sum
/// is `(0.0 + 1.5 * A) + B`. Then the macro moves (`w = 1.0 - 0.5 = 0.5`),
/// the knob moves (the gain target: `w = -0.25 - 0.5`, polarity), a
/// narrower range changes the span, and one whose floor the value is below
/// clamps it; and removing the route returns the edge to its base, which
/// the knob left at `-0.25` (the knob is the edge's gain: one cell), once the
/// lane's source goes (the declick).
#[test]
fn a_route_onto_an_edge_modulates_the_sum_by_the_analytic_amount() {
    let mut s = offline_48k();
    let edge = mixer(&mut s);
    settle(&mut s);
    let plain = render(&mut s, 2, BLOCK);
    assert!(plain.iter().all(|&x| x == A + B), "unity before any route");

    let mut m = s.mods_mut();
    m.range(edge, Amplitude::new(-2.0)..Amplitude::new(2.0))
        .unwrap();
    let source = m.macro_(0.5);
    let route = m
        .route(RouteSpec::new(source, edge, Depth::of(0.25)))
        .unwrap();
    settle(&mut s);

    // Past the lane's first block and the connect's declick.
    let figure = |s: &mut Session, w: f32, what: &str| {
        let got = render(s, 12, BLOCK);
        let want = (0.0 + w * A) + B;
        for (f, &x) in got.iter().enumerate().skip(512) {
            assert_eq!(x.to_bits(), want.to_bits(), "{what}, frame {f}");
        }
    };
    figure(&mut s, 1.5, "knob + offset");

    s.set(source.value().to(tutti_units::Bipolar::of(-0.5)))
        .unwrap();
    settle(&mut s);
    figure(&mut s, 0.5, "the macro moved");

    s.set(gain(&s, edge).to(Amplitude::new(-0.25))).unwrap();
    settle(&mut s);
    figure(&mut s, -0.75, "the knob moved");

    // The range is the route's span too: `-0.25 + 0.25 * -0.5 * 2`.
    s.set(RangeTarget::of(edge).to(Amplitude::new(-2.0)..Amplitude::new(0.0)))
        .unwrap();
    settle(&mut s);
    figure(&mut s, -0.5, "within the range");
    // Span 2.25: `-0.25 - 0.28125` is below the range's floor.
    s.set(RangeTarget::of(edge).to(Amplitude::new(-0.25)..Amplitude::new(2.0)))
        .unwrap();
    settle(&mut s);
    figure(&mut s, -0.25, "clamped to the range");

    s.mods_mut().remove_route(route).unwrap();
    settle(&mut s);
    figure(&mut s, -0.25, "the route gone: the base the knob left");
}

/// The gain has one writer. `GainTarget` is the knob whether or not
/// the edge is routed: a base of 0.5 stays the base when a route arrives (the
/// lane modulates around it, it does not snap to unity), a base moved while
/// routed moves the modulated figure, and removing the route returns to the
/// base, not unity.
///
/// With a range of `-2..=2` (span 4) the macro at 0.5 and depth 0.5 offsets
/// by `0.5 * 0.5 * 4 = 1.0`: `w = base + 1.0`, so `1.5`, then `1.25` once the
/// base moves to 0.25; gone, the base `0.25`.
#[test]
fn the_base_is_the_lanes_knob_and_a_route_returns_to_it() {
    let mut s = offline_48k();
    let edge = mixer(&mut s);
    settle(&mut s);
    s.set(gain(&s, edge).to(Amplitude::new(0.5))).unwrap();
    settle(&mut s);

    let figure = |s: &mut Session, w: f32, what: &str| {
        let got = render(s, 12, BLOCK);
        let want = (0.0 + w * A) + B;
        for (f, &x) in got.iter().enumerate().skip(512) {
            assert_eq!(x.to_bits(), want.to_bits(), "{what}, frame {f}");
        }
    };
    figure(&mut s, 0.5, "the base, before any route");

    let mut m = s.mods_mut();
    m.range(edge, Amplitude::new(-2.0)..Amplitude::new(2.0))
        .unwrap();
    let source = m.macro_(0.5);
    let route = m
        .route(RouteSpec::new(source, edge, Depth::of(0.5)))
        .unwrap();
    settle(&mut s);
    figure(&mut s, 1.5, "the base plus the route's offset");

    s.set(gain(&s, edge).to(Amplitude::new(0.25))).unwrap();
    settle(&mut s);
    figure(&mut s, 1.25, "the base moved while routed");
    assert_eq!(s.get(gain(&s, edge)).unwrap(), Amplitude::new(0.25));

    s.mods_mut().remove_route(route).unwrap();
    settle(&mut s);
    figure(&mut s, 0.25, "the route gone: the base, not unity");
}

/// **A route onto the gain of an edge declared in the same update goes out in
/// that update's one plan, and modulates from its first frame.** The two
/// sources, the wire with its gain at unity and a route of macro `0.5` at
/// depth `0.25` over `-2..=2` are declared before one update: the lane binds
/// to the cells the gain entry made when it was declared, so exactly one plan
/// is sent and its first frame is the weighted fold at `1.0 + 0.5 = 1.5`.
#[test]
fn a_route_onto_an_edge_declared_in_the_update_sends_one_plan() {
    let mut s = offline_48k();
    settle(&mut s);
    let plans = s.graph_ref().testing().commits_sent();

    let edge = mixer(&mut s);
    let mut m = s.mods_mut();
    m.range(edge, Amplitude::new(-2.0)..Amplitude::new(2.0))
        .unwrap();
    let source = m.macro_(0.5);
    m.route(RouteSpec::new(source, edge, Depth::of(0.25)))
        .unwrap();

    let _ = s.update();
    assert_eq!(s.mod_state().problems().next(), None, "the route resolved");
    assert_eq!(
        s.graph_ref().testing().commits_sent(),
        plans + 1,
        "one plan for the wire and its lane"
    );
    let got = render(&mut s, 1, BLOCK);
    let want = (0.0 + 1.5 * A) + B;
    assert_eq!(got[0].to_bits(), want.to_bits(), "the first frame");
    assert_eq!(got[BLOCK - 1].to_bits(), want.to_bits());
}

/// An edge target keys once per edge, apart from a param's, and is no param.
#[test]
fn an_edge_target_keys_apart_and_goes_with_its_node() {
    let mut s = offline_48k();
    let edge = mixer(&mut s);
    let key = s.mods_mut().key(edge);
    assert_eq!(
        key,
        s.mods_mut().key(ModTarget::Edge(edge)),
        "one key an edge"
    );
    let param = ParamRef::unit(tutti_graph::NodeKey(1), UnitParam::Volume);
    assert_ne!(key, s.mods_mut().key(param));
    assert_eq!(ParamRef::from_key(key), None);
}
