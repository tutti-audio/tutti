//! A route's range is a range of the target's unit, and a backwards one is
//! refused where it is declared (doc 030, T2). Its value afterwards is the
//! `RangeTarget`'s: it bounds the effective value and never the base.

use std::sync::Arc;

use tutti_buffers::MaxBlock;
use tutti_graph::NodeKey;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_session::{
    DeclarationError, ParamRef, RangeTarget, RouteError, RouteSpec, Session, SetRefusal,
    ValueErrorKind,
};
use tutti_units::{Depth, Hz, Mix, Q, Samples, UnitParam};

use crate::common::probe::ProbeUnit;
use crate::mod_lanes::{master, render, rig, settle};
use tutti_buffers::ChannelLayout;

fn session() -> (Session, tutti_session::Background) {
    Session::builder()
        .outputs(tutti_buffers::ChannelLayout::STEREO)
        .max_block(MaxBlock::of(Samples::new(256)))
        .build()
        .expect("builds")
}

/// **A range with its start above its end is refused with both bounds, and
/// nothing is declared**: the route that needs the range is still refused as
/// undeclared. A NaN or infinite bound is refused as not finite.
#[test]
fn a_backwards_or_nan_range_is_refused_and_declares_nothing() {
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let mut m = s.mods_mut();
    assert_eq!(
        m.range(lp.cutoff(), Hz::new(4_000.0)..Hz::new(100.0)),
        Err(DeclarationError::Backwards {
            min: 4_000.0,
            max: 100.0
        })
    );
    assert_eq!(
        m.range(lp.cutoff(), Hz::new(f64::NAN)..Hz::new(100.0)),
        Err(DeclarationError::NotFinite)
    );
    assert_eq!(
        m.range_raw(lp.cutoff(), 1.0, f64::NAN),
        Err(DeclarationError::NotFinite)
    );
    assert_eq!(
        m.range_raw(lp.cutoff(), 1.0, f64::INFINITY),
        Err(DeclarationError::NotFinite)
    );
    let src = m.macro_(0.0);
    assert!(matches!(
        m.route(RouteSpec::new(src, lp.cutoff(), Depth::of(0.5))),
        Err(RouteError::UndeclaredRange { .. })
    ));

    // A range of one point is a range.
    assert_eq!(
        m.range(lp.cutoff(), Hz::new(500.0)..Hz::new(500.0)),
        Ok(true)
    );
    assert!(
        m.route(RouteSpec::new(src, lp.cutoff(), Depth::of(0.5)))
            .is_ok()
    );
}

/// **A typed range, an untyped one and the typed reference's `ParamRef` are
/// one declaration**: a range set through the typed form is the target of a
/// route named by the untyped one, and an edge's and a mix's ranges are in
/// their own units.
#[test]
fn a_typed_range_declares_the_target_an_untyped_ref_names() {
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let mut m = s.mods_mut();
    m.range(lp.cutoff(), Hz::new(100.0)..Hz::new(4_000.0))
        .unwrap();
    let src = m.macro_(0.0);
    let same = ParamRef::unit(lp.erase(), tutti_units::UnitParam::Cutoff);
    assert_eq!(ParamRef::from(lp.cutoff()), same);
    assert!(
        m.route(RouteSpec::new(src, same, Depth::of(0.5))).is_ok(),
        "the range is declared"
    );

    // A mix's range is in `Mix`: backwards is refused there too.
    let mix = tutti_session::MixRef::of(NodeKey(1));
    assert_eq!(m.range(mix, Mix::new(0.0)..Mix::new(1.0)), Ok(true));
    assert!(matches!(
        m.range(mix, Mix::new(1.0)..Mix::new(0.0)),
        Err(DeclarationError::Backwards { .. })
    ));
}

/// **A typed range keeps its bounds at the unit's width.** `Hz` is `f64`; a
/// bound `f32` rounds to another figure is declared and read back as given.
#[test]
fn a_typed_range_keeps_bounds_f32_cannot_hold() {
    let (lo, hi) = (100.0 + 1.0 / 1_048_576.0, 4_000.0 + 1.0 / 1_048_576.0);
    assert_ne!(f64::from(lo as f32), lo, "f32 loses the lower bound");
    assert_ne!(f64::from(hi as f32), hi, "f32 loses the upper bound");
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    s.mods_mut()
        .range(lp.cutoff(), Hz::new(lo)..Hz::new(hi))
        .unwrap();
    assert_eq!(s.mods().range_of(lp.cutoff()), Some(lo..=hi));
}

/// **A range declared once keeps its value**: the declaration is the initial
/// range, and a second one answers `false` and changes nothing, so the value
/// has one writer, the target.
#[test]
fn a_second_declaration_keeps_the_range_the_target_holds() {
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    assert_eq!(
        s.mods_mut()
            .range(lp.cutoff(), Hz::new(100.0)..Hz::new(4_000.0)),
        Ok(true)
    );
    s.set(lp.cutoff().range().to(Hz::new(200.0)..Hz::new(300.0)))
        .unwrap();
    assert_eq!(
        s.mods_mut()
            .range(lp.cutoff(), Hz::new(100.0)..Hz::new(4_000.0)),
        Ok(false)
    );
    assert_eq!(
        s.get(lp.cutoff().range()).unwrap(),
        Hz::new(200.0)..Hz::new(300.0),
        "the target's value stands"
    );
    assert_eq!(s.mods().range_of(lp.cutoff()), Some(200.0..=300.0));
}

