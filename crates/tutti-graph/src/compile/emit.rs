//! The ops before colouring: values, the op types that name them, and the
//! helpers that emit delays, sums and merges.

use std::collections::BTreeSet;

use crate::EdgeId;
use crate::compile::ids::{
    AudioFeedbackId, AudioValueId, DelayId, EventFeedbackId, EventValueId, Id, OpId,
    PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, Table, TableSlice,
};
use crate::compile::plan::{DelayKey, DelaySpec, FeedbackKey, InputBound};
use crate::model::spec::EventOutputPort;
use crate::node::MAX_PORTS;
use tutti_units::Samples;

/// Where an audio input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ARef {
    Val(AudioValueId),
    Zero,
    Fb(AudioFeedbackId),
}

/// Where an event input reads from, before colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ERef {
    Val(EventValueId),
    Empty,
    Fb(EventFeedbackId),
}

/// An op before colouring: the same shape as [`Op`](crate::Op) with value ids
/// where slots will go.
pub(super) enum Pre {
    MainInput {
        channel: u16,
        dst: AudioValueId,
    },
    Delay {
        delay: DelayId,
        src: AudioValueId,
        dst: AudioValueId,
    },
    Sum {
        srcs: Vec<ARef>,
        dst: AudioValueId,
    },
    /// A `Sum` with a weighted lane: `lanes` is one entry per addend.
    WeightedSum {
        srcs: Vec<ARef>,
        lanes: Vec<Option<EdgeId>>,
        /// The driven lanes among them.
        gains: Vec<GPort>,
        dst: AudioValueId,
    },
    EventDelay {
        delay: DelayId,
        src: EventValueId,
        dst: EventValueId,
    },
    EventMerge {
        srcs: Vec<ERef>,
        dst: EventValueId,
        bound: Option<InputBound>,
    },
    Node {
        unit: PlanUnitId,
        ain: Vec<ARef>,
        aout: Vec<AudioValueId>,
        ein: Vec<ERef>,
        eout: Vec<EventValueId>,
        params: Vec<PPort>,
    },
    Output {
        channel: u16,
        src: ARef,
        delay: Option<DelayId>,
    },
    Tap {
        tap: PlanTapId,
        srcs: Vec<ARef>,
    },
    EventTap {
        tap: PlanEventTapId,
        src: ERef,
    },
    Meter {
        meter: PlanMeterId,
        srcs: Vec<ARef>,
    },
    Capture {
        feedback: AudioFeedbackId,
        src: AudioValueId,
    },
    EventCapture {
        feedback: EventFeedbackId,
        src: EventValueId,
    },
}

/// A driven param port before colouring: its event source, and the event
/// value it reads.
pub(super) struct PPort {
    pub(super) port: u16,
    pub(super) param: tutti_units::UnitAddr,
    pub(super) from: EventOutputPort,
    pub(super) value: EventValueId,
}

/// A driven lane of a weighted sum before colouring: which addend it is, its
/// event source, and the event value it reads (the sibling of [`PPort`]).
#[derive(Clone, Copy)]
pub(super) struct GPort {
    pub(super) lane: u16,
    pub(super) from: EventOutputPort,
    pub(super) value: EventValueId,
}

/// A value under construction.
pub(crate) struct Val {
    pub(crate) writer: OpId,
    pub(crate) readers: Vec<OpId>,
}

/// What the op-emission pass produces for the colouring pass.
pub(super) struct Emitted {
    pub(super) pre: Table<OpId, Pre>,
    pub(super) audio: Table<AudioValueId, Val>,
    pub(super) event: Table<EventValueId, Val>,
    pub(super) preds: Table<OpId, BTreeSet<OpId>>,
    pub(super) delays: Table<DelayId, DelaySpec>,
}

impl Emitted {
    /// The id the next op pushed gets.
    pub(super) fn next_op(&self) -> OpId {
        OpId::from_index(self.pre.len())
    }

    pub(super) fn push(&mut self, op: Pre) -> OpId {
        self.preds.push(BTreeSet::new());
        self.pre.push(op)
    }

