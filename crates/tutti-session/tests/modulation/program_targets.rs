//! A modulation program's values (`route.polarity()`, `route.curve()`,
//! `lfo.rate()`, `macro_.value()`): a write is staged, the update publishes it
//! as one republish of the lanes it reaches, and a lane that republishes keeps
//! its LFO's phase.

use std::sync::Arc;

use tutti_modulation::{
    Law, LayerClock, Lfo, LfoRate, Polarity, ProgramTime, RATE_ANCHOR_MARGIN, Waveform, shape,
};
use tutti_nodes::automation::{Breakpoint, Trajectory};
use tutti_session::{
    DepthTarget, ModRouteId, ModSourceId, ParamRef, RouteSpec, SourceError, ValueErrorKind,
};
use tutti_units::{Beat, BeatsPerCycle, Bipolar, Hz, SamplePosition, SampleRate, UnitParam};

use crate::common::probe::ProbeUnit;
use crate::mod_lanes::{frame, main_output, render, rig, settle};

const RATE: f64 = 48_000.0;

/// A cutoff probe (knob 500, range `0..=1000`) on output 0 with `source`
/// routed into it at `depth`, settled and rendered once. Returns the param and
/// the route.
fn scene(
    s: &mut tutti_session::Session,
    source: impl FnOnce(&mut tutti_session::Mods<'_>) -> ModSourceId,
    depth: f64,
) -> (ParamRef, ModSourceId, ModRouteId) {
    let probe = s.graph().add(ProbeUnit::new(500.0));
    main_output(s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    let source = source(&mut m);
    let route = m
        .route(RouteSpec::new(source, cutoff, Bipolar::of(depth)))
        .unwrap();
    settle(s);
    render(s, 1_024, 128);
    (cutoff, source, route)
}

/// A trajectory in `Hz` that moves linearly from `from` at beat 0 to `to` at
/// beat 4.
fn ramp(from: f64, to: f64) -> Trajectory<Hz> {
    let mut curve = Trajectory::new();
    curve.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(from)));
    curve.add_breakpoint(Breakpoint::new(Beat::new(4.0), Hz::new(to)));
    curve
}

/// The program the lane on `cutoff` holds now.
fn program(s: &tutti_session::Session, cutoff: ParamRef) -> Arc<tutti_modulation::ControlProgram> {
    let (_, lane) = s.mod_state().lane(cutoff.key()).expect("a lane");
    lane.program()
}

/// The clock of the one layer of the lane's program.
fn clock(s: &tutti_session::Session, cutoff: ParamRef) -> LayerClock {
    program(s, cutoff).layers()[0].clock
}

/// **A route's polarity and curve land at the update, as the figure the shape
/// gives.** A macro at 0.5 on a route of depth 0.4 over a span of 1000:
/// bipolar linear offsets by `0.4 * 0.5 * 1000` = 200; unipolar maps 0.5 to
/// 0.75 and offsets by 300; bipolar `Hold` below full scale offsets by 0.
#[test]
fn a_route_polarity_and_curve_land_at_the_update_as_the_shaped_offset() {
    let mut s = rig(1);
    let (cutoff, _, route) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    // The route's depth is the lane's cell, an `f32` in the program: its
    // offsets carry that rounding.
    let offset = |s: &tutti_session::Session| {
        let amount = program(s, cutoff).layers()[0].amount;
        tutti_modulation::Amount {
            depth: s.mods().route(route).expect("the route").depth,
            ..amount
        }
        .offset(0.5f32, 1_000.0)
    };
    // The depth is stored as an `f32`: a few ulps of the figure's own size.
    let near = |got: f64, want: f64| {
        let bound = 4.0 * f64::from(f32::EPSILON) * want.abs().max(1.0);
        assert!((got - want).abs() < bound, "{got} is not {want}");
    };
    near(offset(&s), 200.0);

    s.set(route.polarity().to(Polarity::Unipolar)).unwrap();
    near(offset(&s), 200.0);
    let _ = s.update();
    near(offset(&s), 300.0);
    let amount = program(&s, cutoff).layers()[0].amount;
    assert_eq!(amount.polarity, Polarity::Unipolar);
    assert_eq!(
        shape(0.5f32, Bipolar::of(0.4), amount.polarity, amount.curve),
        0.3f32,
        "unipolar 0.5 is 0.75, at depth 0.4"
    );

    s.set((
        route.polarity().to(Polarity::Bipolar),
        route.curve().to(Law::Hold),
    ))
    .unwrap();
    let _ = s.update();
    assert_eq!(offset(&s), 0.0);
    assert_eq!(program(&s, cutoff).layers()[0].amount.curve, Law::Hold);
}

