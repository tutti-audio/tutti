use super::*;
use crate::node::param_event::{ParamEvent, ParamRamp};
use crate::node::time::Offset;
use tutti_units::{Hz, ParamKey, Real, Samples};

/// A NaN at either end is no range; an infinite end is a range; `of` is
/// `new` for bounds that are not NaN.
#[test]
fn a_range_holds_no_nan_bound() {
    assert_eq!(ParamRange::new(f64::NAN, 1.0), None);
    assert_eq!(ParamRange::new(0.0, f64::NAN), None);
    assert_eq!(ParamRange::new(f64::NAN, f64::NAN), None);
    let open = ParamRange::new(f64::NEG_INFINITY, f64::INFINITY).expect("infinite bounds");
    assert_eq!(open, ParamRange::UNBOUNDED);
    let r = ParamRange::new(-1.0, 2.0).expect("finite bounds");
    assert_eq!(r, ParamRange::of(-1.0, 2.0));
    assert_eq!((r.min(), r.max()), (-1.0, 2.0));
}

/// `of` refuses a NaN bound by panicking, as a literal's error.
#[test]
#[should_panic(expected = "not NaN")]
fn of_panics_on_a_nan_bound() {
    let _ = ParamRange::of(f64::NAN, 0.0);
}

const AZ: Option<UnitParam> = Some(UnitParam::Azimuth);
const EL: Option<UnitParam> = Some(UnitParam::Elevation);

/// An arc across ±180 contains 180 (and its neighbours either side) and
/// not 0, where `new(170, -170)` ordered would read the opposite arc.
#[test]
fn an_arc_across_180_contains_180_and_not_0() {
    let behind = ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(20.0)).view(AZ);
    for v in [180.0, -180.0, 165.0, -165.0, 160.0, -160.0] {
        assert!(behind.contains(v), "{v}");
    }
    for v in [0.0, 90.0, -90.0, 159.0, -159.0] {
        assert!(!behind.contains(v), "{v}");
    }
    // The same bounds as a line are the opposite, wide range: the trap.
    let line = ParamRange::of(160.0, 200.0).view(EL);
    assert!(line.contains(180.0) && !line.contains(-180.0));
    assert_eq!(behind.span(), 40.0);
}

/// A circular range's span is its arc's length, the whole circle for an
/// unbounded or wide one; a line's is `max - min`.
#[test]
fn a_circular_ranges_span_is_its_arcs_length() {
    assert_eq!(ParamRange::UNBOUNDED.view(AZ).span(), 360.0);
    assert_eq!(ParamRange::of(-180.0, 180.0).view(AZ).span(), 360.0);
    let arc = |half| ParamRange::arc(Azimuth::new(0.0), ArcDegrees::new(half)).view(AZ);
    assert_eq!(arc(90.0).span(), 180.0);
    assert_eq!(arc(400.0).span(), 360.0);
    assert_eq!(ParamRange::of(-90.0, 90.0).view(EL).span(), 180.0);
    assert!(ParamRange::UNBOUNDED.view(EL).span().is_infinite());
    assert!(ParamRange::UNBOUNDED.view(None).span().is_infinite());
}

/// `constrain` wraps for a circle (190 is -170, not 180) and clamps for a
/// line; an arc holds to its nearer end.
#[test]
fn constrain_wraps_a_circle_and_clamps_a_line() {
    let r = ParamRange::of(-180.0, 180.0);
    assert_eq!(r.view(AZ).constrain(190.0), -170.0);
    assert_eq!(r.view(EL).constrain(190.0), 180.0);
    let arc = ParamRange::arc(Azimuth::new(90.0), ArcDegrees::new(30.0)).view(AZ);
    assert_eq!(arc.constrain(100.0), 100.0);
    assert_eq!(arc.constrain(170.0), 120.0);
    assert_eq!(arc.constrain(10.0), 60.0);
    assert_eq!(
        arc.constrain(-170.0),
        120.0,
        "the near end is the short way round"
    );
}

