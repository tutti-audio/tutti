//! What a commit's apply needs, built on the control side.
//!
//! Applying a plan on the audio thread only swaps and moves: everything it
//! needs that is a function of the plan is built here, when the plan is
//! packaged, and travels in the commit as a [`Staged`]. That is the whole new
//! [`State`] (a zeroed arena, the event slots, every ring and FIFO the plan
//! lists, the lanes, the injection buffers and the parallel state). Where each
//! piece of the old state moves to is the commit's [`Carry`], matched once by
//! the compiler, so the audio thread looks nothing up and builds nothing.
//!
//! The carried state itself (a ring's samples, a FIFO's queue, a lane's last
//! weight) lives on the audio thread and is read only there; what the control
//! side knows is how much room it can need. The [`Stager`] remembers, per
//! commit sent, an **upper bound** of every dynamic size the executor will
//! hold once that commit is applied: the capacity of each event FIFO, of each
//! injection buffer, of each chase buffer. A buffer built from the bounds of
//! the commit before it holds whatever the executor's state can then be, so
//! the audio thread never grows one: it moves a carried ring into the slot
//! of its key, copies a retuned ring or a FIFO's queue into the one built for
//! its new length, and writes flushed events into buffers made for them.
//! Everything the executor replaces goes back in the commit, freed on the
//! control thread.

use crate::compile::carry::Carry;
use crate::compile::ids::{EventDelayId, EventFeedbackId, EventSlotId, Id, PlanUnitId, Table};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tutti_buffers::Float;
use tutti_rt::{CappedVec, Gauge};

use super::commit::{Commit, Participants, Rendered};
use super::node_op::Flags;
use super::par::Par;
use super::state::{Gone, Inject, State, Unit};
use crate::EventTapPort;
use crate::NodeKey;
use crate::compile::plan::{EventDelayKey, Op, ParamPortOp, Plan, PlanUnit, UnitIdx};
use crate::edges::event_tap::EventTapWriter;
use crate::edges::meter::MeterWriter;
use crate::edges::tap::TapWriter;
use crate::event::types::Event;
use crate::exec::arena::Arena;
use crate::exec::command::COMMAND_CAPACITY;
use crate::exec::held::{Held, Keys};
use crate::exec::rings::{AudioRing, EventFifo};
use crate::exec::slots::EventSlot;
use crate::model::gain::SumLane;
use crate::model::spec::EventInputPort;
use crate::node::param::ParamState;

/// An input port of one of the new plan's units.
#[derive(Clone, Copy)]
pub(super) struct Sink {
    pub(super) unit: PlanUnitId,
    pub(super) port: u16,
}

/// Where a flush into a [`Sink`] reads from.
#[derive(Clone, Copy)]
pub(super) enum Origin {
    /// The old state's injection buffer, by position.
    Injection(usize),
    /// An old event delay's FIFO that has no successor.
    Delay(EventDelayId),
    /// An old event feedback FIFO that has no successor.
    Feedback(EventFeedbackId),
}

/// What disappears and flushes into a sink that survives.
pub(super) struct Delivery {
    pub(super) from: Origin,
    pub(super) to: Sink,
}

/// An old event edge whose source's held keys are closed at its sink.
pub(super) struct Closer {
    /// The old source's unit, by store index, and the port the edge leaves.
    pub(super) from: UnitIdx,
    pub(super) port: u16,
    /// The new plan keeps the source's unit (at the same index).
    pub(super) kept: bool,
    /// The new plan keeps the edge.
    pub(super) edge_kept: bool,
    pub(super) to: Sink,
    pub(super) route: Route,
}

/// Where the close of a cut edge's held keys goes. Either way the keys go
/// into a table that holds every key and pedal there is, so it cannot
/// overflow and no note off is lost; the table is drained as room frees,
/// pedal up first (with several cut edges into one sink, each table does).
pub(super) enum Route {
    /// The edge goes on through this delay of the plan: its FIFO was built
    /// with a table for owed closes, poured into the queue block by block,
    /// behind what is in flight.
    Delay(EventDelayId),
    /// The edge ended: the table the source's held and owed keys merge into,
    /// drained into the sink's injection buffer as far as it takes now and
    /// block by block after.
    Sink(Held),
    /// The table has been placed.
    Placed,
}

/// Everything a commit's apply installs, built on the control side.
pub(super) struct Staged<F: Float = f32> {
    /// The new state, shell: every ring and writer that comes from the
    /// commit is in place, the rest waits for what carries. An item whose
    /// shell is `None` is the one the commit's [`Carry`] names, and the apply
    /// moves the old one in; a shell the carry names an old item for takes
    /// its contents instead.
    pub(super) state: Option<State<F>>,
    /// The rate each event delay's FIFO is retuned to when it is kept.
    pub(super) delay_rates: Table<EventDelayId, usize>,
    /// The same for event feedback.
    pub(super) feedback_rates: Table<EventFeedbackId, usize>,
    /// Old event edges whose held keys may need closing at the sink.
    pub(super) closers: Vec<Closer>,
    /// Units (plan indices) that may hold a crossfade queued behind a
    /// running one.
    pub(super) queued: Vec<PlanUnitId>,
    /// What flushes into the new plan's sinks after what the replaced nodes
    /// wrote for their successors: undelivered injections, then the vanished
    /// event delays and feedback, in key order.
    pub(super) deliveries: Vec<Delivery>,
    /// Whether the old plan's unit at a store index gets its held-key tables
    /// back from a re-prepare. The stager sized no tables for one that does.
    pub(super) parked_back: Vec<bool>,
    /// The store index and the lane state of each unit whose mix a plan
    /// drives and that may have none.
    pub(super) drives: Vec<(UnitIdx, ParamState<F>)>,
    pub(super) spare_audio: CappedVec<AudioRing<F>>,
    pub(super) spare_events: CappedVec<EventFifo>,
    /// Tables of owed closes merged away, to be freed on the control side.
    pub(super) spare_tables: CappedVec<Held>,
    pub(super) spare_drives: CappedVec<ParamState<F>>,
    /// The executor's chase buffer from this plan on.
    pub(super) chase: CappedVec<Event>,
    /// A store with room for the plan's units, when the executor's has none.
    pub(super) store: Option<Vec<Option<Unit<F>>>>,
    /// The store length the plan needs.
    store_len: usize,
}

