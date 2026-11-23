//! `Session::get(target.effective())`: the value a lane drives a native param
//! to, at most one block old, and the base while no lane drives it. Every
//! figure is analytic, as in `ranges.rs`: a macro at 0.5 routed at depth 0.4
//! into a cutoff of knob 500 over `0..=1000` is `500 + 0.4 * 0.5 * 1000 = 700`.

use tutti_session::{DetachedSession, RouteSpec};
use tutti_units::{Bipolar, Hz};

use crate::common::probe::ProbeUnit;
use crate::mod_lanes::{main_output, render, rig, settle};

/// **A param no lane drives has the base for its effective value**, before
/// and after a commit and a render, and follows a base that moves.
#[test]
fn an_undriven_param_is_effective_at_its_base() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    assert_eq!(s.get(probe.cutoff().effective()).unwrap(), Hz::new(500.0));
    settle(&mut s);
    let _ = render(&mut side, 512, 128);
    assert_eq!(s.get(probe.cutoff().effective()).unwrap(), Hz::new(500.0));
    s.set(probe.cutoff().to(Hz::new(640.0))).unwrap();
    let _ = render(&mut side, 512, 128);
    assert_eq!(s.get(probe.cutoff().effective()).unwrap(), Hz::new(640.0));
}

/// **A lane moves the effective value and leaves the base**; a range
/// narrower than the lane's figure holds the effective value and not the
/// base ("a narrowed range never destroys the base"); the lane removed, the
/// effective value is the base again after a block.
#[test]
fn a_lane_moves_the_effective_value_and_never_the_base() {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(&mut s, &[probe.erase()]);
    let mut m = s.mods_mut();
    m.range(probe.cutoff(), Hz::new(0.0)..Hz::new(1_000.0))
        .unwrap();
    let source = m.macro_(0.5);
    let route = m
        .route(RouteSpec::new(source, probe.cutoff(), Bipolar::of(0.4)))
        .unwrap();
    settle(&mut s);
    let _ = render(&mut side, 1_024, 128);
    let near = |s: &DetachedSession, want: f64| {
        let got = s.get(probe.cutoff().effective()).unwrap().get();
        assert!((got - want).abs() < 1e-3, "{got} is not {want}");
    };
    near(&s, 700.0);
    assert_eq!(s.get(probe.cutoff()).unwrap(), Hz::new(500.0));

    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(600.0)))
        .unwrap();
    settle(&mut s);
    let _ = render(&mut side, 1_024, 128);
    near(&s, 600.0);
    assert_eq!(
        s.get(probe.cutoff()).unwrap(),
        Hz::new(500.0),
        "the base is not the range's"
    );

    s.mods_mut().remove_route(route).unwrap();
    settle(&mut s);
    let _ = render(&mut side, 1_024, 128);
    assert_eq!(s.get(probe.cutoff().effective()).unwrap(), Hz::new(500.0));
}

/// **An effective read of a node that is gone fails at the call**, as a
/// base read does.
#[test]
fn an_effective_read_of_a_removed_node_is_no_such_target() {
    let (mut s, _side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = probe.cutoff();
    s.remove(probe.erase()).unwrap();
    let err = s.get(cutoff.effective()).unwrap_err();
    assert_eq!(err.kind, tutti_session::ValueErrorKind::NoSuchTarget);
}
