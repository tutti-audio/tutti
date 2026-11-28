//! Modulation in the session: the session's
//! own declarations ([`Session::mods`]), applied by `Phase::Modulation`, committed once and resynced by `Phase::Commit`.
//!
//! The render tests stand on analytic figures (doc 014 §5.3's bound for a
//! sine on the control grid, a follower's closed form), never on a
//! reference render. The probe nodes are `tests/common/probe.rs`'s.

use crate::committed::committed;
use crate::common;
use crate::offline::offline_48k;

use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_modulation::ControlProgram;
use tutti_modulation::{Lfo, Polarity, TargetKey};
use tutti_nodes::automation::{ControlLaneNode, UnitLaneConfig};
use tutti_session::{
    MainOutput, ModTarget, OutputPort, PHASES, ParamRef, PhaseCx, Problem, RouteError, RouteSpec,
    Session, SessionEvent, TargetName,
};
use tutti_units::Samples;
use tutti_units::{BeatsPerCycle, Bipolar, Hz, UnitParam};

use common::probe::{ProbeUnit, Pulse};

const RATE: f64 = 48_000.0;
/// A lane's control step (`tutti_modulation::CONTROL_GRID`).
const STEP: usize = 32;
/// A port's connect crossfade: `DECLICK` at 48 kHz.
const DECLICK: usize = 240;

/// An offline session at 48 kHz, stereo.
/// Every phase at `now`: one update, at a time the test picks.
fn update_at(s: &mut Session, now: Instant) -> Vec<SessionEvent> {
    let cx = PhaseCx::at(now);
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, cx, &mut out);
    }
    out
}

/// Updates until nothing is left to resolve: nodes committed, then lanes.
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

/// `500 + 1000 · depth · sin(2π · rate · t)` at frame `f`: a bipolar linear
/// sine route on a `0..=1000` target with knob 500.
fn lfo_value(depth: f64, rate: f64, f: usize) -> f64 {
    500.0 + 1000.0 * depth * (std::f64::consts::TAU * rate * f as f64 / RATE).sin()
}

/// Doc 014 §5.3's bound for a linear sine at `rate` on the 32-frame grid,
/// times the swing, plus `f32` rounding of values near 1000.
fn lfo_tolerance(depth: f64, rate: f64) -> f64 {
    let h = std::f64::consts::TAU * rate * STEP as f64 / RATE;
    h * h / 8.0 * 1000.0 * depth + 1e-3
}

/// A follower's analytic level (attack `ta`, release `tr`) of a 0.8 pulse
/// on frames `4800..7200`, one control step late and linear between grid
/// points (doc 014 §6), ducking a `0..=1` param with knob 0.8 at depth −0.6.
fn ducked(f: usize, ta: f64, tr: f64) -> f64 {
    let pole = |tau: f64| (-1.0 / (tau * RATE)).exp();
    let (ca, cr) = (pole(ta), pole(tr));
    let env = |g: usize| -> f64 {
        if g <= 4_800 {
            0.0
        } else if g <= 7_200 {
            0.8 * (1.0 - ca.powi(g as i32 - 4_800))
        } else {
            0.8 * (1.0 - ca.powi(2_400)) * cr.powi(g as i32 - 7_200)
        }
    };
    let g = f / STEP * STEP;
    let x = if g < STEP {
        0.0
    } else {
        let (a, b) = (env(g - STEP), env(g));
        a + (b - a) * (f - g) as f64 / STEP as f64
    };
    0.8 - 0.6 * x
}