/// A ramp event starts on its frame and lands on its target on its last
/// frame; a zero-length one is a step on its frame.
#[test]
fn a_ramp_lands_on_its_last_frame() {
    let ev = |at: usize, t: f64, len: usize| {
        ParamEvent::ramp(
            Offset::of(Samples::new(at), Samples::new(16)),
            ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(t), Samples::new(len)),
        )
    };
    let events = [ev(4, 100.0, 4), ev(12, -5.0, 0)];
    let mut r = Ramp::default();
    let mut out = [0.0f32; 16];
    render_ramps(
        &mut r,
        &events,
        UnitParam::Cutoff.into(),
        &mut out,
        |_| 0.0,
        &mut Corners::new(&mut [0; 4096]),
    );
    assert_eq!(&out[..4], &[0.0; 4], "nothing before the event");
    assert_eq!(out[4], 25.0);
    assert_eq!(out[7], 100.0, "on target at the ramp's last frame");
    assert_eq!(out[11], 100.0, "held");
    assert_eq!(out[12], -5.0, "a step lands on its frame");
    // Another param's ramp is not this port's.
    let mut r = Ramp::default();
    render_ramps(
        &mut r,
        &events,
        UnitParam::Q.into(),
        &mut out,
        |_| 0.0,
        &mut Corners::new(&mut [0; 4096]),
    );
    assert!(out.iter().all(|&x| x == 0.0));
}

/// A segment and then a ramp on one frame, as a source's first events:
/// the ramp replaces the segment from the base at that frame, as it
/// would alone, under a base that moves every frame.
#[test]
fn a_ramp_after_a_first_segment_on_its_frame_starts_from_the_base() {
    use crate::node::param_event::ParamSegment;
    let at = Offset::of(Samples::new(4), Samples::new(8));
    let seg = ParamSegment::new(
        ParamKey::<Hz>::CUTOFF,
        Hz::new(5.0),
        Hz::new(9.0),
        Samples::new(20),
    );
    let ramp = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(0.0), Samples::new(2));
    let base = |i: usize| 1.0 + i as f32;
    let mut after_segment = [0.0f32; 8];
    let events = [ParamEvent::segment(at, seg), ParamEvent::ramp(at, ramp)];
    render_ramps(
        &mut Ramp::default(),
        &events,
        UnitParam::Cutoff.into(),
        &mut after_segment,
        base,
        &mut Corners::new(&mut [0; 4096]),
    );
    let mut alone = [0.0f32; 8];
    let events = [ParamEvent::ramp(at, ramp)];
    render_ramps(
        &mut Ramp::default(),
        &events,
        UnitParam::Cutoff.into(),
        &mut alone,
        base,
        &mut Corners::new(&mut [0; 4096]),
    );
    assert_eq!(after_segment, [1.0, 2.0, 3.0, 4.0, 2.5, 0.0, 0.0, 0.0]);
    assert_eq!(after_segment, alone, "as the ramp alone");
}

/// A linear ramp renders what the port rendered before ramps had a
/// shape, bit for bit: `from + (target - from) · (done / len)`, written
/// out here, at every frame of ramps of many lengths and spans.
#[test]
fn a_linear_ramp_renders_the_unshaped_bits() {
    for len in [1usize, 2, 3, 7, 10, 64, 97, 1000] {
        for (from, target) in [(0.0f64, 1.0f64), (0.3, -7.9), (440.0, 1234.5)] {
            // The port renders in `f64` and crosses to `f32` once a frame:
            // the oracle is the same line, from the base the lane gives it.
            let from32 = f32::from_unit(Hz::new(from));
            let start = f64::from(from32);
            let frames = len + 4;
            let events = [ParamEvent::ramp(
                Offset::of(Samples::new(1), Samples::new(frames)),
                ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(target), Samples::new(len)),
            )];
            let mut r = Ramp::default();
            let mut out = vec![0.0f32; frames];
            render_ramps(
                &mut r,
                &events,
                UnitParam::Cutoff.into(),
                &mut out,
                |_| from32,
                &mut Corners::new(&mut vec![0; frames]),
            );
            for (j, &v) in out[1..].iter().enumerate() {
                let done = j + 1;
                let want = f32::from_unit(Hz::new(if done >= len {
                    target
                } else {
                    start + (target - start) * (done as f64 / len as f64)
                }));
                assert_eq!(v.to_bits(), want.to_bits(), "len {len}, frame {j}");
            }
        }
    }
}