impl<F: Float> Staged<F> {
    /// Items its spare lists refused: the ones the apply leaked.
    pub(super) fn refused(&self) -> usize {
        self.spare_audio
            .refused()
            .saturating_add(self.spare_events.refused())
            .saturating_add(self.spare_tables.refused())
            .saturating_add(self.spare_drives.refused())
    }

    /// The parallel state for `participants` (the pool the executor has now),
    /// built for the plan this was staged for: the pool was set after the
    /// commit was sent. Control side: allocates.
    pub(super) fn set_participants(&mut self, plan: &Plan, participants: Option<usize>) {
        if let Some(state) = &mut self.state {
            state.par = Some(Par::new(
                plan,
                participants.unwrap_or(0),
                self.store_len,
                state.inject.iter().map(|i| i.unit),
            ));
        }
    }
}

/// One injection buffer the executor holds, for the next commit's flush:
/// what flushed into it (and may still be there) and the room kept for what a
/// fade's end delivers, which a render empties first.
struct Shell {
    key: NodeKey,
    port: u16,
    flush: usize,
    room: usize,
    /// Tables of owed closes it can hold.
    owed: usize,
}

/// What [`Stager::stage`] needs from the commit's editor.
pub(crate) struct StageIn<'a, F: Float> {
    /// The plan the executor runs before this commit.
    pub(crate) old: Option<&'a Plan>,
    /// The lane state for the mix of the unit at a key.
    pub(crate) mix_lane: &'a dyn Fn(NodeKey) -> Option<ParamState<F>>,
}

/// What every step of staging reads.
struct Cx<'a> {
    plan: &'a Plan,
    old: Option<&'a Plan>,
    /// What the plan takes over from `old`.
    carry: &'a Carry,
    /// The commit is a re-prepare's resume.
    resumed: bool,
    max_block: usize,
    cap: usize,
}

impl Cx<'_> {
    /// The input `at`, when the plan has it.
    fn sink_of(&self, at: EventInputPort) -> Option<Sink> {
        let unit = self.plan.unit_id(at.node)?;
        (at.port < self.plan.units[unit].shape.event_in).then_some(Sink {
            unit,
            port: at.port,
        })
    }

    /// Whether the old plan's unit at each store index gets its held-key
    /// tables back after a re-prepare: it is in the new plan, at the same
    /// generation (and so at the same index).
    fn parked_back(&self) -> Vec<bool> {
        let mut back = Vec::new();
        for o in self.old.iter().flat_map(|p| p.units.iter()) {
            let at = o.idx.0 as usize;
            back.resize(back.len().max(at + 1), false);
            back[at] = self
                .plan
                .unit(o.key)
                .is_some_and(|n| n.generation == o.generation);
        }
        back
    }
}

/// What the stager knows of the executor's state, as of the last commit it
/// sent: the bounds [`Staged`] is built from.
pub(crate) struct Stager {
    event_cap: usize,
    /// Pool participants the executor runs on (`Executor::set_pool`).
    pool: Gauge<Participants>,
    /// Blocks the executor has rendered (`Executor::render_applied`).
    rendered: Gauge<Rendered>,
    /// Each event delay's FIFO the executor holds, by the last plan's id.
    fifos: Table<EventDelayId, FifoMemo>,
    /// The same for event feedback.
    feedback: Table<EventFeedbackId, FifoMemo>,
    /// The injection buffers of the last commit. Kept by key, not by unit
    /// index: a buffer is folded into the rooms until `observe` sees its commit
    /// acked, so it can outlive a commit that gives its `UnitIdx` to another key.
    shells: Shells,
    /// The longest a replaced node's chase can be, by key.
    chase: BTreeMap<NodeKey, usize>,
    /// Crossfades sent and not yet returned, by key.
    fading: BTreeMap<NodeKey, u32>,
    /// The executor's store.
    store: StoreBound,
}

/// What the stager knows of one event FIFO the executor holds: an upper
/// bound, as of the last commit sent. A FIFO no old one continues starts
/// empty (`default`).
#[derive(Clone, Copy, Debug, Default)]
struct FifoMemo {
    /// The capacity of its queue.
    cap: usize,
    /// It holds a table for owed closes.
    owing: bool,
    /// It has room for param events.
    param_room: bool,
}

/// The injection buffers of the last commit, with the commit that made them
/// and, once it was seen applied, the block count then. The buffers are
/// dropped from the count two blocks after the commit applied.
struct Shells {
    list: Vec<Shell>,
    seq: u64,
    seen: Option<u64>,
}

impl Shells {
    fn new() -> Self {
        Self {
            list: Vec::new(),
            seq: 0,
            seen: None,
        }
    }