/// **A read before the update is the pending value, and the lane still holds
/// the one in force.**
#[test]
fn a_get_before_the_update_is_the_pending_value() {
    let mut s = rig(1);
    let (cutoff, lfo, route) = scene(&mut s, |m| m.lfo(Lfo::sine().rate(Hz::new(2.0))), 0.5);
    let before = program(&s, cutoff);
    assert_eq!(s.get(route.polarity()).unwrap(), Polarity::Bipolar);
    assert_eq!(s.get(route.curve()).unwrap(), Law::Linear);
    assert_eq!(s.get(lfo.rate()).unwrap(), LfoRate::Hz(Hz::new(2.0)));

    s.set((
        route.polarity().to(Polarity::Unipolar),
        lfo.rate().to(LfoRate::Hz(Hz::new(5.0))),
    ))
    .unwrap();
    assert_eq!(s.get(route.polarity()).unwrap(), Polarity::Unipolar);
    assert_eq!(s.get(lfo.rate()).unwrap(), LfoRate::Hz(Hz::new(5.0)));
    assert!(
        Arc::ptr_eq(&program(&s, cutoff), &before),
        "nothing is published before the update"
    );

    let _ = s.update();
    assert!(!Arc::ptr_eq(&program(&s, cutoff), &before), "published");
    assert_eq!(s.get(route.polarity()).unwrap(), Polarity::Unipolar);
    assert_eq!(s.get(lfo.rate()).unwrap(), LfoRate::Hz(Hz::new(5.0)));
}

/// **Two writes before an update publish the second, and a pair that ends
/// where it began publishes nothing.**
#[test]
fn two_writes_before_an_update_publish_the_second_value_once() {
    let mut s = rig(1);
    let (cutoff, _, route) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    let settled = program(&s, cutoff);

    s.set(route.polarity().to(Polarity::Unipolar)).unwrap();
    s.set(route.polarity().to(Polarity::Bipolar)).unwrap();
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&program(&s, cutoff), &settled),
        "the first write never reached the lane, and the second changes nothing"
    );

    s.set(route.curve().to(Law::Hold)).unwrap();
    s.set(route.curve().to(Law::Smootherstep)).unwrap();
    let _ = s.update();
    assert_eq!(
        program(&s, cutoff).layers()[0].amount.curve,
        Law::Smootherstep
    );
    let published = program(&s, cutoff);
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&program(&s, cutoff), &published),
        "an update with nothing written republishes nothing"
    );
}

/// **A rate set lands at the update with the LFO's phase continuing.** A 2 Hz
/// sine set to 5 Hz is dated by an anchor, at which the layer is where 2 Hz
/// left it (`2 * at / rate` cycles, not 0) and from which it advances 5
/// cycles a second: the layer's cycles are continuous across the anchor and
/// gain 5 over the next second.
#[test]
fn an_lfo_rate_set_lands_at_the_update_and_the_phase_continues() {
    let mut s = rig(1);
    let (cutoff, lfo, _) = scene(&mut s, |m| m.lfo(Lfo::sine().rate(Hz::new(2.0))), 0.5);
    assert!(matches!(
        clock(&s, cutoff),
        LayerClock::Seconds {
            rate,
            anchor: None,
            ..
        } if rate == Hz::new(2.0)
    ));

    s.set(lfo.rate().to(LfoRate::Hz(Hz::new(5.0)))).unwrap();
    frame(&mut s);
    let LayerClock::Seconds {
        rate,
        anchor: Some(anchor),
        ..
    } = clock(&s, cutoff)
    else {
        panic!("the new rate is dated by an anchor");
    };
    assert_eq!(rate, Hz::new(5.0));
    assert_eq!(anchor.prev_rate, Hz::new(2.0));
    let at = anchor.at.get() as f64;
    let played = 1_024.0; // rendered before the update that dates the anchor
    assert!(
        at >= played + RATE_ANCHOR_MARGIN.get() as f64,
        "the anchor is a margin past the frames already played"
    );
    assert!(
        (anchor.phase - 2.0 * at / RATE).abs() < 1e-9,
        "the anchor holds where 2 Hz left the phase: {} cycles",
        anchor.phase
    );

    let layer = program(&s, cutoff).layers()[0].clone();
    let cycles = |frame: f64| {
        layer
            .clock
            .cycles(&ProgramTime {
                beat: Beat::new(0.0),
                play: SamplePosition::new(frame),
                play_epoch: anchor.epoch,
                rate: SampleRate::of(RATE),
            })
            .unwrap()
    };
    assert!((cycles(at - 4_800.0) - 2.0 * (at - 4_800.0) / RATE).abs() < 1e-9);
    assert!((cycles(at) - 2.0 * at / RATE).abs() < 1e-9, "continuous");
    assert!(
        (cycles(at + RATE) - cycles(at) - 5.0).abs() < 1e-9,
        "5 cycles in the next second"
    );
}