    pub(super) fn audio_value(&mut self, writer: OpId) -> AudioValueId {
        self.audio.push(Val {
            writer,
            readers: Vec::new(),
        })
    }

    pub(super) fn event_value(&mut self, writer: OpId) -> EventValueId {
        self.event.push(Val {
            writer,
            readers: Vec::new(),
        })
    }

    /// Pushes the op that writes a new audio value, built from that value.
    pub(super) fn push_audio(
        &mut self,
        op: impl FnOnce(AudioValueId) -> Pre,
    ) -> (OpId, AudioValueId) {
        let value = self.audio_value(self.next_op());
        (self.push(op(value)), value)
    }

    /// Pushes the op that writes a new event value, built from that value.
    pub(super) fn push_event(
        &mut self,
        op: impl FnOnce(EventValueId) -> Pre,
    ) -> (OpId, EventValueId) {
        let value = self.event_value(self.next_op());
        (self.push(op(value)), value)
    }

    pub(super) fn read_audio(&mut self, v: AudioValueId, op: OpId) {
        let w = self.audio[v].writer;
        self.audio[v].readers.push(op);
        self.preds[op].insert(w);
    }

    pub(super) fn read_event(&mut self, v: EventValueId, op: OpId) {
        let w = self.event[v].writer;
        self.event[v].readers.push(op);
        self.preds[op].insert(w);
    }

    pub(super) fn read_aref(&mut self, r: ARef, op: OpId) {
        if let ARef::Val(v) = r {
            self.read_audio(v, op);
        }
    }

    pub(super) fn read_eref(&mut self, r: ERef, op: OpId) {
        if let ERef::Val(v) = r {
            self.read_event(v, op);
        }
    }
}

/// The position of `key` in the sorted feedback `list`.
pub(super) fn fb_index<F: Id>(key: FeedbackKey, list: &TableSlice<F, FeedbackKey>) -> F {
    F::from_index(
        list.as_slice()
            .binary_search(&key)
            .expect("feedback keys are collected before they are read"),
    )
}

/// `v` through its PDC delay of `by` frames, keyed `key`: the value itself
/// when `by` is zero (no op), else the output of a `Pre::Delay` ring.
pub(super) fn align_audio(em: &mut Emitted, v: AudioValueId, by: Samples, key: DelayKey) -> ARef {
    if by.is_zero() {
        return ARef::Val(v);
    }
    let delay = em.delays.push(DelaySpec { key, len: by });
    let (op, dv) = em.push_audio(|dst| Pre::Delay { delay, src: v, dst });
    em.read_audio(v, op);
    ARef::Val(dv)
}

/// Event value `v`, delayed by `by` through a ring keyed `key` when `by` is
/// not zero (the value itself otherwise).
pub(super) fn align_event(
    em: &mut Emitted,
    v: EventValueId,
    by: Samples,
    key: DelayKey,
) -> EventValueId {
    if by.is_zero() {
        return v;
    }
    let delay = em.delays.push(DelaySpec { key, len: by });
    let (op, dv) = em.push_event(|dst| Pre::EventDelay { delay, src: v, dst });
    em.read_event(v, op);
    dv
}

/// One addend of a sum: the value, and the edge whose gain weights it when
/// the edge has an entry.
#[derive(Clone, Copy)]
pub(super) struct Addend {
    pub(super) r: ARef,
    pub(super) lane: Option<EdgeId>,
    /// The event source that drives the lane's gain, and the event value
    /// that carries it to the sum (through its PDC delay when it needs one).
    pub(super) drive: Option<GainDrive>,
}

/// The event source that drives an addend's lane, and the event value that
/// carries it to the sum.
#[derive(Clone, Copy)]
pub(super) struct GainDrive {
    pub(super) from: EventOutputPort,
    pub(super) value: EventValueId,
}