    /// The buffers of commit `seq`, not yet seen applied.
    fn replace(&mut self, list: Vec<Shell>, seq: u64) {
        *self = Self {
            list,
            seq,
            seen: None,
        };
    }

    /// The executor has applied commits up to `acked` and rendered `blocks`:
    /// the buffers are empty once it has rendered two blocks since it applied
    /// theirs.
    fn observe(&mut self, acked: u64, blocks: u64) {
        if self.list.is_empty() || acked < self.seq {
            return;
        }
        match self.seen {
            None => self.seen = Some(blocks),
            Some(seen) if blocks >= seen + 2 => {
                self.list.clear();
                self.seen = None;
            }
            Some(_) => {}
        }
    }
}

/// The length and capacity of the executor's unit store: the length the next
/// commit builds for, and the capacity the executor's vector was last grown
/// to, so growing moves into one built here.
struct StoreBound {
    len: usize,
    cap: usize,
}

impl StoreBound {
    /// The length a commit that needs `needed` units builds for.
    fn length_for(&self, needed: usize) -> usize {
        self.len.max(needed)
    }

    /// Record a store of `len`, and return the vector it grows into when
    /// `len` passes the capacity (doubling it).
    fn grow<T>(&mut self, len: usize) -> Option<Vec<T>> {
        let grown = (len > self.cap).then(|| {
            self.cap = len.max(self.cap * 2);
            Vec::with_capacity(self.cap)
        });
        self.len = len;
        grown
    }
}

/// What [`Stager::delays`] builds: the shells of the PDC rings and FIFOs, and
/// the bounds the next commit builds from.
struct Delays<F: Float> {
    /// The shell of each ring: `None` when the old ring of the same key
    /// moves in whole.
    audio: Vec<Option<AudioRing<F>>>,
    /// Rings whose old ring is copied into the shell and goes back.
    audio_moved: usize,
    event_fifos: Vec<Option<EventFifo>>,
    events: FifoPlan<EventDelayId>,
}

/// What [`Stager::feedback`] builds: the shells of the feedback rings and
/// FIFOs.
struct Feedback<F: Float> {
    audio: Vec<Option<AudioRing<F>>>,
    event: Vec<Option<EventFifo>>,
    fifo: FifoPlan<EventFeedbackId>,
}

/// The FIFO plan of the event delays or the event feedback: `I` is the id of
/// their table.
struct FifoPlan<I> {
    /// The most each FIFO's queue can hold past its new bound.
    extra: Vec<usize>,
    /// FIFOs whose old queue is copied into the shell and goes back.
    moved: usize,
    /// The rate each kept FIFO is retuned to.
    rate: Table<I, usize>,
    /// What the executor's FIFOs are once this commit applies.
    memo: Table<I, FifoMemo>,
}

impl<I: Id> FifoPlan<I> {
    fn new() -> Self {
        Self {
            extra: Vec::new(),
            moved: 0,
            rate: Table::default(),
            memo: Table::default(),
        }
    }
}

/// How an event FIFO of bound `want` takes over an old one of capacity
/// `have`: whether the old one is kept in place (retuned, built with room
/// enough), the capacity it has afterwards, and the most its queue can hold
/// past the bound.
fn fit_fifo(have: Option<usize>, want: usize) -> (bool, usize, usize) {
    match have {
        Some(have) if have >= want => (true, have, have - want),
        _ => (false, want, 0),
    }
}

impl Stager {
    pub(crate) fn new(
        event_cap: usize,
        pool: Gauge<Participants>,
        rendered: Gauge<Rendered>,
    ) -> Self {
        Self {
            event_cap,
            pool,
            rendered,
            fifos: Table::default(),
            feedback: Table::default(),
            shells: Shells::new(),
            chase: BTreeMap::new(),
            fading: BTreeMap::new(),
            store: StoreBound { len: 0, cap: 0 },
        }
    }

