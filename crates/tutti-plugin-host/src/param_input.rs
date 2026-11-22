//! A plugin node's event input, read for its parameters: the parameter
//! events on it (a [`ParamRamp`](tutti_graph::ParamRamp) or a
//! [`ParamSegment`], in the plugin's address model)
//! become points in the node's [`ParamDelivery`], through its
//! [`SegmentPoints`], and every other event is handed back on its frame.
//!
//! One walk for both plugin nodes: a chunked Processor (the out-of-process one) runs it for each
//! piece of a chunk (`node`'s graph module), the other in-process
//! Processors for each block (`node`), so the two read a
//! ramp, a segment and an address the same way.

use tutti_graph::{Cx, Event, EventKind, Offset, ParamEvent, ParamKind, ParamSegment};
use tutti_units::{ParamAddr, Samples};

use tutti_plugin_types::ParameterQueue;
use tutti_units::ParamId;

use super::param_delivery::{ParamDelivery, chunk_offset};
use super::segment_points::SegmentPoints;

/// The events of `events` and the param events of `params_in` (each sorted by
/// frame) on block frames `from..from + n`, placed on chunk frames from `at`
/// on. On one frame a param event comes before an event: a note sounds with
/// the value its frame sets.
///
/// - A ramp moves from where its parameter stood at the end of the frame
///   before its own (the segment in flight there, else the value the node
///   last gave it; an event earlier on the ramp's frame does not count, as
///   in a param port) to its target, along its shape over its duration: it
///   is sent as the [`ParamSegment`] that makes, so its points are a
///   segment's, its first a corner. A ramp with no duration, or on a parameter the node
///   has given no value yet, is one point, its target on its frame. Either
///   way it stops a segment in flight on its parameter.
/// - A segment is its value on its frame, then its value on the play-grid
///   frames, the chunk edges and its arrival ([`SegmentPoints`]), which
///   this walk emits on the frames between events and up to the end of the
///   piece. A point is a grid point when the play clock at its frame
///   (`cx.play_at`) is a multiple of 32: past a queue's room grid points go
///   first.
/// - Any other event (MIDI, harmony) goes to `other` with its chunk frame.
///
/// The classification is by frame alone: a lane's corner (a breakpoint, a
/// wrap) that lands on a grid frame is a grid point, and is dropped first
/// like one. That loses nothing a grid point would not: the lane's grid
/// piece starting on that frame carries the same value. A ramp's first
/// point is always a corner.
///
/// `cx` is the node's call: its `Env::frame` is the absolute frame of block
/// frame 0 (where a segment in flight is along itself), and its play clock
/// classifies a segment's first point. A parameter event with no value for this
/// plugin is skipped. Allocation-free.
#[allow(
    clippy::too_many_arguments,
    reason = "the two plugin nodes keep the delivery and the segments in \
              different places; a struct over them would exist only for this call"
)]
pub fn take(
    events: &[Event],
    params_in: &[ParamEvent],
    from: usize,
    n: usize,
    at: usize,
    cx: &Cx<'_>,
    segments: &mut SegmentPoints,
    params: &mut ParamDelivery,
    mut other: impl FnMut(&Event, usize),
) {
    let (mut ei, mut pi) = (
        events.partition_point(|e| e.offset.index() < from),
        params_in.partition_point(|e| e.offset.index() < from),
    );
    params.begin_walk();
    // This call's frame `i` as the node's absolute frame.
    let base = cx.env.frame.get();
    let abs = |i: usize| base + i as u64;
    // Whether absolute frame `f` (in this call) is a play-grid frame: its
    // play clock count a multiple of 32.
    let grid = |f: u64| {
        usize::try_from(f - base)
            .ok()
            .and_then(|i| Offset::new(Samples::new(i), cx.env.block_len))
            .is_some_and(|off| {
                cx.play_at(off)
                    .play
                    .get()
                    .is_multiple_of(ParameterQueue::POINT_SPACING as u64)
            })
    };
    // The frames whose segment points are sent: up to each event's, then
    // the rest of the piece.
    let mut done = from;
    while let Some(next) = pick(events.get(ei), params_in.get(pi)) {
        let o = match next {
            Next::Event(e) => {
                ei += 1;
                e.offset.index()
            }
            Next::Param(p) => {
                pi += 1;
                p.offset.index()
            }
        };
        if o >= from + n {
            break;
        }
        let frame = at + o - from;
        segments.points(params, abs(done), abs(o), at + done - from, grid);
        done = o;
        match next {
            Next::Param(ParamEvent {
                kind: ParamKind::Ramp(ramp),
                ..
            }) => {
                let ParamAddr::Id(id) = ramp.addr() else {
                    continue;
                };
                let Some(value) = ramp.target_by_id(id) else {
                    continue;
                };
                // Where the parameter stood at the end of the frame before
                // the ramp's, whatever came earlier on its own frame: a
                // param port's ramp starts from there too once its source
                // has started (its first starts from the base at its own
                // frame).
                let stands = stood(segments, params, id, o, abs(o));
                match stands {
                    Some(stands) if ramp.duration() > Samples::new(0) => {
                        let seg = ParamSegment::by_id(id, stands, value, ramp.duration())
                            .law(ramp.shape());
                        segments.start(params, id, seg, abs(o), frame, false);
                    }
                    _ => {
                        // A step: it stops a segment in flight on its
                        // parameter.
                        segments.interrupt(id);
                        params.point(id, chunk_offset(frame), value);
                    }
                }
            }
            Next::Param(ParamEvent {
                kind: ParamKind::Segment(seg),
                ..
            }) => {
                // A plugin parameter takes point values: the segment's value
                // on its frame, then points along it until it lands.
                let ParamAddr::Id(id) = seg.addr() else {
                    continue;
                };
                // Noted before the segment moves it, for a ramp later on
                // this frame.
                stood(segments, params, id, o, abs(o));
                segments.start(params, id, *seg, abs(o), frame, grid(abs(o)));
            }
            Next::Event(e) => match e.kind {
                EventKind::Midi(_) | EventKind::Harmony(_) => other(e, frame),
                // A plugin is no source a lane reads: nothing to re-send.
                EventKind::Resync => {}
            },
        }
    }
    segments.points(params, abs(done), abs(from + n), at + done - from, grid);
}