/// **Doc 014 §8.1, headless (design 015 §5.1), renders its analytic figures on an offline
/// session.** A sine LFO of one cycle per half beat on a cutoff (knob 500
/// in `0..=1000`, depth 0.3) renders the analytic sine, bar-locked at 120
/// BPM (4 Hz), within §5.3's bound past the connect's declick; and the pad
/// (knob 0.8 in `0..=1`) dips with the kick through a follower (5 ms /
/// 100 ms) at depth −0.6, analytically.
#[test]
fn the_8_1_example_renders_its_analytic_values() {
    let mut s = offline_48k();
    let filter = s.graph().add(ProbeUnit::new(500.0));
    let pad = s.graph().add(ProbeUnit::of(UnitParam::Volume, 0.8));
    let kick_bus = s.graph().add(ForkByClone(Pulse {
        from: 4_800,
        to: 7_200,
        level: 0.8,
    }));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(filter, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(pad, 0)])
        .unwrap()
        .unfaded();

    let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
    let gain = ParamRef::unit(pad, UnitParam::Volume);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    m.range_raw(gain, 0.0, 1.0).unwrap();
    let wobble = m.lfo(Lfo::sine().rate(BeatsPerCycle::of(0.5)));
    m.route(RouteSpec::new(wobble, cutoff, Bipolar::of(0.3)))
        .unwrap();
    let kick = m.follower(common::meter(), kick_bus);
    m.route(RouteSpec::new(kick, gain, Bipolar::of(-0.6)))
        .unwrap(); // sidechain
    settle(&mut s);

    // Rolling from beat 0 at frame 0 (120 BPM, the transport's default).
    s.play().expect("room");
    let (total, block) = (24_000, 128);
    let mut all = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0f32; block * 2];
    for _ in 0..total / block {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        for frame in buf.as_chunks::<2>().0 {
            all[0].push(frame[0]);
            all[1].push(frame[1]);
        }
    }
    let tol = lfo_tolerance(0.3, 4.0);
    for (f, &v) in all[0].iter().enumerate().skip(DECLICK) {
        let want = lfo_value(0.3, 4.0, f);
        assert!(
            (f64::from(v) - want).abs() < tol,
            "cutoff, frame {f}: {v} vs {want}"
        );
    }
    for (f, &v) in all[1].iter().enumerate() {
        let want = ducked(f, 0.005, 0.1);
        assert!(
            (f64::from(v) - want).abs() < 1e-4,
            "pad, frame {f}: {v} vs {want}"
        );
    }
    let low = all[1].iter().copied().fold(f32::INFINITY, f32::min);
    assert!(low < 0.4, "the pad dips with the kick: {low}");
}

/// **The unresolved warning fires after 4 s of `now`, not after N
/// updates.** A target whose key names no node stays unresolved: a
/// thousand updates at one instant report nothing, nor does one at 3.9 s;
/// the one at 4 s reports it, once, and later ones do not repeat it.
#[test]
fn the_unresolved_warning_fires_after_four_seconds_not_n_updates() {
    let mut s = offline_48k();
    let ghost = ParamRef::unit(NodeKey(999), UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(ghost, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(3.0)));
    m.route(RouteSpec::new(lfo, ghost, Bipolar::of(0.3)))
        .unwrap();
    let t0 = Instant::now();
    let warnings = |events: Vec<SessionEvent>| -> Vec<(ModTarget, Duration)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                SessionEvent::Modulation(Problem::Unresolved {
                    target: TargetName::Own(target),
                    waited,
                }) => Some((target, waited)),
                _ => None,
            })
            .collect()
    };

    for _ in 0..1_000 {
        assert_eq!(warnings(update_at(&mut s, t0)), vec![]);
    }
    let early = t0 + Duration::from_millis(3_900);
    assert_eq!(warnings(update_at(&mut s, early)), vec![]);
    let due = t0 + Duration::from_secs(4);
    assert_eq!(
        warnings(update_at(&mut s, due)),
        vec![(ModTarget::Param(ghost), Duration::from_secs(4))],
        "reported once it has waited 4 s"
    );
    for later in 1..10 {
        let at = due + Duration::from_secs(later);
        assert_eq!(warnings(update_at(&mut s, at)), vec![], "and once only");
    }
}

/// **A refusal names the node and the param** (doc 030 T5): a route whose
/// target lost its range (`clear_range` after the route was made) is refused
/// by the compile, and the `Problem` says which param of which node, in its
/// `Display`. Reported once, held while it lasts, and cleared when the range
/// returns.
#[test]
fn a_refusal_names_the_node_and_the_param() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    assert!(m.clear_range(cutoff));

    let refused: Vec<Problem> = s
        .update()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Modulation(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(refused.len(), 1, "reported once: {refused:?}");
    let Problem::Uncompilable { at, .. } = &refused[0] else {
        panic!("a refusal: {:?}", refused[0]);
    };
    assert_eq!(*at, Some(TargetName::Own(cutoff.into())));
    let text = refused[0].to_string();
    assert!(
        text.contains("Cutoff") && text.contains(&format!("node {}", probe.erase().0)),
        "names the param and its node: {text}"
    );
    assert_eq!(s.mod_state().problems().collect::<Vec<_>>(), [&refused[0]]);
    assert!(
        !s.update()
            .iter()
            .any(|e| matches!(e, SessionEvent::Modulation(_))),
        "not repeated while it lasts"
    );

    s.mods_mut().range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let _ = s.update();
    assert_eq!(
        s.mod_state().problems().next(),
        None,
        "cleared once it applies"
    );
}