    /// A crossfade at `key` came back.
    pub(crate) fn faded(&mut self, key: NodeKey) {
        if let Some(n) = self.fading.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                self.fading.remove(&key);
            }
        }
    }

    /// The executor has applied commits up to `acked`. Injection buffers it
    /// built are empty after it renders: a block delivers what they hold,
    /// and the next the chase a fade's end puts in them.
    pub(crate) fn observe(&mut self, acked: u64) {
        let blocks = self.rendered.get().0.get();
        self.shells.observe(acked, blocks);
    }

    /// Builds what applying `c` needs, and puts it in `c`.
    pub(crate) fn stage<F: Float>(&mut self, c: &mut Commit<F>, input: StageIn<'_, F>) {
        let plan = Arc::clone(
            c.plan
                .as_ref()
                .expect("a commit carries its plan until applied"),
        );
        // Taken out for the steps below, which read it beside `c`; put back at
        // the end, for the apply.
        let carry = std::mem::take(&mut c.delta.carry);
        let cx = Cx {
            plan: &plan,
            old: input.old,
            carry: &carry,
            resumed: c.resumed,
            max_block: plan.prepare.max_block().get(),
            cap: self.event_cap,
        };
        self.track(c, &cx);
        let parked_back = if cx.resumed {
            cx.parked_back()
        } else {
            Vec::new()
        };
        Self::prepare_incoming(c, &cx, &parked_back);
        let closers = self.closers(c, &cx);
        let to_close: BTreeSet<EventDelayKey> = closers
            .iter()
            .filter_map(|k| match k.route {
                Route::Delay(i) => Some(plan.program.event_delays[i].key),
                _ => None,
            })
            .collect();
        let delays = self.delays(&cx, &to_close);
        let feedback = self.feedback(&cx);
        let outlets = outlets(c, &cx);
        let lanes = lanes(c, &cx);
        let drives = drives(c, &cx, input.mix_lane);
        let mut flush = self.flush(c, &cx, closers);
        let sizes = Sizes::of(&cx, &delays.events, &feedback.fifo);
        let (inject, shells) = self.shells(&cx, &flush, sizes.widest);
        let views = view_rooms(&plan, &inject, sizes.widest);
        let slot_capacity = |i: EventSlotId| {
            (plan.program.sizing.events[i].events(cx.cap) + sizes.extra[i]).max(views[i].0)
                + views[i].1
        };
        let param_slots = param_slots(&plan);
        let store_len = self.store.length_for(c.delta.store_len() as usize);
        let participants = self.pool.get().0;
        let par = Par::new(
            &plan,
            participants,
            store_len,
            inject.iter().map(|i| i.unit),
        );
        let state = State {
            arena: Arena::new(plan.program.sizing.audio.len(), cx.max_block),
            flags: {
                let mut v = vec![Flags::NONE; plan.program.sizing.audio.len()];
                v[0] = Flags::SILENCE;
                v
            },
            events: plan
                .program
                .sizing
                .events
                .ids()
                .map(|i| {
                    let params = if param_slots[i] { slot_capacity(i) } else { 0 };
                    EventSlot::with_capacity(slot_capacity(i), params)
                })
                .collect(),
            audio_delays: delays.audio,
            event_delays: delays.event_fifos,
            audio_fb: feedback.audio,
            event_fb: feedback.event,
            taps: outlets.taps,
            event_taps: outlets.event_taps,
            meters: outlets.meters,
            lanes,
            inject,
            has_inject: Table::filled(plan.units.len(), false),
            has_due: Table::filled(plan.units.len(), false),
            par: Some(par),
        };
        // The store grows by moving into one built here.
        let store = self.store.grow(store_len);
        let tables = flush
            .rooms
            .values()
            .map(|r| r.closers + r.carried)
            .sum::<usize>();
        let spare_drives = carry.lanes.iter().flatten().count() + drives.len();
        let moved_events = delays.events.moved + feedback.fifo.moved;
        reserve_returns(c, &cx);
        let wanted: usize = plan
            .units
            .iter()
            .filter_map(|u| u.shape.chase)
            .map(|n| n.get() as usize)
            .sum();
        c.staged = Some(Box::new(Staged {
            state: Some(state),
            delay_rates: delays.events.rate,
            feedback_rates: feedback.fifo.rate,
            closers: std::mem::take(&mut flush.closers),
            queued: std::mem::take(&mut flush.queued),
            deliveries: std::mem::take(&mut flush.deliveries),
            parked_back,
            spare_drives: CappedVec::with_capacity(spare_drives),
            drives,
            spare_audio: CappedVec::with_capacity(delays.audio_moved),
            spare_events: CappedVec::with_capacity(moved_events),
            spare_tables: CappedVec::with_capacity(tables),
            chase: CappedVec::with_capacity(wanted),
            store,
            store_len,
        }));
        c.delta.carry = carry;
        // The bounds the next commit builds from.
        self.fifos = delays.events.memo;
        self.feedback = feedback.fifo.memo;
        self.shells.replace(shells, c.seq);
    }

    /// Crossfades in flight, and how long a chase can be, by key.
    fn track<F: Float>(&mut self, c: &Commit<F>, cx: &Cx<'_>) {
        for (key, f) in c.delta.fades() {
            if !f.is_cut() {
                *self.fading.entry(*key).or_default() += 1;
            }
        }
        let old_units = cx.old.iter().flat_map(|p| p.units.iter());
        for u in cx.plan.units.iter().chain(old_units) {
            if let Some(n) = u.shape.chase {
                let e = self.chase.entry(u.key).or_default();
                *e = (*e).max(n.get() as usize);
            }
        }
        self.chase
            .retain(|k, _| cx.plan.unit(*k).is_some() || self.fading.contains_key(k));
    }

    /// PDC rings and FIFOs: the shell of each, built for the ones the commit's
    /// [`Carry`] does not move in whole, and what the executor's FIFOs are
    /// once it applies.
    fn delays<F: Float>(&self, cx: &Cx<'_>, to_close: &BTreeSet<EventDelayKey>) -> Delays<F> {
        let plan = cx.plan;
        let mut audio_moved = 0;
        let audio = plan
            .program
            .audio_delays
            .iter_enumerated()
            .map(|(n, d)| match cx.carry.audio_rings[n] {
                // The old ring has the length: it moves in whole.
                Some(o)
                    if cx
                        .old
                        .is_some_and(|p| p.program.audio_delays[o].len == d.len) =>
                {
                    None
                }
                carried => {
                    audio_moved += usize::from(carried.is_some());
                    Some(AudioRing::new(d.len))
                }
            })
            .collect();
        let mut events = FifoPlan::new();
        let mut event_fifos = Vec::with_capacity(plan.program.event_delays.len());
        for (n, d) in plan.program.event_delays.iter_enumerated() {
            let carried = cx.carry.event_rings[n];
            let before = carried.map_or_else(FifoMemo::default, |o| self.fifo_memo(o));
            let src = d.key.origin();
            let rate = plan.event_tap_rate(src, d.len, cx.max_block, cx.cap);
            let want = EventFifo::bound(d.len.get(), rate, cx.max_block);
            let (mut keep, mut cap, over) = fit_fifo(carried.map(|_| before.cap), want);
            // A FIFO that owes closes has a table for them: one that
            // has none yet is built afresh and takes the queue over.
            let table = before.owing || to_close.contains(&d.key);
            // A FIFO for a source that writes param events has room
            // for them: one kept from before the source did is built
            // afresh and takes the queue over.
            let params = plan.event_tap_params(src);
            if keep && (!before.owing && table || !before.param_room && params) {
                keep = false;
                cap = cap.max(want);
            }
            event_fifos.push((!keep).then(|| {
                let f = EventFifo::sized_for(d.len, rate, cx.max_block, cap);
                let f = if params { f.with_params() } else { f };
                if table { f.with_owed() } else { f }
            }));
            events.moved += usize::from(carried.is_some() && !keep);
            events.extra.push(over);
            events.rate.push(rate);
            events.memo.push(FifoMemo {
                cap,
                owing: table,
                param_room: params,
            });
        }
        Delays {
            audio,
            audio_moved,
            event_fifos,
            events,
        }
    }

    /// What the executor's FIFO `o` of the last plan is, as of the last commit.
    /// A carried FIFO was in that plan, whose table the memo is; one with no
    /// entry counts as nothing.
    fn fifo_memo(&self, o: EventDelayId) -> FifoMemo {
        self.fifos.get(o).copied().unwrap_or_default()
    }

    /// Feedback delays, by key: exactly the edge's delay long.
    fn feedback<F: Float>(&self, cx: &Cx<'_>) -> Feedback<F> {
        let plan = cx.plan;
        let audio = plan
            .program
            .audio_feedback
            .iter_enumerated()
            .map(|(n, f)| {
                cx.carry.audio_fb[n]
                    .is_none()
                    .then(|| AudioRing::new(f.key.delay))
            })
            .collect();
        let mut fifo = FifoPlan::new();
        let mut event = Vec::with_capacity(plan.program.event_feedback.len());
        for (n, f) in plan.program.event_feedback.iter_enumerated() {
            let carried = cx.carry.event_fb[n];
            let before = carried.map_or_else(FifoMemo::default, |o| {
                self.feedback.get(o).copied().unwrap_or_default()
            });
            let src = f.key.from;
            let rate =
                plan.event_tap_rate(EventTapPort::Out(src), f.key.delay, cx.max_block, cx.cap);
            let want = EventFifo::bound(f.key.delay.get(), rate, cx.max_block);
            let (mut keep, mut cap, over) = fit_fifo(carried.map(|_| before.cap), want);
            // As a delay's FIFO: one kept from before its source wrote
            // param events is built afresh.
            let params = plan.event_tap_params(EventTapPort::Out(src));
            if keep && !before.param_room && params {
                keep = false;
                cap = cap.max(want);
            }
            event.push((!keep).then(|| {
                let fifo = EventFifo::sized_for(f.key.delay, rate, cx.max_block, cap);
                if params { fifo.with_params() } else { fifo }
            }));
            fifo.moved += usize::from(carried.is_some() && !keep);
            fifo.extra.push(over);
            fifo.rate.push(rate);
            fifo.memo.push(FifoMemo {
                cap,
                owing: false,
                param_room: params,
            });
        }
        Feedback { audio, event, fifo }
    }

    /// What disappears flushes to sinks that survive, and held keys of an
    /// edge that ended are closed at the sink that lost it: each sink gets a
    /// buffer for everything that can flow into it, and each flush its sink.
    fn flush<F: Float>(&self, c: &Commit<F>, cx: &Cx<'_>, closers: Vec<Closer>) -> Flush {
        let plan = cx.plan;
        let mut rooms: BTreeMap<(PlanUnitId, u16), Room> = BTreeMap::new();
        let mut deliveries = Vec::new();
        // What replaced nodes wrote for their successors.
        for i in &c.install {
            let sink = cx.sink_of(EventInputPort::new(i.at.key, 0));
            if let (Some(n), Some(to)) = (i.chase, sink) {
                rooms.entry((to.unit, 0)).or_default().flush += n.get() as usize;
            }
        }
        // Injections not yet delivered: each buffer counts once, with what
        // flushed into it before, and its room for a fade's end. The list
        // is the executor's injection buffers, in their order.
        for (n, s) in self.shells.list.iter().enumerate() {
            if let Some(to) = cx.sink_of(EventInputPort::new(s.key, s.port)) {
                let r = rooms.entry((to.unit, to.port)).or_default();
                r.flush += s.flush;
                r.room = r.room.max(s.room);
                r.carried += s.owed;
                deliveries.push(Delivery {
                    from: Origin::Injection(n),
                    to,
                });
            }
        }
        // Event delays and feedback that no longer exist: what their queues
        // hold flushes to the sink.
        for g in &cx.carry.vanished_delays {
            if let Some(to) = cx.sink_of(g.at) {
                let memo = self.fifo_memo(g.from);
                let r = rooms.entry((to.unit, to.port)).or_default();
                r.flush += memo.cap;
                // What the FIFO still owes moves to the sink.
                r.carried += usize::from(memo.owing);
                deliveries.push(Delivery {
                    from: Origin::Delay(g.from),
                    to,
                });
            }
        }
        for g in &cx.carry.vanished_feedback {
            if let Some(to) = cx.sink_of(g.at) {
                let have = self.feedback.get(g.from).copied().unwrap_or_default();
                rooms.entry((to.unit, to.port)).or_default().flush += have.cap;
                deliveries.push(Delivery {
                    from: Origin::Feedback(g.from),
                    to,
                });
            }
        }
        for k in &closers {
            if matches!(k.route, Route::Sink(_)) {
                rooms.entry((k.to.unit, k.to.port)).or_default().closers += 1;
            }
        }
        // A crossfade queued behind a running one holds what its predecessor
        // wrote for the unit that waits: delivered when the fade ends, into
        // the unit's first event input.
        let mut queued = Vec::new();
        for (u, unit) in plan.units.iter_enumerated() {
            let Some(&n) = self.chase.get(&unit.key) else {
                continue;
            };
            if !self.fading.contains_key(&unit.key) {
                continue;
            }
            queued.push(u);
            if unit.shape.event_in > 0 {
                let r = rooms.entry((u, 0)).or_default();
                r.room = r.room.max(n);
            }
        }
        Flush {
            rooms,
            closers,
            queued,
            deliveries,
        }
    }

    /// The old event edges whose held keys may need closing at the sink: an
    /// edge that ended, or one whose source may be gone, from a source that
    /// carries notes. Each goes into a table ([`Route`]) that holds every key and
    /// pedal there is, so it cannot overflow, drained block by block as room
    /// frees, as the owed keys of a bypassed unit are. No close is ever
    /// refused for room, and no note is left stuck.
    fn closers<F: Float>(&self, c: &Commit<F>, cx: &Cx<'_>) -> Vec<Closer> {
        let Some(old) = cx.old else {
            return Vec::new();
        };
        let plan = cx.plan;
        // The sources that may be gone: every key a commit retires, replaces
        // or cuts (after a re-prepare, the units that do not come back are
        // the ones the delta retires or replaces).
        let maybe_gone: BTreeSet<NodeKey> = c
            .delta
            .retire()
            .iter()
            .chain(c.delta.replace().iter().map(|(o, _)| o))
            .chain(c.delta.cuts())
            .map(|p| p.key)
            .collect();
        let replaced: BTreeSet<NodeKey> = c.delta.replace().iter().map(|(_, n)| n.key).collect();
        let mut closers = Vec::new();
        for e in &old.event_edges {
            let &(from, at) = e;
            let in_plan = plan.event_edges.binary_search(e).is_ok();
            if (in_plan && !maybe_gone.contains(&from.node)) || replaced.contains(&at.node) {
                continue;
            }
            let source = old.unit(from.node).filter(|f| f.shape.note_events);
            let (Some(to), Some(source)) = (cx.sink_of(at), source) else {
                continue;
            };
            let delay = plan
                .program
                .event_delays
                .iter_enumerated()
                .find(|(_, d)| d.key == EventDelayKey::Edge { at, from })
                .map(|(id, _)| id);
            let route = match delay {
                Some(i) if in_plan => Route::Delay(i),
                _ => Route::Sink(Held::new()),
            };
            closers.push(Closer {
                from: source.idx,
                port: from.port,
                kept: plan.unit(from.node).is_some(),
                edge_kept: in_plan,
                to,
                route,
            });
        }
        closers
    }

    /// The injection buffers: one for each sink that can be flushed to, sorted
    /// by unit and port, and the bounds they leave for the next commit.
    fn shells(&self, cx: &Cx<'_>, flush: &Flush, widest: usize) -> (Vec<Inject>, Vec<Shell>) {
        let mut shells = Vec::with_capacity(flush.rooms.len());
        let inject = flush
            .rooms
            .iter()
            .map(|(&(unit, port), r)| {
                // A sink owed closes has a block's worth of room more, for them.
                let owed = r.closers + r.carried;
                let drain = if owed > 0 { widest.max(cx.cap) } else { 0 };
                let events = r.flush + r.room + drain;
                shells.push(Shell {
                    key: cx.plan.units[unit].key,
                    port,
                    flush: r.flush + drain,
                    room: r.room,
                    owed,
                });
                Inject {
                    unit,
                    port,
                    events: CappedVec::with_capacity(events),
                    live: false,
                    closed_from: 0,
                    owed: CappedVec::with_capacity(owed),
                }
            })
            .collect();
        (inject, shells)
    }

    /// What the units that arrive hold: held-key tables, and room for a
    /// crossfade's chase. Each replaced unit's chase is read off the old plan
    /// here, for the flush and the apply.
    fn prepare_incoming<F: Float>(c: &mut Commit<F>, cx: &Cx<'_>, parked_back: &[bool]) {
        for inst in &mut c.install {
            let Some(shape) = cx.plan.unit(inst.at.key).map(|u| u.shape) else {
                continue;
            };
            let ports = shape.event_out as usize;
            let back = parked_back.get(inst.at.idx.0 as usize) == Some(&true);
            let inc = &mut inst.incoming;
            if ports > 0 && !back {
                inc.keys.size(ports, shape.note_events, false);
            }
            if ports > 0 && inc.fade.is_some() {
                inc.staging = Keys::staging(ports, shape.note_events);
            }
            inst.chase = inst
                .was
                .and_then(|w| cx.old?.unit(w.key))
                .and_then(|u| u.shape.chase);
            if let Some(f) = &mut inc.fade {
                f.chase = CappedVec::with_capacity(inst.chase.map_or(0, |n| n.get() as usize));
            }
        }
    }
}