/// **A rate in beats per cycle replaces the clock and keeps its phase offset:
/// the layer is at `beat / per_cycle` cycles.**
#[test]
fn an_lfo_rate_in_beats_per_cycle_is_bar_locked() {
    let mut s = rig(1);
    let (cutoff, lfo, _) = scene(
        &mut s,
        |m| m.lfo(Lfo::sine().phase(tutti_units::PhaseIncrement::new(0.25))),
        0.5,
    );
    s.set(lfo.rate().to(LfoRate::Beats(BeatsPerCycle::of(4.0))))
        .unwrap();
    let _ = s.update();
    let clock = clock(&s, cutoff);
    let at = |beat: f64| ProgramTime {
        beat: Beat::new(beat),
        play: SamplePosition::new(0.0),
        play_epoch: 0,
        rate: SampleRate::of(RATE),
    };
    assert!((clock.cycles(&at(2.0)).unwrap() - 0.75).abs() < 1e-12);
    assert!((clock.cycles(&at(6.0)).unwrap() - 1.75).abs() < 1e-12);
}

/// **A change of beats per cycle is bar-locked, with no anchor**: the layer is
/// at `beat / per_cycle` cycles on either side of the update, so the phase at
/// a beat follows the new span at once.
#[test]
fn an_lfo_beats_rate_change_is_bar_locked_with_no_anchor() {
    let mut s = rig(1);
    let (cutoff, lfo, _) = scene(
        &mut s,
        |m| m.lfo(Lfo::sine().rate(BeatsPerCycle::of(4.0))),
        0.5,
    );
    s.set(lfo.rate().to(LfoRate::Beats(BeatsPerCycle::of(2.0))))
        .unwrap();
    let _ = s.update();
    let clock = clock(&s, cutoff);
    assert!(
        matches!(clock, LayerClock::Beats { .. }),
        "no anchor on a beats clock"
    );
    let at = ProgramTime {
        beat: Beat::new(3.0),
        play: SamplePosition::new(0.0),
        play_epoch: 0,
        rate: SampleRate::of(RATE),
    };
    assert!((clock.cycles(&at).unwrap() - 1.5).abs() < 1e-12);
}

/// **A macro stays a constant when its value is set, so its lane stays steady**:
/// every layer is a `Waveform::Const` of the new value, which is what keeps the
/// lane from walking a grid.
#[test]
fn a_macro_set_keeps_the_lane_steady() {
    let mut s = rig(1);
    let (cutoff, knob, _) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    assert!(program(&s, cutoff).layers().iter().all(|l| l.is_constant()));

    s.set(knob.value().to(Bipolar::of(-0.25))).unwrap();
    assert_eq!(s.get(knob.value()).unwrap(), Bipolar::of(-0.25), "pending");
    assert_eq!(program(&s, cutoff).layers()[0].wave, Waveform::Const(0.5));
    let _ = s.update();
    let program = program(&s, cutoff);
    assert_eq!(program.layers().len(), 1);
    assert!(program.layers().iter().all(|l| l.is_constant()), "steady");
    assert_eq!(program.layers()[0].wave, Waveform::Const(-0.25));
    assert_eq!(s.get(knob.value()).unwrap(), Bipolar::of(-0.25));
}

