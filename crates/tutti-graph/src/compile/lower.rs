//! Compiler lowering: the ops before colouring, with slots in place of values,
//! as the plan's public ops, lists and value tables.

use crate::EdgeId;
use crate::compile::colour::Coloured;
use crate::compile::emit::{ARef, Emitted, Pre, Val};
use crate::compile::ids::{AudioSlotId, DelayId, EventSlotId, Id, OpId, Table, TableSlice};
use crate::compile::plan::{GainPortOp, Op, ParamPortOp, Span, Value};
use crate::node::InPlaceMask;

/// The plan's ops and the flat lists and tables they index.
///
/// The public `Op`, `Value` and port structs name slots, units, delays and
/// ops by raw `u32` until the plan is narrowed; each id is `Id::get` here.
/// Every [`Span`] an op holds names a run of the list it indexes.
pub(super) struct Lowered {
    pub(super) ops: Table<OpId, Op>,
    pub(super) audio_list: Vec<AudioSlotId>,
    pub(super) event_list: Vec<EventSlotId>,
    pub(super) param_ports: Vec<ParamPortOp>,
    pub(super) sum_lanes: Vec<Option<EdgeId>>,
    pub(super) gain_ports: Vec<GainPortOp>,
    pub(super) audio_values: Table<crate::compile::ids::AudioValueId, Value>,
    pub(super) event_values: Table<crate::compile::ids::EventValueId, Value>,
    pub(super) value_readers: Vec<OpId>,
}

/// Lowers every op of `emitted` to the slots of `coloured`.
///
/// Takes the [`Emitted`] ops and their [`Coloured`] slots; returns the
/// [`Lowered`] ops in emission order, each with its input and output slots,
/// and one [`Value`] per audio and event value naming its slot, writer and
/// readers.
pub(super) fn lower(emitted: &Emitted, coloured: &Coloured) -> Lowered {
    let aslot = |v| coloured.aslot(v);
    let eslot = |v| coloured.eslot(v);
    let aref_slot = |r| coloured.aref_slot(r);
    let eref_slot = |r| coloured.eref_slot(r);
    let mut audio_list: Vec<AudioSlotId> = Vec::new();
    let mut event_list: Vec<EventSlotId> = Vec::new();
    let mut param_ports: Vec<ParamPortOp> = Vec::new();
    let mut sum_lanes: Vec<Option<EdgeId>> = Vec::new();
    let mut gain_ports: Vec<GainPortOp> = Vec::new();
    let ops: Table<OpId, Op> = emitted
        .pre
        .iter()
        .map(|op| match op {
            Pre::MainInput { channel, dst } => Op::MainInput {
                channel: *channel,
                dst: aslot(*dst).get(),
            },
            Pre::Delay { delay, src, dst } => Op::Delay {
                delay: delay.get(),
                src: aslot(*src).get(),
                dst: aslot(*dst).get(),
            },
            Pre::Sum { srcs, dst } => Op::Sum {
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
                dst: aslot(*dst).get(),
            },
            Pre::WeightedSum {
                srcs,
                lanes,
                gains,
                dst,
            } => {
                let lanes = Span::extend(&mut sum_lanes, lanes.iter().copied());
                let gains = Span::extend(
                    &mut gain_ports,
                    gains.iter().map(|g| GainPortOp {
                        lane: g.lane,
                        from: g.from,
                        slot: eslot(g.value).get(),
                    }),
                );
                Op::WeightedSum {
                    srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
                    dst: aslot(*dst).get(),
                    lanes,
                    gains,
                }
            }
            Pre::EventDelay { delay, src, dst } => Op::EventDelay {
                delay: delay.get(),
                src: eslot(*src).get(),
                dst: eslot(*dst).get(),
            },
            Pre::EventMerge { srcs, dst, bound } => Op::EventMerge {
                srcs: Span::extend(&mut event_list, srcs.iter().map(|&r| eref_slot(r))),
                dst: eslot(*dst).get(),
                bound: *bound,
            },
            Pre::Node {
                unit,
                ain,
                aout,
                ein,
                eout,
                params,
            } => {
                let params = Span::extend(
                    &mut param_ports,
                    params.iter().map(|p| ParamPortOp {
                        port: p.port,
                        param: p.param,
                        from: p.from,
                        slot: eslot(p.value).get(),
                    }),
                );
                let mask = ain
                    .iter()
                    .zip(aout)
                    .enumerate()
                    .filter(|&(_, (&r, &o))| matches!(r, ARef::Val(u) if aslot(u) == aslot(o)))
                    .fold(InPlaceMask::NONE, |mask, (c, _)| mask.with(c));
                Op::Node {
                    unit: unit.get(),
                    audio_in: Span::extend(&mut audio_list, ain.iter().map(|&r| aref_slot(r))),
                    audio_out: Span::extend(&mut audio_list, aout.iter().map(|&v| aslot(v))),
                    event_in: Span::extend(&mut event_list, ein.iter().map(|&r| eref_slot(r))),
                    event_out: Span::extend(&mut event_list, eout.iter().map(|&v| eslot(v))),
                    in_place: mask,
                    params,
                }
            }
            Pre::Output {
                channel,
                src,
                delay,
            } => Op::Output {
                channel: *channel,
                src: aref_slot(*src).get(),
                delay: delay.map(DelayId::get),
            },
            Pre::Tap { tap, srcs } => Op::Tap {
                tap: tap.get(),
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
            },
            Pre::EventTap { tap, src } => Op::EventTap {
                tap: tap.get(),
                src: eref_slot(*src).get(),
            },
            Pre::Meter { meter, srcs } => Op::Meter {
                meter: meter.get(),
                srcs: Span::extend(&mut audio_list, srcs.iter().map(|&r| aref_slot(r))),
            },
            Pre::Capture { feedback, src } => Op::Capture {
                feedback: feedback.get(),
                src: aslot(*src).get(),
            },
            Pre::EventCapture { feedback, src } => Op::EventCapture {
                feedback: feedback.get(),
                src: eslot(*src).get(),
            },
        })
        .collect();

    let mut value_readers: Vec<OpId> = Vec::new();
    let audio_values = lower_values(&emitted.audio, &mut value_readers, |v| aslot(v).get());
    let event_values = lower_values(&emitted.event, &mut value_readers, |v| eslot(v).get());
    Lowered {
        ops,
        audio_list,
        event_list,
        param_ports,
        sum_lanes,
        gain_ports,
        audio_values,
        event_values,
        value_readers,
    }
}

/// The public [`Value`] table for `vals`, each reader list appended to
/// `readers`.
fn lower_values<V: Id>(
    vals: &TableSlice<V, Val>,
    readers: &mut Vec<OpId>,
    slot: impl Fn(V) -> u32,
) -> Table<V, Value> {
    vals.iter_enumerated()
        .map(|(i, v)| Value {
            slot: slot(i),
            writer: v.writer.get(),
            readers: Span::extend(readers, v.readers.iter().copied()),
        })
        .collect()
}