/// The sinks a commit can flush to, and what each is flushed from.
struct Flush {
    rooms: BTreeMap<(PlanUnitId, u16), Room>,
    closers: Vec<Closer>,
    queued: Vec<PlanUnitId>,
    deliveries: Vec<Delivery>,
}

/// The sizes a plan's slots and buffers are built to.
struct Sizes {
    /// Per event slot, what a FIFO carried past its new bound can add.
    extra: Table<EventSlotId, usize>,
    /// The widest event slot.
    widest: usize,
}

impl Sizes {
    fn of(
        cx: &Cx<'_>,
        rings: &FifoPlan<EventDelayId>,
        feedback: &FifoPlan<EventFeedbackId>,
    ) -> Self {
        let plan = cx.plan;
        let extra = widen(cx, &rings.extra, &feedback.extra);
        let widest = plan
            .program
            .sizing
            .events
            .iter_enumerated()
            .map(|(i, cap)| cap.events(cx.cap) + extra[i])
            .max()
            .unwrap_or(0);
        Self { extra, widest }
    }
}

/// What each view slot holds beyond its source, as `(floor, room)`: the
/// slot holds at least `floor` events (a bypassed node's closes drain into
/// it a block at a time, as many as the widest slot holds), and `room` more:
/// the commands a block brings and the events flushed to the port. Slots
/// that are no node's view get `(0, 0)`.
fn view_rooms(plan: &Plan, inject: &[Inject], widest: usize) -> Table<EventSlotId, (usize, usize)> {
    let mut rooms = Table::filled(plan.program.sizing.events.len(), (0, 0));
    for op in &plan.program.ops {
        let Op::Node { unit, .. } = *op else {
            continue;
        };
        let ports = plan.ports[unit].slots(&plan.nodes);
        for (port, &view) in ports.ein.iter().enumerate() {
            let flushed = inject
                .binary_search_by_key(&(unit, port as u16), |i| (i.unit, i.port))
                .map_or(0, |i| inject[i].events.capacity());
            let r = &mut rooms[view];
            *r = (r.0.max(widest), r.1.max(flushed + COMMAND_CAPACITY));
        }
    }
    rooms
}