/// **A route onto a target with no range is refused when it is made**, naming
/// the target, and adds nothing: it used to be accepted, and the next apply
/// refused with `UndeclaredTarget` and held back every later edit. Once the
/// range is declared the same call succeeds, and the update applies clean.
#[test]
fn a_route_onto_an_unranged_target_is_refused_at_the_call() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    assert_eq!(
        m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5))),
        Err(RouteError::UndeclaredRange {
            target: cutoff.into()
        })
    );
    let events = s.update();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Modulation(_))),
        "nothing was added to refuse: {events:?}"
    );
    assert_eq!(s.mod_state().problems().next(), None);

    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(
        m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
            .is_ok()
    );
    let _ = s.update();
    assert_eq!(s.mod_state().problems().next(), None, "applies clean");
}

/// **The graph's own removal takes the session's declarations too**, as
/// `Session::remove` does: a route into a node removed through
/// `s.graph().remove` is gone after the next update, and nothing is reported
/// for it (before, the route stayed and `Problem::Unresolved` repeated on
/// every update).
#[test]
fn removing_a_node_through_the_graph_forgets_its_declarations() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);

    s.remove(probe).unwrap();
    for _ in 0..8 {
        let _ = s.update();
    }
    assert_eq!(s.mods().routes().count(), 0, "the route into it went");
    assert_eq!(s.mod_state().problems().next(), None, "nothing to warn of");
    assert!(s.mod_state().lane(cutoff.key()).is_none(), "its lane went");
}

/// **A removed node takes the session's declarations on it**: its range
/// and the route into it go with it, so nothing is left to refuse or to
/// warn about.
#[test]
fn removing_a_node_forgets_its_declarations() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_some());

    s.remove(probe).unwrap();
    settle(&mut s);
    assert!(s.mod_state().lane(cutoff.key()).is_none(), "its lane went");
}

/// The key a session `ParamRef` names is its node's key bits.
#[test]
fn a_param_ref_round_trips_through_its_key() {
    let node = NodeKey(42);
    let p = ParamRef::unit(node, UnitParam::Cutoff);
    assert_eq!(p.key().node(), Some(42));
    assert_eq!(ParamRef::from_key(p.key()), Some(p));
    assert_eq!(
        ParamRef::from_key(TargetKey::edge(42)),
        None,
        "an edge is no param"
    );
}

/// A source declaring `LATE` frames of latency: a look-ahead's bookkeeping.
#[derive(Clone)]
struct Late;

const LATE: usize = 100;

impl tutti_graph::Node for Late {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
            .latency(tutti_units::Latency::new(Samples::new(LATE)))
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.5);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A lane to a node added in the same update goes out in that update's one
/// plan, and its latency is published with it.** A follower of a latent node
/// (`LATE` frames) is routed to a probe inserted in the same update, the probe
/// alone on output 0. The apply binds the lane to the probe's staged cells, so
/// the one commit sends the probe, the follower and the lane: that plan has
/// `LATE` frames on output 0 (the follower's path in front of the probe's
/// param port), and the session publishes its total before the update
/// returns. Exactly one plan is sent.
#[test]
fn a_lane_to_a_node_added_in_the_update_sends_one_plan_with_its_latency() {
    let mut s = offline_48k();
    let late = s.graph().add(ForkByClone(Late));
    let _ = s.update();
    assert_eq!(
        s.latency(),
        Samples::new(0),
        "precondition: nothing latent out"
    );

    let probe = s.graph().add(ProbeUnit::new(500.0));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(probe, 0)])
        .unwrap()
        .unfaded();
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let follower = m.follower(common::meter(), late);
    m.route(RouteSpec::new(follower, cutoff, Bipolar::of(0.5)))
        .unwrap();

    let events = s.update();
    assert_eq!(s.mod_state().problems().next(), None, "the lane is built");
    assert_eq!(committed(&events), 1, "one plan for the node and its lane");
    assert_eq!(s.latency(), Samples::new(LATE), "the plan's latency");
}

/// **The update frees a lane's program a block held across its
/// replacement**: a route's polarity edited while a borrow on the lane's cell
/// (a block in progress) is held republishes the lane's program, and once
/// the borrow goes the next update's `Reclaim` phase frees the old one with
/// nothing published again. The lanes are inserted by the apply, not
/// through `Graph::add`, so the session sweeps them itself.
#[test]
fn an_update_frees_a_program_a_block_held_across_a_route_edit() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    let route = m
        .route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    let (_, lane) = s.mod_state().lane(cutoff.key()).expect("the lane");
    let lane = lane.clone();
    assert_eq!(lane.retired_programs(), 0, "settled");

    let mut block = lane.probe_reader();
    {
        let _held = block.lend();
        s.set(route.polarity().to(Polarity::Unipolar)).unwrap();
        let _ = s.update();
        assert_eq!(lane.retired_programs(), 1, "republished, the old held");
    }
    let _ = s.update();
    assert_eq!(lane.retired_programs(), 0, "the update freed it");
}