/// A list a node cannot declare is a named error, not a panic.
#[test]
fn an_undeclarable_list_is_a_named_error() {
    use UnitParam::*;
    let kinds = [
        Cutoff, Q, Drive, Wet, Threshold, Ratio, Attack, Release, Volume,
    ];
    assert_eq!(
        ParamPorts::try_new(&kinds).map(|p| p.len()),
        Ok(kinds.len())
    );
    let over = [Cutoff; MAX_PARAM_PORTS + 1];
    assert_eq!(
        ParamPorts::try_new(&over),
        Err(ParamPortsError::TooMany {
            count: MAX_PARAM_PORTS + 1
        })
    );
    assert_eq!(
        ParamPorts::try_new(&[Q, Cutoff, Q]),
        Err(ParamPortsError::Duplicate(Q.into()))
    );
}

/// The port's corners follow `ParamInput::corners`' rule: a new
/// segment's frame and its arrival are corners; the piece in flight sent
/// again, resumed (a source's re-send, a resync), is none; a step is one
/// corner; a ramp's frame and the frame it lands on are corners.
#[test]
fn corners_are_where_the_trajectory_changes_piece() {
    use crate::node::param_event::ParamSegment;
    let key = ParamKey::<Hz>::CUTOFF;
    let at = |i: usize| Offset::of(Samples::new(i), Samples::new(64));
    let seg = |from: f64, to: f64, len: usize| {
        ParamSegment::new(key, Hz::new(from), Hz::new(to), Samples::new(len))
    };
    let corners = |events: &[ParamEvent]| {
        let mut buf = [0u32; 64];
        let mut c = Corners::new(&mut buf);
        render_ramps(
            &mut Ramp::default(),
            events,
            UnitParam::Cutoff.into(),
            &mut [0.0; 64],
            |_| 0.0,
            &mut c,
        );
        let n = c.len();
        buf[..n].to_vec()
    };
    // A segment at 4 over 10 frames, in the ramp convention (its frame
    // one step in): its frame and the frame it lands on, 13.
    let a = seg(1.0, 2.0, 10);
    assert_eq!(corners(&[ParamEvent::segment(at(4), a)]), [4, 13]);
    // The same piece re-sent resumed at 9, five frames on: still 4, 13.
    let resent = a.resumed(a.position() + Samples::new(5), a.length());
    assert_eq!(
        corners(&[
            ParamEvent::segment(at(4), a),
            ParamEvent::segment(at(9), resent)
        ]),
        [4, 13]
    );
    // Re-sent after it arrived, at rest on its end: no corner either.
    let rested = a.resumed(a.length(), a.length());
    assert_eq!(
        corners(&[
            ParamEvent::segment(at(4), a),
            ParamEvent::segment(at(30), rested)
        ]),
        [4, 13]
    );
    // A different piece at 9 is a corner, and so is its own arrival.
    let b = seg(1.5, 3.0, 4);
    assert_eq!(
        corners(&[ParamEvent::segment(at(4), a), ParamEvent::segment(at(9), b)]),
        [4, 9, 12]
    );
    // A step is one corner on its frame.
    assert_eq!(
        corners(&[ParamEvent::segment(at(7), seg(0.0, 5.0, 0))]),
        [7]
    );
    // A ramp of 3 frames at 20: its frame and the frame it lands on, 22.
    let ramp = ParamRamp::new(key, Hz::new(8.0), Samples::new(3));
    assert_eq!(corners(&[ParamEvent::ramp(at(20), ramp)]), [20, 22]);
}

/// **A range's two bounds are one publish's.** While another thread
/// publishes `-k..k` as fast as it can, a reader that lends once per
/// block never reads a `min` that is not the negation of its `max` (a
/// torn pair would), and it ends on the last publish; the cell has
/// nothing retired once the reader is idle.
#[test]
fn a_range_is_read_whole_while_another_thread_publishes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const LAST: u32 = 20_000;
    let addr: UnitAddr = UnitParam::Cutoff.into();
    let ranges = ParamRanges::new(ParamPorts::of(&[addr]));
    let mut reader = ranges.readers().remove(0);
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            for k in 1..=LAST {
                let k = f64::from(k);
                assert!(ranges.store(addr, ParamRange::of(-k, k)));
            }
            done.store(true, Ordering::Release);
        });
        loop {
            let finished = done.load(Ordering::Acquire);
            let r = reader.load();
            assert_eq!(r.min(), -r.max(), "one publish's bounds: {r:?}");
            if finished {
                break;
            }
        }
    });
    let last = f64::from(LAST);
    assert_eq!(reader.load(), ParamRange::of(-last, last));
    assert_eq!(ranges.reclaim(), 0, "nothing is held once the reader idles");
}