/// What can flow into one injection buffer.
#[derive(Default)]
struct Room {
    /// Flushed events, and what stayed from the commit before.
    flush: usize,
    /// What a fade's end delivers later.
    room: usize,
    /// Edges cut into this sink whose held keys are closed through a table:
    /// each table is drained into the buffer, a block's worth at a time.
    closers: usize,
    /// Tables still owed from the commit before, which move over.
    carried: usize,
}

/// The writers a plan names, taps first: each is the one opened since the
/// last commit, which wins over a carried one of the same id, or `None`, for
/// the writer the commit's [`Carry`] moves in.
struct Outlets<F: Float> {
    taps: Vec<Option<TapWriter<F>>>,
    event_taps: Vec<Option<EventTapWriter>>,
    meters: Vec<Option<MeterWriter>>,
}

/// Taps, event taps and meters, by id. A writer the plan does not name stays
/// in the old state (or the commit) and is freed on the control side.
fn outlets<F: Float>(c: &mut Commit<F>, cx: &Cx<'_>) -> Outlets<F> {
    let plan = cx.plan;
    fn open<I: PartialEq, W>(opened: &mut Vec<(I, W)>, id: &I) -> Option<W> {
        let at = opened.iter().position(|(o, _)| o == id)?;
        Some(opened.swap_remove(at).1)
    }
    Outlets {
        taps: plan
            .program
            .taps
            .iter()
            .map(|k| open(&mut c.opened.taps, k))
            .collect(),
        event_taps: plan
            .program
            .event_taps
            .iter()
            .map(|k| open(&mut c.opened.event_taps, k))
            .collect(),
        meters: plan
            .program
            .meters
            .iter()
            .map(|(k, lanes)| {
                let w = open(&mut c.opened.meters, k);
                if let Some(w) = &w {
                    w.set_width(*lanes);
                }
                w
            })
            .collect(),
    }
}