/// Sum `refs` (already in source order) into one audio value: the left fold
/// `((+0.0 + a_0) + a_1) + ..`, each
/// weighted addend `acc + w * a`.
///
/// Silence (`ARef::Zero`) adds nothing and is dropped; none left is silence,
/// one plain one is itself (an alias, no op). A weighted one is an op even
/// alone (`+0.0 + w * a`: a plain alias keeps a `-0.0` and a weight does not).
/// Up to [`MAX_PORTS`] addends are one op. A wider fan-in is a **chain, not a
/// tree**: a float sum is not associative, so the first op folds the first
/// `MAX_PORTS` and each next one reads the last result as its first addend
/// (plain: its weight is `1.0`) and adds the following `MAX_PORTS - 1`. The
/// accumulator is never `-0.0`, so adding it to `+0.0` again changes no bit:
/// the chain is one flat fold, wherever it is cut. An op with no weighted
/// lane is a plain `Op::Sum`.
pub(super) fn sum_chain(em: &mut Emitted, refs: Vec<Addend>) -> ARef {
    let refs: Vec<Addend> = refs.into_iter().filter(|a| a.r != ARef::Zero).collect();
    let emit = |em: &mut Emitted, addends: Vec<Addend>| -> ARef {
        let srcs: Vec<ARef> = addends.iter().map(|a| a.r).collect();
        let lanes: Vec<Option<EdgeId>> = addends.iter().map(|a| a.lane).collect();
        let gains: Vec<GPort> = addends
            .iter()
            .enumerate()
            .filter_map(|(lane, a)| {
                a.drive.map(|d| GPort {
                    lane: lane as u16,
                    from: d.from,
                    value: d.value,
                })
            })
            .collect();
        let weighted = lanes.iter().any(Option::is_some);
        let (op, v) = em.push_audio(|dst| {
            if weighted {
                Pre::WeightedSum {
                    srcs: srcs.clone(),
                    lanes,
                    gains: gains.clone(),
                    dst,
                }
            } else {
                Pre::Sum {
                    srcs: srcs.clone(),
                    dst,
                }
            }
        });
        for &r in &srcs {
            em.read_aref(r, op);
        }
        // The sum reads each driving lane's events: a read of the op, so the
        // colouring and the verifier cover it.
        for g in &gains {
            em.read_event(g.value, op);
        }
        ARef::Val(v)
    };
    match refs.len() {
        0 => ARef::Zero,
        1 if refs[0].lane.is_none() => refs[0].r,
        len => {
            let first = len.min(MAX_PORTS);
            let mut acc = emit(em, refs[..first].to_vec());
            for run in refs[first..].chunks(MAX_PORTS - 1) {
                let mut addends = Vec::with_capacity(run.len() + 1);
                addends.push(Addend {
                    r: acc,
                    lane: None,
                    drive: None,
                });
                addends.extend_from_slice(run);
                acc = emit(em, addends);
            }
            acc
        }
    }
}

/// Merge `refs` (already in source order) into one event stream, in
/// `(offset, source order)`.
///
/// Up to [`MAX_PORTS`] sources is one `EventMerge` op. Wider fan-in becomes a
/// tree: contiguous runs of at most `MAX_PORTS` sources merge first, and the
/// run results merge in run order. Because each merge breaks ties toward the
/// lower input and the runs are contiguous and in order, a tie between
/// sources in different runs still goes to the earlier source — the tree
/// delivers exactly what one flat merge would.
///
/// With a `bound` (the node's `Shape::event_input_capacity`) the last merge
/// keeps at most that many events, and a single source goes through it too:
/// the bound holds whatever the edges are. The runs below it hold all their
/// sources.
pub(super) fn merge_tree(em: &mut Emitted, refs: Vec<ERef>, bound: Option<InputBound>) -> ERef {
    match refs.len() {
        0 => ERef::Empty,
        1 if bound.is_none() || refs[0] == ERef::Empty => refs[0],
        n if n <= MAX_PORTS => {
            let (op, mv) = em.push_event(|dst| Pre::EventMerge {
                srcs: refs.clone(),
                dst,
                bound,
            });
            for &r in &refs {
                em.read_eref(r, op);
            }
            ERef::Val(mv)
        }
        _ => {
            let runs: Vec<ERef> = refs
                .chunks(MAX_PORTS)
                .map(|run| merge_tree(em, run.to_vec(), None))
                .collect();
            merge_tree(em, runs, bound)
        }
    }
}
