//! The plan's non-node ops: main inputs, delays, sums, event merges,
//! outputs and captures.

use super::node_op::Flags;
use super::state::Ring;
use crate::compile::ids::{AudioSlotId, EventSlotId};
use crate::compile::plan::{EMPTY_SLOT, GainPortOp, InputBound, ZERO_SLOT};
use crate::event::merge::{merge_params, merge_walk};
use crate::event::types::Event;
use crate::exec::arena::Role;
use crate::exec::rings::{AudioRing, EventFifo};
use crate::exec::slots::{AudioSlots, EventSlot, EventSlots, SlotFlags, borrow_events, event_pair};
use crate::model::gain::{self, SumLane, Weight};
use crate::node::MAX_PORTS;
use crate::node::param_event::ParamEvent;
use tutti_buffers::Float;
use tutti_units::Samples;

/// An audio delay's ring.
#[inline]
pub(crate) fn audio_ring<F: Float>(r: &mut Option<Ring<F>>) -> &mut AudioRing<F> {
    match r {
        Some(Ring::Audio(a)) => a,
        _ => unreachable!("an audio delay on an event ring"),
    }
}

/// An event delay's FIFO.
#[inline]
pub(crate) fn event_ring<F: Float>(r: &mut Option<Ring<F>>) -> &mut EventFifo {
    match r {
        Some(Ring::Event(f)) => f,
        _ => unreachable!("an event delay on an audio ring"),
    }
}

/// [`Op::MainInput`](crate::Op::MainInput).
#[inline]
pub(crate) fn main_input<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized>(
    arena: &mut A,
    flags: &mut Fl,
    inputs: &[&[F]],
    channel: u16,
    dst: AudioSlotId,
    frames: usize,
) {
    arena
        .slot_mut(dst, frames)
        .copy_from_slice(&inputs[channel as usize][..frames]);
    flags.set(dst, Flags::NONE);
}

/// [`Op::Delay`](crate::Op::Delay); `src == dst` is the in-place form.
#[inline]
pub(crate) fn delay_op<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized>(
    ring: &mut AudioRing<F>,
    arena: &mut A,
    flags: &mut Fl,
    src: AudioSlotId,
    dst: AudioSlotId,
    frames: usize,
) {
    let src_silent = flags.get(src).is_silent();
    let out_silent = if src == dst {
        ring.run_in_place(arena.slot_mut(dst, frames), src_silent)
    } else {
        let (s, d) = arena.pair(src, dst, frames);
        ring.run(s, d, src_silent)
    };
    flags.set(dst, Flags::new(out_silent, out_silent));
}