/// Weighted lanes, by edge: what a lane holds carries across a commit that
/// keeps the edge (not a sample-rate change's reset: a weight is not time),
/// and a new edge starts at its value. A lane a source drives has its state
/// built here.
fn lanes<F: Float>(c: &Commit<F>, cx: &Cx<'_>) -> Vec<SumLane<F>> {
    let plan = cx.plan;
    let mut driven = vec![false; plan.program.sum_lanes.len()];
    for op in &plan.program.ops {
        if let Op::WeightedSum { lanes, gains, .. } = *op {
            for g in &plan.program.gain_ports[gains.range()] {
                driven[lanes.range().start + usize::from(g.lane)] = true;
            }
        }
    }
    plan.program
        .sum_lanes
        .iter()
        .zip(&driven)
        .map(|(id, &driven)| match id {
            None => SumLane::plain(),
            Some(id) => SumLane::weighted(
                c.opened
                    .gains
                    .get(id)
                    .cloned()
                    .expect("the editor sends a cell for every weighted lane"),
                driven,
                cx.max_block,
            ),
        })
        .collect()
}

/// The lanes that drive a unit's mix: for a unit placed now, or one no
/// earlier plan drove.
fn drives<F: Float>(
    c: &Commit<F>,
    cx: &Cx<'_>,
    mix_lane: &dyn Fn(NodeKey) -> Option<ParamState<F>>,
) -> Vec<(UnitIdx, ParamState<F>)> {
    let placed: BTreeSet<NodeKey> = c.install.iter().map(|i| i.at.key).collect();
    // A kept unit is at one index in both plans: it needs a lane only when
    // the old plan did not drive its mix.
    let driven_before = |key| {
        cx.old
            .and_then(|p| p.unit_id(key).map(|u| drives_mix(p, u)))
            .unwrap_or(false)
    };
    mix_driven(cx.plan)
        .filter_map(|pu| {
            (placed.contains(&pu.key) || !driven_before(pu.key))
                .then(|| mix_lane(pu.key).map(|lane| (pu.idx, lane)))
                .flatten()
        })
        .collect()
}