/// **A target names what the source is.** A macro has a value and no rate, an
/// LFO a rate and no value, and a route or source that was removed has none: a
/// call that names one fails at the call, and `set_source` refuses a source
/// of another kind, so a macro keeps its value target.
#[test]
fn a_target_the_source_does_not_have_is_refused_at_the_call() {
    let mut s = rig(1);
    let (cutoff, knob, route) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    fn refused<T>(r: Result<T, tutti_session::ValueError<&'static str>>) -> ValueErrorKind {
        r.err().expect("refused").kind
    }
    assert_eq!(
        refused(s.set(knob.rate().to(LfoRate::Hz(Hz::new(1.0))))),
        ValueErrorKind::NoSuchTarget
    );

    assert!(matches!(
        s.mods_mut().set_source(knob, Lfo::sine()),
        Err(SourceError::ChangesKind { .. })
    ));
    s.set(knob.value().to(Bipolar::ZERO))
        .expect("still a macro");
    assert_eq!(
        refused(s.set(knob.rate().to(LfoRate::Hz(Hz::new(1.0))))),
        ValueErrorKind::NoSuchTarget,
        "still no rate"
    );
    let _ = s.update();
    assert!(matches!(
        program(&s, cutoff).layers()[0].wave,
        Waveform::Const(_)
    ));

    s.mods_mut().remove_route(route).unwrap();
    assert_eq!(
        refused(s.set(route.curve().to(Law::Hold))),
        ValueErrorKind::NoSuchTarget
    );
    s.mods_mut().remove_source(knob).unwrap();
    assert_eq!(
        refused(s.set(knob.rate().to(LfoRate::Hz(Hz::new(1.0))))),
        ValueErrorKind::NoSuchTarget
    );
}

/// **A block that adopts a republished program or automation curve allocates
/// nothing**: the writes and the update run outside the gate, and the audio
/// side only lends.
#[test]
fn blocks_adopting_a_republished_program_and_curve_do_not_allocate() {
    let mut s = tutti_session::Session::builder()
        .max_block(tutti_buffers::MaxBlock::of(tutti_units::Samples::new(128)))
        .build()
        .expect("builds");
    let probe = s.graph().add(ProbeUnit::new(500.0));
    s.graph()
        .set_sources(
            tutti_session::MainOutput::channel(0),
            [tutti_session::OutputPort::new(probe, 0)],
        )
        .unwrap();
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(m.automate(cutoff, Arc::new(ramp(100.0, 900.0))));
    let lfo = m.lfo(Lfo::sine().rate(Hz::new(2.0)));
    let route = m
        .route(RouteSpec::new(lfo, cutoff, Bipolar::of(0.5)))
        .unwrap();
    for _ in 0..4 {
        let _ = s.update();
    }
    let mut block = vec![0.0f32; 2 * 128];
    let render = |s: &mut tutti_session::Session, block: &mut Vec<f32>| {
        s.render(
            &mut tutti_buffers::InterleavedMut::new(block, tutti_buffers::ChannelLayout::STEREO)
                .unwrap(),
        );
    };
    render(&mut s, &mut block);
    let before = block.clone();

    s.set((
        route.polarity().to(Polarity::Unipolar),
        route.curve().to(Law::Hold),
        lfo.rate().to(LfoRate::Hz(Hz::new(7.0))),
    ))
    .unwrap();
    s.set(probe.cutoff().automation().to(Arc::new(ramp(900.0, 100.0))))
        .unwrap();
    let _ = s.update();
    for _ in 0..8 {
        render(&mut s, &mut block);
    }
    // The probe is on output 0 of a stereo mix: the last frame's left sample.
    // The transport is stopped at beat 0. The route is `Hold`, which offsets
    // nothing below full scale, so the cutoff is the new ramp's first value,
    // 900, within the `f32` the block holds.
    let want = 900.0f64;
    let got = f64::from(block[2 * 127]);
    assert!(
        (got - want).abs() < 4.0 * f64::from(f32::EPSILON) * want,
        "the republished curve and program are adopted: {got} is not {want}"
    );
    // The program before the update read at most 100 + 0.5 * 1000 there.
    assert!(
        f64::from(before[2 * 127]) <= 600.0 + 1e-3,
        "the block before the update is on the old program"
    );
}