/// The next of an event list and a param event list, by frame.
#[derive(Clone, Copy)]
enum Next<'a> {
    Event(&'a Event),
    Param(&'a ParamEvent),
}

/// The earlier of the next event and the next param event; on one frame the
/// param event, so a note sounds with the value its frame sets.
fn pick<'a>(event: Option<&'a Event>, param: Option<&'a ParamEvent>) -> Option<Next<'a>> {
    match (event, param) {
        (None, None) => None,
        (Some(e), None) => Some(Next::Event(e)),
        (None, Some(p)) => Some(Next::Param(p)),
        (Some(e), Some(p)) if e.offset < p.offset => Some(Next::Event(e)),
        (Some(_), Some(p)) => Some(Next::Param(p)),
    }
}

/// Where `address` stood at the end of the frame before block frame `o`
/// (absolute frame `abs`): the segment in flight there, else the value the
/// node last gave it. Read at the parameter's first event on the frame and
/// kept for the frame ([`ParamDelivery::frame_start`]), so a step or a
/// segment earlier on the frame does not move it.
fn stood(
    segments: &SegmentPoints,
    params: &mut ParamDelivery,
    address: ParamId,
    o: usize,
    abs: u64,
) -> Option<f64> {
    params.frame_start(address, o, |params| {
        abs.checked_sub(1)
            .and_then(|f| segments.value_at(address, f))
            .or_else(|| params.current(address))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeliveryCounters;
    use std::sync::Arc;
    use tutti_graph::{
        Env, Exponent, Law, ParamRamp, PlayClock, PlayReading, TransportChanges, TransportHistory,
        TransportPosition,
    };
    use tutti_units::ParamId;
    use tutti_units::{Beat, Bpm, Frame, Latency, SampleRate, Samples};

    /// The block's `Env` at absolute frame 0, its play clock starting at
    /// `play`.
    fn env(len: usize, play: u64) -> Env {
        Env {
            frame: Frame::new(0),
            sample_rate: SampleRate::SR_48K,
            block_len: Samples::new(len),
            transport: TransportPosition::new(false, Bpm::of(120.0), Beat::new(0.0), None),
            changes: TransportChanges::NONE,
            play: PlayClock {
                start: PlayReading {
                    epoch: 0,
                    play: Frame::new(play),
                },
                next_epoch: 0,
            },
        }
    }

    /// **A ramp reaches a plugin along its shape.** In one 64-frame block
    /// whose play clock starts at 1 (frame 31 is the one grid frame): a
    /// ramp over 8 frames to 0 on a parameter the node has given nothing is
    /// a step, its target on its frame; a `PowerIn(2)` ramp to 1 over 40
    /// frames at frame 11 moves from that 0, its points on its frame
    /// (`(1/40)²`) and on the grid frame (`(21/40)²`); a linear ramp at
    /// frame 40 interrupts it from where it stood on frame 39 (`(29/40)²`),
    /// a quarter of the way to 0 on its frame, 0 on its last. The shaped
    /// ramp's own arrival (frame 50) is never sent.
    #[test]
    fn a_ramp_reaches_a_plugin_along_its_shape() {
        const TOL: f64 = 1e-12;
        let address = ParamId::new(7);
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([address], Arc::clone(&counters));
        params.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let len = 64;
        let env = env(len, 1);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        };
        let at = |i: usize, ramp: ParamRamp| {
            ParamEvent::ramp(Offset::of(Samples::new(i), Samples::new(len)), ramp)
        };
        let events = [
            at(0, ParamRamp::by_id(ParamId::new(7), 0.0, Samples::new(8))),
            at(
                11,
                ParamRamp::by_id(ParamId::new(7), 1.0, Samples::new(40))
                    .with_shape(Law::PowerIn(Exponent::of(2.0))),
            ),
            at(40, ParamRamp::by_id(ParamId::new(7), 0.0, Samples::new(4))),
        ];
        take(
            &[],
            &events,
            0,
            len,
            0,
            &cx,
            &mut segments,
            &mut params,
            |_, _| {},
        );
        let points: Vec<(u32, f64)> = params.changes().queues[0]
            .points
            .iter()
            .map(|p| (p.sample_offset.get(), p.value.get()))
            .collect();
        let stood = (29.0f64 / 40.0).powi(2);
        let want = [
            (0, 0.0),
            (11, (1.0f64 / 40.0).powi(2)),
            (31, (21.0f64 / 40.0).powi(2)),
            (40, stood * 0.75),
            (43, 0.0),
        ];
        assert_eq!(points.len(), want.len(), "{points:?}");
        for ((o, v), (wo, wv)) in points.iter().zip(want) {
            assert_eq!(*o, wo, "{points:?}");
            assert!((v - wv).abs() <= TOL, "frame {o}: {v} vs {wv}");
        }
    }

    /// **A ramp starts from where its parameter stood before its frame,**
    /// whatever came earlier on that frame, as a param port's does. A step
    /// to 0.2 at frame 0; at frame 10 a step to 0.4, then a ramp to 1 over
    /// 4 frames: the ramp starts from 0.2, so frame 10 reads 0.4 (sent once,
    /// changes only) and 13 reads 1. At frame 20 a segment, then a ramp to 0
    /// over 2 frames: the ramp starts from the 1 held before the frame, half
    /// way on frame 20.
    #[test]
    fn a_ramp_starts_from_the_value_before_its_frame() {
        const TOL: f64 = 1e-12;
        let address = ParamId::new(7);
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([address], Arc::clone(&counters));
        params.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let len = 64;
        let env = env(len, 1);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        };
        let off = |i: usize| Offset::of(Samples::new(i), Samples::new(len));
        let events = [
            ParamEvent::ramp(
                off(0),
                ParamRamp::by_id(ParamId::new(7), 0.2, Samples::new(0)),
            ),
            ParamEvent::ramp(
                off(10),
                ParamRamp::by_id(ParamId::new(7), 0.4, Samples::new(0)),
            ),
            ParamEvent::ramp(
                off(10),
                ParamRamp::by_id(ParamId::new(7), 1.0, Samples::new(4)),
            ),
            ParamEvent::segment(
                off(20),
                ParamSegment::by_id(ParamId::new(7), 0.9, 0.9, Samples::new(2)),
            ),
            ParamEvent::ramp(
                off(20),
                ParamRamp::by_id(ParamId::new(7), 0.0, Samples::new(2)),
            ),
        ];
        take(
            &[],
            &events,
            0,
            len,
            0,
            &cx,
            &mut segments,
            &mut params,
            |_, _| {},
        );
        let points: Vec<(u32, f64)> = params.changes().queues[0]
            .points
            .iter()
            .map(|p| (p.sample_offset.get(), p.value.get()))
            .collect();
        let want = [
            (0, 0.2),
            (10, 0.4),
            (13, 1.0),
            (20, 0.9),
            (20, 0.5),
            (21, 0.0),
        ];
        assert_eq!(points.len(), want.len(), "{points:?}");
        for ((o, v), (wo, wv)) in points.iter().zip(want) {
            assert_eq!(*o, wo, "{points:?}");
            assert!((v - wv).abs() <= TOL, "frame {o}: {v} vs {wv}: {points:?}");
        }
    }

    /// The queue of `address` in the chunk being filled, as `(frame, value)`.
    fn queue(params: &mut ParamDelivery, address: ParamId) -> Vec<(u32, f64)> {
        params
            .changes()
            .queues
            .iter()
            .find(|q| q.param_id == address)
            .map(|q| {
                q.points
                    .iter()
                    .map(|p| (p.sample_offset.get(), p.value.get()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `take` over a whole `len`-frame block of `events`, its play clock
    /// starting at 1.
    fn walk(
        events: &[ParamEvent],
        len: usize,
        segments: &mut SegmentPoints,
        params: &mut ParamDelivery,
    ) {
        let env = env(len, 1);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        };
        take(&[], events, 0, len, 0, &cx, segments, params, |_, _| {});
    }

    /// **A parameter the plugin's list did not name keeps its start too.**
    /// Opaque id 9 on a delivery that lists nothing, never given a value: a
    /// step to 0.4 and then a ramp to 1 over 4 frames, both on frame 10.
    /// The step learns a spare slot; the ramp still finds no start before
    /// its frame, so it is a jump to 1, as on a listed parameter.
    #[test]
    fn an_unlisted_parameter_keeps_its_start_on_the_frame() {
        let address = ParamId::new(9);
        let mut params =
            ParamDelivery::new(std::iter::empty(), Arc::new(DeliveryCounters::default()));
        params.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let off = |i: usize| Offset::of(Samples::new(i), Samples::new(64));
        let events = [
            ParamEvent::ramp(
                off(10),
                ParamRamp::by_id(ParamId::new(9), 0.4, Samples::new(0)),
            ),
            ParamEvent::ramp(
                off(10),
                ParamRamp::by_id(ParamId::new(9), 1.0, Samples::new(4)),
            ),
        ];
        walk(&events, 64, &mut segments, &mut params);
        assert_eq!(queue(&mut params, address), vec![(10, 0.4), (10, 1.0)]);
    }

    /// **A frame's start is the walk's own.** A step to 0.2 on frame 10 of
    /// one block, whose chunk does not go (the value is owed); a ramp to 1
    /// over 4 frames on frame 10 of the next block starts from that 0.2:
    /// the owed value on frame 0, 0.4 on frame 10, 1 on frame 13.
    #[test]
    fn a_frame_start_is_not_kept_from_an_earlier_block() {
        let address = ParamId::new(7);
        let mut params = ParamDelivery::new([address], Arc::new(DeliveryCounters::default()));
        params.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let off = |i: usize| Offset::of(Samples::new(i), Samples::new(64));
        let step = [ParamEvent::ramp(
            off(10),
            ParamRamp::by_id(ParamId::new(7), 0.2, Samples::new(0)),
        )];
        walk(&step, 64, &mut segments, &mut params);
        params.handed_off(false);
        params.begin();
        segments.begin(64);
        let ramp = [ParamEvent::ramp(
            off(10),
            ParamRamp::by_id(ParamId::new(7), 1.0, Samples::new(4)),
        )];
        walk(&ramp, 64, &mut segments, &mut params);
        let got = queue(&mut params, address);
        let want = [(0, 0.2), (10, 0.4), (13, 1.0)];
        assert_eq!(got.len(), want.len(), "{got:?}");
        for ((o, v), (wo, wv)) in got.iter().zip(want) {
            assert_eq!(*o, wo, "{got:?}");
            assert!((v - wv).abs() <= 1e-12, "frame {o}: {v} vs {wv}: {got:?}");
        }
    }

    /// **The play clock decides which points are grid points.** A block
    /// whose play clock starts at 10 takes a segment on each of frames
    /// 0..300 (two frames long, holding alternating values) into one
    /// parameter whose queue holds 259 points (a 64-frame chunk's room).
    /// Nine of the 300 starts fall on a frame whose play count is a multiple
    /// of 32 (22, 54, .. 278): past the room those nine go first, counted as
    /// grid drops, and only then are corners merged: 300 - 259 - 9, less
    /// the one start that follows the refused grid point at 278 and so
    /// repeats the queue's last value (skipped, changes only). The last
    /// segment's arrival repeats its value too. None is left on a grid
    /// frame.
    #[test]
    fn a_full_queue_drops_the_play_grid_first() {
        let address = ParamId::new(7);
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([address], Arc::clone(&counters));
        let room = params.prepare(64);
        assert_eq!(room, 259);
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        params.begin();
        segments.begin(64);
        let len = 512;
        let env = env(len, 10);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        };
        let events: Vec<ParamEvent> = (0..300)
            .map(|i| {
                let v = if i % 2 == 0 { 0.25 } else { 0.75 };
                let seg = ParamSegment::by_id(ParamId::new(7), v, v, Samples::new(2));
                ParamEvent::segment(Offset::of(Samples::new(i), Samples::new(len)), seg)
            })
            .collect();
        take(
            &[],
            &events,
            0,
            len,
            0,
            &cx,
            &mut segments,
            &mut params,
            |_, _| {},
        );
        let stats = counters.snapshot();
        assert_eq!(stats.grid_points_dropped, 9, "the nine grid frames");
        assert_eq!(stats.points_merged, 300 - 259 - 9 - 1);
        let offsets: Vec<u32> = params.changes().queues[0]
            .points
            .iter()
            .map(|p| p.sample_offset.get())
            .collect();
        assert_eq!(offsets.len(), room);
        assert!(
            offsets.iter().all(|&o| (10 + o) % 32 != 0),
            "no point left on a grid frame: {offsets:?}"
        );
    }

    /// **On one frame a param event comes before an event.** A note at 5 and
    /// at 10, a param step at 5 and at 10: the walk hands the events to `other`
    /// on their frames, and each frame's param event is taken ahead of its
    /// note, so the point is placed first.
    #[test]
    fn a_param_event_is_taken_before_an_event_on_its_frame() {
        use tutti_ump::Ump;
        let off = |i: usize| Offset::of(Samples::new(i), Samples::new(64));
        let note = |i: usize| Event::midi(off(i), Ump([0x2090_3c64, 0, 0, 0]));
        let step = |i: usize, v: f64| {
            ParamEvent::ramp(
                off(i),
                ParamRamp::by_id(ParamId::new(7), v, Samples::new(0)),
            )
        };
        let events = [note(5), note(10)];
        let params = [step(5, 0.25), step(10, 0.75)];
        let order: Vec<char> = {
            let (mut ei, mut pi) = (0, 0);
            let mut out = Vec::new();
            while let Some(n) = pick(events.get(ei), params.get(pi)) {
                match n {
                    Next::Event(_) => {
                        ei += 1;
                        out.push('e');
                    }
                    Next::Param(_) => {
                        pi += 1;
                        out.push('p');
                    }
                }
            }
            out
        };
        assert_eq!(order, ['p', 'e', 'p', 'e']);

        let address = ParamId::new(7);
        let mut delivery = ParamDelivery::new([address], Arc::new(DeliveryCounters::default()));
        delivery.prepare(64);
        let mut segments = SegmentPoints::default();
        segments.prepare(delivery.room());
        delivery.begin();
        segments.begin(64);
        let env = env(64, 1);
        let cx = Cx {
            env: &env,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        };
        let mut seen = Vec::new();
        take(
            &events,
            &params,
            0,
            64,
            0,
            &cx,
            &mut segments,
            &mut delivery,
            |_, frame| seen.push(frame),
        );
        assert_eq!(seen, [5, 10], "`other` sees the notes on their frames");
        let frames: Vec<u32> = delivery.changes().queues[0]
            .points
            .iter()
            .map(|p| p.sample_offset.get())
            .collect();
        assert_eq!(frames, [5, 10], "the param points land on frames 5 and 10");
    }
}