/// **A range write is checked at the call and lands nothing when refused**: a
/// start above the end is `Backwards`, a NaN or infinite bound `NotFinite`, a
/// param with no declared range has no target (`NoSuchTarget`), and a mix's
/// range is in `Mix`.
#[test]
fn a_range_write_is_refused_at_the_call() {
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let undeclared = s
        .set(lp.cutoff().range().to(Hz::new(1.0)..Hz::new(2.0)))
        .unwrap_err();
    assert_eq!(undeclared.kind, ValueErrorKind::NoSuchTarget);

    s.mods_mut()
        .range(lp.cutoff(), Hz::new(100.0)..Hz::new(4_000.0))
        .unwrap();
    for (range, why) in [
        (Hz::new(4_000.0)..Hz::new(100.0), SetRefusal::Backwards),
        (Hz::new(f64::NAN)..Hz::new(100.0), SetRefusal::NotFinite),
        (Hz::new(0.0)..Hz::new(f64::INFINITY), SetRefusal::NotFinite),
    ] {
        let refused = s.set(lp.cutoff().range().to(range)).unwrap_err();
        assert_eq!(refused.kind, ValueErrorKind::Refused(why));
    }
    assert_eq!(
        s.get(lp.cutoff().range()).unwrap(),
        Hz::new(100.0)..Hz::new(4_000.0),
        "nothing landed"
    );

    let mix = tutti_session::MixRef::of(NodeKey(1));
    s.mods_mut()
        .range(mix, Mix::new(0.0)..Mix::new(1.0))
        .unwrap();
    s.set(RangeTarget::of(mix).to(Mix::new(0.25)..Mix::new(0.75)))
        .unwrap();
    assert_eq!(
        s.get(RangeTarget::of(mix)).unwrap(),
        Mix::new(0.25)..Mix::new(0.75)
    );
}

/// **A narrowed range bounds the effective value and leaves the base alone.** A
/// macro at 0.5 routed at depth 0.4 into a cutoff of knob 500 over `0..=1000`
/// is `500 + 0.4 * 0.5 * 1000 = 700`. Narrowed to `0..=600` the span is 600,
/// the figure `500 + 0.4 * 0.5 * 600 = 620` and the lane clamps it to 600,
/// while the knob reads 500 throughout; widened again it is 700.
#[test]
fn a_narrowed_range_bounds_the_effective_value_and_not_the_base() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe.erase()]);
    let mut m = s.mods_mut();
    m.range(probe.cutoff(), Hz::new(0.0)..Hz::new(1_000.0))
        .unwrap();
    let source = m.macro_(0.5);
    m.route(RouteSpec::new(source, probe.cutoff(), Depth::of(0.4)))
        .unwrap();
    settle(&mut s);
    let tail = |side: &mut _| *render(side, 1_024, 128)[0].last().unwrap();
    let near = |got: f32, want: f32| {
        assert!((got - want).abs() < 1e-3, "{got} is not {want}");
    };
    near(tail(&mut side), 700.0);

    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(600.0)))
        .unwrap();
    assert_eq!(
        s.get(probe.cutoff().range()).unwrap(),
        Hz::new(0.0)..Hz::new(600.0)
    );
    settle(&mut s);
    near(tail(&mut side), 600.0);
    assert_eq!(
        s.get(probe.cutoff()).unwrap(),
        Hz::new(500.0),
        "the base is not the range's"
    );

    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(1_000.0)))
        .unwrap();
    settle(&mut s);
    near(tail(&mut side), 700.0);
    assert_eq!(s.get(probe.cutoff()).unwrap(), Hz::new(500.0));
}

/// **A range edit sends no plan and republishes no program**: it is a store
/// into the lane's range.
#[test]
fn a_range_write_republishes_no_program() {
    let (mut s, mut side) = rig(1);
    let (probe, _) = s.graph().add(ProbeUnit::new(500.0));
    master(&mut s, &[probe.erase()]);
    let mut m = s.mods_mut();
    m.range(probe.cutoff(), Hz::new(0.0)..Hz::new(1_000.0))
        .unwrap();
    let source = m.macro_(0.5);
    m.route(RouteSpec::new(source, probe.cutoff(), Depth::of(0.4)))
        .unwrap();
    settle(&mut s);
    render(&mut side, 1_024, 128);
    let lane = |s: &tutti_session::DetachedSession| {
        s.mod_state()
            .lane(ParamRef::unit(probe, UnitParam::Cutoff).key())
            .expect("a lane")
            .1
            .clone()
    };
    let before = lane(&s).program();
    assert_eq!(lane(&s).range(), (0.0, 1_000.0));
    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(600.0)))
        .unwrap();
    settle(&mut s);
    assert_eq!(
        lane(&s).range(),
        (0.0, 600.0),
        "the lane reads the new range"
    );
    assert!(Arc::ptr_eq(&before, &lane(&s).program()));
}

/// **A removed node's range and route depth are refused at the call**, with no
/// update in between: the targets go with the node, not at the next update.
#[test]
fn a_range_and_a_depth_of_a_removed_node_are_refused_at_the_call() {
    let (mut s, _bg) = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.7)),
    ));
    let mut m = s.mods_mut();
    m.range(lp.cutoff(), Hz::new(100.0)..Hz::new(4_000.0))
        .unwrap();
    let source = m.macro_(0.0);
    m.route(RouteSpec::new(source, lp.cutoff(), Depth::of(0.5)))
        .unwrap();
    let depth = tutti_session::DepthTarget::of(source, lp.cutoff());
    s.set(depth.to(Depth::of(0.25))).unwrap();

    s.graph().remove(lp).unwrap();
    let range = s.set(lp.cutoff().range().to(Hz::new(1.0)..Hz::new(2.0)));
    assert_eq!(range.unwrap_err().kind, ValueErrorKind::NoSuchTarget);
    let depth = s.set(depth.to(Depth::of(0.1)));
    assert_eq!(depth.unwrap_err().kind, ValueErrorKind::NoSuchTarget);
}