/// Whether the last driven param of `unit` is the mix.
fn drives_mix(plan: &Plan, unit: PlanUnitId) -> bool {
    plan.program.param_ports[plan.ports[unit].params.range()]
        .last()
        .is_some_and(ParamPortOp::is_mix)
}

/// The units of `plan` whose last driven param is the mix.
fn mix_driven(plan: &Plan) -> impl Iterator<Item = &PlanUnit> {
    plan.units
        .iter_enumerated()
        .filter(|&(unit, _)| drives_mix(plan, unit))
        .map(|(_, pu)| pu)
}

/// Per event slot, whether it can hold param events: a slot a param-writing
/// node's output is placed in ([`Shape::param_events`](crate::Shape::param_events)),
/// a feedback slot of one, or a delay's or a merge's output with such a slot
/// among its inputs. Only these slots reserve room for them. A slot the
/// colouring shares among values takes the union over the plan.
fn param_slots(plan: &Plan) -> Table<EventSlotId, bool> {
    let mut has = Table::filled(plan.program.sizing.events.len(), false);
    for spec in plan.program.event_feedback.iter() {
        has[spec.slot] |= plan.event_port_params(spec.key.from);
    }
    // Ops run in topological order, so a merge comes after what feeds it.
    for op in &plan.program.ops {
        match *op {
            Op::Node {
                unit, event_src, ..
            } => {
                let ports = plan.ports[unit].slots(&plan.nodes);
                let takes = plan.units[unit].shape.param_events;
                // A view carries the param events of its source, and those
                // of the commands scheduled to a node that takes them.
                for (&view, &src) in ports.ein.iter().zip(plan.event_slots(event_src)) {
                    has[view] |= takes || (src != crate::compile::plan::EMPTY_SLOT && has[src]);
                }
                if takes {
                    for &s in ports.eout {
                        has[s] = true;
                    }
                }
            }
            Op::EventDelay { src, dst, .. } => {
                let from = has[src];
                has[dst] |= from;
            }
            Op::EventMerge { srcs, dst, .. } => {
                let any = plan.program.event_list[srcs.range()]
                    .iter()
                    .any(|&s| s != crate::compile::plan::EMPTY_SLOT && has[s]);
                has[dst] |= any;
            }
            _ => {}
        }
    }
    has
}

/// A FIFO carried across a commit that lowered its bound can hold more than
/// the bound its output slot was priced at: each slot is widened by the most
/// that can be, and so is every merge downstream of it.
fn widen(cx: &Cx<'_>, ring_extra: &[usize], efb_extra: &[usize]) -> Table<EventSlotId, usize> {
    let plan = cx.plan;
    let mut extra = Table::filled(plan.program.sizing.events.len(), 0usize);
    debug_assert_eq!(plan.program.event_feedback.len(), efb_extra.len());
    for (f, &x) in plan.program.event_feedback.ids().zip(efb_extra) {
        let e = &mut extra[plan.program.event_feedback[f].slot];
        *e = (*e).max(x);
    }
    // Ops run in topological order, so a merge comes after whatever feeds it.
    for op in &plan.program.ops {
        match *op {
            Op::EventDelay { delay, dst, .. } => {
                let e = &mut extra[dst];
                *e = (*e).max(ring_extra[delay.index()]);
            }
            Op::EventMerge { srcs, dst, .. } => {
                let sum = plan.program.event_list[srcs.range()]
                    .iter()
                    .map(|&s| extra[s])
                    .sum::<usize>();
                let e = &mut extra[dst];
                *e = (*e).max(sum);
            }
            Op::Node {
                unit, event_src, ..
            } => {
                let ports = plan.ports[unit].slots(&plan.nodes);
                for (&view, &src) in ports.ein.iter().zip(plan.event_slots(event_src)) {
                    let from = extra[src];
                    let e = &mut extra[view];
                    *e = (*e).max(from);
                }
            }
            _ => {}
        }
    }
    extra
}

/// Room for what applying hands back.
fn reserve_returns<F: Float>(c: &mut Commit<F>, cx: &Cx<'_>) {
    let replaces = c.delta.replace().len();
    let parked = if c.resumed {
        cx.old.map_or(0, |o| o.units.len())
    } else {
        0
    };
    let held = c.delta.retire().len() + replaces + c.delta.cuts().len() + parked;
    c.gone = Gone {
        held: CappedVec::with_capacity(held),
        spare: CappedVec::with_capacity(2 * (c.delta.retire().len() + replaces + parked)),
        chased: CappedVec::with_capacity(replaces),
        held_refused: CappedVec::with_capacity(replaces),
        chase_events: CappedVec::default(),
    };
    c.spare_held = CappedVec::with_capacity(held + c.gone.spare.capacity() + 4 * c.install.len());
}

/// Stable sort of `events` by offset, in place and without allocating: the
/// merge sort of the standard library takes a scratch buffer.
pub(super) fn sort_by_offset(events: &mut [Event]) {
    for i in 1..events.len() {
        let key = events[i].offset;
        let at = events[..i].partition_point(|e| e.offset <= key);
        events[at..=i].rotate_right(1);
    }
}