/// **An automation curve set lands at the update, and the param's value at a
/// beat follows it.** A cutoff automated from 100 Hz at beat 0 to 900 Hz at
/// beat 4 reads 500 at beat 2; set to a ramp from 900 down to 100 it reads 500
/// at beat 2 and 700 at beat 1. A read before the update is the pending
/// curve, and a param with no automation has no target.
#[test]
fn an_automation_curve_set_lands_at_the_update_and_the_param_follows_it() {
    let mut s = rig(1);
    let probe = s.graph().add(ProbeUnit::new(300.0));
    main_output(&mut s, &[probe.erase()]);
    let cutoff = ParamRef::unit(probe, UnitParam::Cutoff);
    let target = probe.cutoff().automation();
    assert_eq!(
        s.get(target).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget,
        "no lane declared"
    );
    let first = Arc::new(ramp(100.0, 900.0));
    let mut m = s.mods_mut();
    m.range_raw(cutoff, 0.0, 1_000.0).unwrap();
    assert!(m.automate(cutoff, Arc::clone(&first)));
    assert!(
        !m.automate(cutoff, Arc::new(ramp(0.0, 0.0))),
        "a declared lane keeps its curve: the target sets it"
    );
    settle(&mut s);
    let at = |s: &tutti_session::Session, beat: f64| {
        let program = program(s, cutoff);
        program
            .automation()
            .expect("automated")
            .value_at(Beat::new(beat))
    };
    assert_eq!(at(&s, 2.0), Some(500.0));
    assert_eq!(s.get(target).unwrap(), first);

    let second = Arc::new(ramp(900.0, 100.0));
    s.set(target.to(Arc::clone(&second))).unwrap();
    assert_eq!(s.get(target).unwrap(), second, "pending");
    assert_eq!(at(&s, 1.0), Some(300.0), "the lane still plays the first");
    let _ = s.update();
    assert_eq!(at(&s, 1.0), Some(700.0));
    assert_eq!(at(&s, 2.0), Some(500.0));
    assert_eq!(s.get(target).unwrap(), second);

    s.mods_mut().clear_automation(cutoff);
    assert_eq!(
        s.set(target.to(first)).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget,
        "a cleared lane has no target"
    );
}

/// **A depth set lands at once, with no republish**: the lane reads the depth
/// from a cell, so the lane's program is the same `Arc` and no update has run
/// when the next block carries the new offset. A macro at 0.5 on a route of
/// depth 0.4 over a span of 1000 offsets the knob (500) by
/// `0.4 * 0.5 * 1000 = 200`; set to -0.2 the offset is `-100`.
#[test]
fn a_depth_set_lands_at_once_with_no_republish() {
    let mut s = rig(1);
    let (cutoff, source, route) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    let before = program(&s, cutoff);
    let tail = |s: &mut tutti_session::Session| *render(s, 1_024, 128)[0].last().unwrap();
    let near = |got: f32, want: f32| {
        assert!((got - want).abs() < 1e-3, "{got} is not {want}");
    };
    near(tail(&mut s), 700.0);

    let depth = DepthTarget::of(source, cutoff);
    assert_eq!(s.get(depth).unwrap(), Bipolar::of(0.4));
    s.set(depth.to(Bipolar::of(-0.2))).unwrap();
    assert_eq!(s.get(depth).unwrap(), Bipolar::of(-0.2));
    assert_eq!(s.mods().route(route).unwrap().depth, Bipolar::of(-0.2));
    // No update ran: the next block is the new figure.
    near(tail(&mut s), 400.0);
    assert!(
        Arc::ptr_eq(&program(&s, cutoff), &before),
        "the program was not republished"
    );
    let _ = s.update();
    assert!(
        Arc::ptr_eq(&program(&s, cutoff), &before),
        "nor by the update"
    );
    near(tail(&mut s), 400.0);

    // Inverted and zero: the offset follows the depth.
    s.set(depth.to(Bipolar::of(-0.4))).unwrap();
    near(tail(&mut s), 300.0);
    s.set(depth.to(Bipolar::ZERO)).unwrap();
    near(tail(&mut s), 500.0);
}

/// **A depth target names its route by source and target**, so a pair with no
/// route (never declared, or removed) is `NoSuchTarget`, and a second route
/// between the pair is refused where it is declared.
#[test]
fn a_depth_target_is_named_by_its_source_and_target() {
    let mut s = rig(1);
    let (cutoff, source, route) = scene(&mut s, |m| m.macro_(0.5), 0.4);
    let other = s.mods_mut().macro_(0.0);
    assert_eq!(
        s.set(DepthTarget::of(other, cutoff).to(Bipolar::of(0.1)))
            .unwrap_err()
            .kind,
        ValueErrorKind::NoSuchTarget,
        "no route from this source"
    );
    assert!(matches!(
        s.mods_mut()
            .route(RouteSpec::new(source, cutoff, Bipolar::of(0.1))),
        Err(tutti_session::RouteError::Duplicate { .. })
    ));
    s.mods_mut().remove_route(route).unwrap();
    assert_eq!(
        s.set(DepthTarget::of(source, cutoff).to(Bipolar::of(0.1)))
            .unwrap_err()
            .kind,
        ValueErrorKind::NoSuchTarget,
        "the route is gone"
    );
}