/// **The update frees the program a block held across a republish of a lane
/// the app added with `Graph::add`**: the session sweeps only the lanes it
/// wired, so the node's own reclaimer is what frees this one.
#[test]
fn an_update_frees_a_program_a_block_held_across_a_hand_added_lanes_republish() {
    let mut s = offline_48k();
    let node = ControlLaneNode::from(UnitLaneConfig::new(
        UnitParam::Cutoff,
        ControlProgram::default(),
    ));
    let lane = node.controls();
    let _key = s.graph().add(node);
    let _ = s.update();

    let mut block = lane.probe_reader();
    {
        let _held = block.lend();
        lane.publish(ControlProgram::default());
        assert_eq!(lane.retired_programs(), 1, "republished, the old held");
    }
    let _ = s.update();
    assert_eq!(lane.retired_programs(), 0, "the update freed it");
}

/// **An edit through `Session::mods` after a steady state applies.** The
/// session keeps its own declarations built between edits, and an
/// update skips an apply whose inputs are all unchanged; a route edit,
/// a range edit and a new route each reach the graph on the next update.
#[test]
fn an_own_edit_after_a_steady_state_applies() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range(probe.cutoff(), Hz::new(0.0)..Hz::new(1_000.0))
        .unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    let route = m
        .route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    for _ in 0..4 {
        let _ = s.update();
    }
    let program = |s: &Session| {
        s.mod_state()
            .lane(cutoff.key())
            .expect("a lane")
            .1
            .program()
    };
    let before = program(&s);

    s.set(route.polarity().to(Polarity::Unipolar)).unwrap();
    let _ = s.update();
    assert!(
        !std::sync::Arc::ptr_eq(&before, &program(&s)),
        "the polarity edit is republished"
    );

    s.set(probe.cutoff().range().to(Hz::new(0.0)..Hz::new(2_000.0)))
        .unwrap();
    let _ = s.update();
    let range = s.mods().range_of(probe.cutoff()).expect("a declared range");
    assert_eq!(*range.end(), 2_000.0, "the range edit is stored");
}

/// **A steady update applies nothing.** A lane on a probe's cutoff, settled:
/// every further update sends no plan, leaves the lane's published program as
/// it was and reports nothing. After an update whose apply committed (a
/// follower routed in), the next one is as steady as any.
#[test]
fn a_steady_update_sends_nothing_and_publishes_nothing() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let other = s.graph().add(ProbeUnit::new(0.1));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    let read = |s: &Session| {
        s.mod_state()
            .lane(cutoff.key())
            .expect("a lane")
            .1
            .program()
    };
    let steady = |s: &mut Session, program: &std::sync::Arc<_>| {
        for _ in 0..8 {
            let events = s.update();
            assert!(
                events.is_empty(),
                "no plan was sent, and nothing to report: {events:?}"
            );
            assert!(
                std::sync::Arc::ptr_eq(&read(s), program),
                "no program was published"
            );
        }
    };
    let before = read(&s);
    steady(&mut s, &before);

    let mut m = s.mods_mut();
    let follower = m.follower(common::meter(), other);
    m.route(RouteSpec::new(follower, cutoff, Bipolar::of(0.25)))
        .unwrap();
    let events = s.update();
    let applied = read(&s);
    assert_eq!(
        committed(&events),
        1,
        "the follower's node and edge were committed"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&applied, &before),
        "and the lane's program published"
    );
    steady(&mut s, &applied);
}

/// **A play start applies again.** An LFO's rate edit while stopped is dated
/// from an anchor that is only valid within the play epoch: after the edit
/// applies and settles, starting the transport moves the epoch, and the next
/// update republishes the lane's program without the anchor.
#[test]
fn a_play_start_republishes_an_anchored_program() {
    let mut s = offline_48k();
    let probe = s.graph().add(ProbeUnit::new(500.0));
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    m.route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    settle(&mut s);
    s.set(lfo.rate().to(Hz::new(3.0))).unwrap();
    let _ = s.update();
    let program = |s: &Session| {
        s.mod_state()
            .lane(cutoff.key())
            .expect("a lane")
            .1
            .program()
    };
    let anchored = program(&s);
    let _ = s.update();
    assert!(std::sync::Arc::ptr_eq(&anchored, &program(&s)), "steady");

    s.play().expect("room");
    let mut buf = vec![0.0f32; 256];
    s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    let _ = s.update();
    assert!(
        !std::sync::Arc::ptr_eq(&anchored, &program(&s)),
        "the play start dropped the anchor from the program"
    );
}