/// [`Op::Sum`](crate::Op::Sum) or [`Op::WeightedSum`](crate::Op::WeightedSum) of the slots `list` into `dst`: the left
/// fold `((+0.0 + w_0 * a_0) + w_1 * a_1) + ..` in the engine's sample format `F`, one rounding per
/// multiply and per add, never fused. `lanes` is empty for a plain sum, else one per
/// addend; an addend's weight is exactly `1.0` unless its lane says otherwise,
/// and a `1.0` weight is the plain add, no multiply (`crate::gain`).
///
/// An addend flagged silent is skipped whatever its weight: the
/// accumulator starts at `+0.0` and so is never `-0.0`, and `acc + w * (+0.0)`
/// is `acc` bit for bit (a finite `w`; every base is finite). A weight of
/// `0.0` is not skipped by value: `0.0 * NaN` is `NaN`, and the render must
/// not depend on whether a gain happens to be zero. Every lane's state
/// advances **every** block, skipped addend or not. When no addend is live
/// the output is zero-filled and flagged silent; otherwise it is constant when
/// every live addend is **and none is ramping** (a constant times a ramp is
/// not constant). The flags are a claim made from the addends' flags, never
/// from scanning samples. The loop is source-major (it vectorises); each
/// sample still sees the same sequence of adds as the frame-major fold.
///
/// `dst` is the first addend's own slot in the in-place form. That addend is
/// then not borrowed for reading: it is weighted in place (`dst = +0.0 + w *
/// dst`, which turns a `-0.0` into `+0.0`, as `+0.0 + w * a_0` does) and the
/// rest are added into it. `declick` is the plan's declick length
/// ([`Prepare::declick`](crate::Prepare)), which every lane's fade runs over.
#[inline]
pub(crate) fn sum_op<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    arena: &mut A,
    flags: &mut Fl,
    events: &E,
    list: &[AudioSlotId],
    lanes: &mut [SumLane<F>],
    gains: &[GainPortOp<EventSlotId>],
    dst: AudioSlotId,
    frames: usize,
    declick: Samples,
) {
    debug_assert!(!list.is_empty() && list.len() <= MAX_PORTS);
    debug_assert!(lanes.is_empty() || lanes.len() == list.len());
    // Moved on first, for every lane: a silent addend's weight moves too. A
    // driven lane runs the param port's step on its source's events here.
    let mut weights = [Weight::Unit; MAX_PORTS];
    for (k, (w, lane)) in weights.iter_mut().zip(lanes.iter_mut()).enumerate() {
        let source = gains
            .iter()
            .find(|g| usize::from(g.lane) == k)
            .map(|g| (g.from, &events.pev(g.slot)[..]));
        *w = SumLane::weight(lane, frames, declick, source);
    }
    // Requests, then sorted by slot; the order of addition is the `Read`
    // index, among the live addends.
    let mut reqs = [(ZERO_SLOT, Role::Read(0)); MAX_PORTS + 1];
    let mut live_weights = [Weight::Unit; MAX_PORTS];
    let (mut live, mut m) = (0usize, 0usize);
    let (mut in_place, mut constant) = (false, true);
    for (k, &s) in list.iter().enumerate() {
        let f = flags.get(s);
        if f.is_silent() {
            continue;
        }
        constant &= f.is_constant()
            && !matches!(
                weights[k],
                Weight::Ramp { .. } | Weight::Frames(_) | Weight::Fade { .. }
            );
        if live == 0 && s == dst {
            in_place = true;
        } else {
            reqs[m] = (s, Role::Read(live as u8));
            m += 1;
        }
        live_weights[live] = weights[k];
        live += 1;
    }
    if live == 0 {
        arena.slot_mut(dst, frames).fill(F::ZERO);
        flags.set(dst, Flags::SILENCE);
        return;
    }
    reqs[m] = (dst, Role::Write(0));
    m += 1;
    reqs[..m].sort_unstable();
    let mut ins: [&[F]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut outs: [&mut [F]; 1] = [&mut [][..]];
    arena.borrow(frames, &reqs[..m], &mut ins, &mut outs);
    let [out] = outs;
    // `ins[k]` is live addend `k`, but for the first when it is `dst` itself.
    gain::first(out, (!in_place).then(|| ins[0]), live_weights[0]);
    for (a, &w) in ins[1..live].iter().zip(&live_weights[1..live]) {
        gain::add(out, a, w);
    }
    flags.set(dst, Flags::new(false, constant));
}

/// [`Op::EventDelay`](crate::Op::EventDelay). Returns the events its FIFO refused.
#[inline]
pub(crate) fn event_delay_op<E: EventSlots + ?Sized>(
    fifo: &mut EventFifo,
    events: &mut E,
    src: EventSlotId,
    dst: EventSlotId,
    frames: usize,
) -> u64 {
    let (input, output) = event_pair(events, src, dst);
    output.clear();
    u64::from(fifo.run(input, output, frames))
}

/// [`Op::EventMerge`](crate::Op::EventMerge) of the slots `list` into `dst`. Returns the events it
/// dropped: none unless `bound` (the node's `Shape::event_input_capacity`) is
/// below what the sources hold, since the slot holds all its inputs
/// (`event_slot_capacity`) otherwise. The count is the caller's to charge
/// (`Dropped::charge_merge`).
pub(crate) fn event_merge_op<E: EventSlots + ?Sized>(
    list: &[EventSlotId],
    dst: EventSlotId,
    bound: Option<InputBound>,
    events: &mut E,
) -> u64 {
    let mut reqs = [(EMPTY_SLOT, Role::Read(0)); MAX_PORTS + 1];
    for (i, &s) in list.iter().enumerate() {
        reqs[i] = (s, Role::Read(i as u8));
    }
    reqs[list.len()] = (dst, Role::Write(0));
    let mut ins: [&[Event]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut pins: [&[ParamEvent]; MAX_PORTS] = [&[]; MAX_PORTS];
    let mut output: Option<&mut EventSlot> = None;
    borrow_events(
        events,
        &mut reqs[..=list.len()],
        |port, v| {
            ins[port as usize] = &v.events;
            pins[port as usize] = &v.params;
        },
        |_, v| output = Some(v),
    );
    let EventSlot { events, params } = output.expect("dst borrowed");
    events.clear();
    params.clear();
    let room = bound.map_or(usize::MAX, |b| b.capacity.get() as usize);
    let room = events.capacity().min(room);
    // The param events are never bounded by a node's input capacity: the slot
    // holds every one its sources can write.
    let lost = merge_params(&pins[..list.len()], params);
    let cut = merge_walk(&ins[..list.len()], room, |_, e| {
        events.push(e);
    });
    u64::from(lost) + u64::from(cut)
}

/// [`Op::Output`](crate::Op::Output): slot `src` into `out`, through the channel's alignment
/// ring if it has one.
#[inline]
pub(crate) fn output_op<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized>(
    ring: Option<&mut AudioRing<F>>,
    out: &mut [F],
    arena: &A,
    flags: &Fl,
    src: AudioSlotId,
) {
    let s = arena.slot(src, out.len());
    match ring {
        Some(ring) => {
            ring.run(s, out, flags.get(src).is_silent());
        }
        None => out.copy_from_slice(s),
    }
}

/// [`Op::Capture`](crate::Op::Capture).
#[inline]
pub(crate) fn capture_op<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized>(
    ring: &mut AudioRing<F>,
    arena: &A,
    flags: &Fl,
    src: AudioSlotId,
    frames: usize,
) {
    ring.push(arena.slot(src, frames), flags.get(src).is_silent());
}
