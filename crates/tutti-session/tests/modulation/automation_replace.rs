//! A node's automation is a declaration on its key, so it outlives a replace:
//! the target still reads and writes, the declaration is still held, and the
//! curve still drives the incoming node's param.

use std::sync::Arc;

use tutti_graph::Fade;
use tutti_nodes::automation::{Breakpoint, Trajectory};
use tutti_session::{ParamRef, Replacement};
use tutti_units::{Beat, Hz, Samples, UnitParam};

use crate::common::probe::ProbeUnit;
use crate::mod_lanes::{main_output, render, rig, settle};

/// A trajectory holding `at` Hz from beat 0.
fn hold(at: f64) -> Trajectory<Hz> {
    let mut curve = Trajectory::new();
    curve.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(at)));
    curve
}

/// A cutoff probe (knob 300, range `0..=1000`) automated to 800, rendered
/// once and replaced by a probe whose knob is 400, with `fade`. Asserts, after
/// the replace, that the target reads and writes, the declaration is held and
/// the curve plays on the incoming node.
fn replaced_keeps_automation(replacement: impl FnOnce(ProbeUnit) -> Replacement<ProbeUnit>) {
    let (mut s, mut side) = rig(1);
    let probe = s.graph().add(ProbeUnit::new(300.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let target = probe.cutoff().automation();
    let first = Arc::new(hold(800.0));
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(m.automate(cutoff, Arc::clone(&first)));
    settle(&mut s);
    assert_eq!(*render(&mut side, 1_024, 128)[0].last().unwrap(), 800.0);

    s.graph()
        .replace(probe, replacement(ProbeUnit::new(400.0)))
        .expect("the replace is accepted");
    settle(&mut s);

    assert!(
        s.mods().is_automated(cutoff),
        "the declaration is still held"
    );
    assert_eq!(s.get(target).unwrap(), first, "the target still reads");
    assert_eq!(
        *render(&mut side, 4_096, 128)[0].last().unwrap(),
        800.0,
        "the kept curve drives the incoming node's param, not its knob (400)"
    );
    let second = Arc::new(hold(600.0));
    s.set(target.to(Arc::clone(&second)))
        .expect("the target still writes");
    assert_eq!(s.get(target).unwrap(), second);
    let _ = s.update();
    assert_eq!(
        *render(&mut side, 4_096, 128)[0].last().unwrap(),
        600.0,
        "the curve drives the incoming node's param"
    );
}

/// **A crossfade replace keeps the automation target and its curve.** The
/// target answers, the declaration stays held, and the set curve (600) plays.
#[test]
fn a_crossfade_replace_keeps_the_automation() {
    replaced_keeps_automation(Replacement::new);
}

/// **A swap replace keeps the automation target and its curve.**
#[test]
fn a_swap_replace_keeps_the_automation() {
    replaced_keeps_automation(|unit| Replacement::new(unit).fade(Fade::new(Samples::ZERO)));
}
