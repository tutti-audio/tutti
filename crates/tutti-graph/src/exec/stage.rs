//! What a commit's apply needs, built on the control side.
//!
//! Applying a plan on the audio thread only swaps and moves: everything it
//! needs that is a function of the plan is built here, when the plan is
//! packaged, and travels in the commit as a [`Staged`]. That is the whole new
//! [`State`] (a zeroed arena, the event slots, every ring and FIFO the plan
//! lists, the lanes, the injection buffers and the parallel state) and the
//! maps that say where each piece of the old state moves to, so the audio
//! thread looks nothing up and builds nothing.
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

use crate::compile::ids::{
    AudioDelayId, EventDelayId, EventFeedbackId, EventSlotId, Id, PlanUnitId, Table, TableSlice,
};
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
use crate::compile::plan::{
    AudioDelay, AudioDelayKey, EventDelayKey, EventFeedbackKey, Op, ParamPortOp, Plan, UnitIdx,
};
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
use crate::model::spec::{EventInputPort, EventOutputPort};
use crate::node::param::ParamState;

/// How a PDC ring of the new plan gets its state from the executor's old
/// state: `from` is the old plan's id of the ring.
#[derive(Clone, Copy, Debug)]
pub(super) enum AudioCarry {
    /// Nothing carries: the ring built for it starts empty.
    New,
    /// The old ring moves in whole.
    Keep { from: AudioDelayId },
    /// The old ring is copied into the one built for the new length, and goes
    /// back in the commit.
    Move { from: AudioDelayId },
    /// The old ring is copied into the one built for it and stays where it
    /// is, for the ring that takes it over: an old main output's channel, kept
    /// sounding while the new main output's channel of the same source goes on
    /// with the same history.
    Copy { from: AudioDelayId },
}

/// How a FIFO of the new plan, a delay's or a feedback's, gets its state from
/// the executor's old state: `from` is the old plan's id `I` of the FIFO.
#[derive(Clone, Copy, Debug)]
pub(super) enum EventCarry<I> {
    /// Nothing carries: the FIFO built for it starts empty.
    New,
    /// The old FIFO moves in whole and is retuned to the new length at `rate`
    /// (built with room for it, so nothing allocates).
    Keep { from: I, rate: usize },
    /// The old FIFO's queue is copied into the one built for the new length,
    /// and the old one goes back in the commit.
    Move { from: I },
}

/// An old event edge whose source's held keys are closed at its sink.
pub(super) struct Closer {
    pub(super) from: EventOutputPort,
    pub(super) at: EventInputPort,
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

/// A delay or feedback FIFO that goes away: its queue flushes into `at`.
pub(super) struct Vanished<I> {
    pub(super) from: I,
    pub(super) at: EventInputPort,
}

/// Everything a commit's apply installs, built on the control side.
pub(super) struct Staged<F: Float = f32> {
    /// Whether time-based state carries (no sample-rate change).
    pub(super) carry: bool,
    /// The new state, shell: every ring and writer that comes from the
    /// commit is in place, the rest waits for what carries.
    pub(super) state: Option<State<F>>,
    pub(super) audio_delays: Vec<AudioCarry>,
    pub(super) event_delays: Vec<EventCarry<EventDelayId>>,
    pub(super) event_fb: Vec<EventCarry<EventFeedbackId>>,
    /// The old index each audio feedback ring, tap, event tap, meter and
    /// lane of the new plan takes over, if any.
    pub(super) audio_fb: Vec<Option<u32>>,
    pub(super) taps: Vec<Option<u32>>,
    pub(super) event_taps: Vec<Option<u32>>,
    pub(super) meters: Vec<Option<u32>>,
    pub(super) lanes: Vec<Option<u32>>,
    /// Old event delays and event feedback that no longer exist: their
    /// queues flush, in key order.
    pub(super) gone_delays: Vec<Vanished<EventDelayId>>,
    pub(super) gone_feedback: Vec<Vanished<EventFeedbackId>>,
    /// Old event edges whose held keys may need closing at the sink.
    pub(super) closers: Vec<Closer>,
    /// Units (plan indices) that may hold a crossfade queued behind a
    /// running one.
    pub(super) queued: Vec<PlanUnitId>,
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
    /// Whether time-based state carries: false for the resume of a re-prepare
    /// that changed the sample rate.
    pub(crate) carry: bool,
    /// This commit is a re-prepare's resume: every unit comes back, and the
    /// held-key tables of those the plan keeps go back to them.
    pub(crate) resumed: bool,
    /// The lane state for the mix of the unit at a key.
    pub(crate) mix_lane: &'a dyn Fn(NodeKey) -> Option<ParamState<F>>,
}

/// What every step of staging reads.
struct Cx<'a> {
    plan: &'a Plan,
    old: Option<&'a Plan>,
    carry: bool,
    resumed: bool,
    max_block: usize,
    cap: usize,
}

impl Cx<'_> {
    /// The plan unit index of the input `at`, when the plan has it.
    fn sink_of(&self, at: EventInputPort) -> Option<PlanUnitId> {
        let u = self.plan.unit_id(at.node)?;
        (at.port < self.plan.units[u].shape.event_in).then_some(u)
    }

    /// Whether a unit of the old plan comes back after a re-prepare: it is
    /// in the new plan, at the same generation.
    fn parked_back(&self, key: NodeKey) -> bool {
        self.resumed
            && self
                .old
                .and_then(|o| o.unit(key))
                .zip(self.plan.unit(key))
                .is_some_and(|(a, b)| a.generation == b.generation)
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
    /// The capacity of each event delay's FIFO, by key.
    fifos: BTreeMap<EventDelayKey, usize>,
    /// The same for event feedback.
    feedback: BTreeMap<EventFeedbackKey, usize>,
    /// The event delays whose FIFO holds a table for owed closes.
    owing: BTreeSet<EventDelayKey>,
    /// The event delays whose FIFO has room for param events, and the same
    /// for event feedback.
    param_fifos: BTreeSet<EventDelayKey>,
    param_feedback: BTreeSet<EventFeedbackKey>,
    /// The injection buffers of the last commit.
    shells: Shells,
    /// The longest a replaced node's chase can be, by key.
    chase: BTreeMap<NodeKey, usize>,
    /// Crossfades sent and not yet returned, by key.
    fading: BTreeMap<NodeKey, u32>,
    /// The executor's store.
    store: StoreBound,
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

/// What [`Stager::delays`] builds: the shells of the PDC rings and FIFOs and
/// how each takes over the old one, and the bounds the next commit builds from.
struct Delays<F: Float> {
    audio: Vec<Option<AudioRing<F>>>,
    audio_from: Vec<AudioCarry>,
    event_fifos: Vec<Option<EventFifo>>,
    events: FifoPlan<EventDelayId>,
    /// The capacity of each event FIFO, by key.
    caps: BTreeMap<EventDelayKey, usize>,
    /// The event delays whose FIFO holds a table for owed closes.
    owing: BTreeSet<EventDelayKey>,
    /// The event delays whose FIFO has room for param events.
    with_params: BTreeSet<EventDelayKey>,
}

/// The ids of the rows of `table` by their keys.
fn keyed<I: Id, T, K: Ord>(table: &TableSlice<I, T>, key: impl Fn(&T) -> K) -> BTreeMap<K, I> {
    table.iter_enumerated().map(|(i, t)| (key(t), i)).collect()
}

/// Whether an event FIFO's old state is moved into its new one.
fn is_move<I>(carry: &&EventCarry<I>) -> bool {
    matches!(carry, EventCarry::Move { .. })
}

/// The FIFO plan of the event delays or the event feedback: `I` is the id of
/// their table.
struct FifoPlan<I> {
    from: Vec<EventCarry<I>>,
    /// The most each FIFO's queue can hold past its new bound.
    extra: Vec<usize>,
}

/// Which way a FIFO of bound `want` takes over an old one of capacity
/// `have`, and the capacity it has afterwards.
fn fit_fifo<I>(
    from: Option<(I, usize)>,
    want: usize,
    rate: usize,
) -> (EventCarry<I>, usize, usize) {
    match from {
        // Room enough: the old FIFO is retuned in place, and keeps its
        // capacity; what it holds past the new bound is its excess.
        Some((j, have)) if have >= want => (EventCarry::Keep { from: j, rate }, have, have - want),
        Some((j, _)) => (EventCarry::Move { from: j }, want, 0),
        None => (EventCarry::New, want, 0),
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
            fifos: BTreeMap::new(),
            feedback: BTreeMap::new(),
            owing: BTreeSet::new(),
            param_fifos: BTreeSet::new(),
            param_feedback: BTreeSet::new(),
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
        let cx = Cx {
            plan: &plan,
            old: input.old,
            carry: input.carry,
            resumed: input.resumed,
            max_block: plan.prepare.max_block().get(),
            cap: self.event_cap,
        };
        self.track(c, &cx);
        let closers = self.closers(c, &cx);
        let to_close: BTreeSet<EventDelayKey> = closers
            .iter()
            .filter_map(|k| match k.route {
                Route::Delay(i) => Some(plan.program.event_delays[i].key),
                _ => None,
            })
            .collect();
        let delays = self.delays(&cx, &to_close);
        let (audio_fb, audio_fb_from, efb, efb_fifo, efb_caps, efb_params) = self.feedback(&cx);
        let outlets = outlets(c, &cx);
        let (lanes, lanes_from) = lanes(c, &cx);
        let drives = drives(c, &cx, input.mix_lane);
        let mut flush = self.flush(c, &cx, closers);
        let sizes = Sizes::of(&cx, &delays.events, &efb_fifo);
        let (inject, shells) = self.shells(&cx, &flush, sizes.widest);
        let views = view_rooms(&plan, &inject, sizes.widest);
        let slot_capacity = |i: EventSlotId| {
            (plan.program.sizing.events[i].events(cx.cap) + sizes.extra[i]).max(views[i].0)
                + views[i].1
        };
        let param_slots = param_slots(&plan);
        let store_len = self.store.length_for(c.delta.store_len as usize);
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
            audio_fb,
            event_fb: efb,
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
        self.prepare_incoming(c, &cx);
        let tables = flush
            .rooms
            .values()
            .map(|r| r.closers + r.carried)
            .sum::<usize>();
        let spare_drives = lanes_from.iter().flatten().count() + drives.len();
        let moved_audio = delays
            .audio_from
            .iter()
            .filter(|r| matches!(r, AudioCarry::Move { .. }))
            .count();
        let moved_events = delays.events.from.iter().filter(is_move).count()
            + efb_fifo.from.iter().filter(is_move).count();
        reserve_returns(c, &cx);
        let wanted: usize = plan
            .units
            .iter()
            .filter_map(|u| u.shape.chase)
            .map(|n| n.get() as usize)
            .sum();
        c.staged = Some(Box::new(Staged {
            carry: cx.carry,
            state: Some(state),
            audio_delays: delays.audio_from,
            event_delays: delays.events.from,
            event_fb: efb_fifo.from,
            audio_fb: audio_fb_from,
            taps: outlets.taps_from,
            event_taps: outlets.event_taps_from,
            meters: outlets.meters_from,
            lanes: lanes_from,
            gone_delays: std::mem::take(&mut flush.gone_delays),
            gone_feedback: std::mem::take(&mut flush.gone_feedback),
            closers: std::mem::take(&mut flush.closers),
            queued: std::mem::take(&mut flush.queued),
            spare_drives: CappedVec::with_capacity(spare_drives),
            drives,
            spare_audio: CappedVec::with_capacity(moved_audio),
            spare_events: CappedVec::with_capacity(moved_events),
            spare_tables: CappedVec::with_capacity(tables),
            chase: CappedVec::with_capacity(wanted),
            store,
            store_len,
        }));
        // The bounds the next commit builds from.
        self.fifos = delays.caps;
        self.owing = delays.owing;
        self.param_fifos = delays.with_params;
        self.param_feedback = efb_params;
        self.feedback = efb_caps;
        self.shells.replace(shells, c.seq);
    }

    /// Crossfades in flight, and how long a chase can be, by key.
    fn track<F: Float>(&mut self, c: &Commit<F>, cx: &Cx<'_>) {
        for (key, f) in &c.delta.fades {
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

    /// PDC rings and FIFOs, by key: the shell of each, how it takes over the
    /// old one, and the capacity of each event FIFO for the next commit.
    fn delays<F: Float>(&self, cx: &Cx<'_>, to_close: &BTreeSet<EventDelayKey>) -> Delays<F> {
        let plan = cx.plan;
        let old_audio = cx
            .old
            .map(|p| keyed(&p.program.audio_delays, |d| d.key))
            .unwrap_or_default();
        let old_events = cx
            .old
            .map(|p| keyed(&p.program.event_delays, |d| d.key))
            .unwrap_or_default();
        let old_by_source = cx
            .old
            .map(|p| ring_by_source(&p.program.audio_delays))
            .unwrap_or_default();
        let mut audio = Vec::with_capacity(plan.program.audio_delays.len());
        let mut audio_from = Vec::with_capacity(plan.program.audio_delays.len());
        for d in plan.program.audio_delays.iter() {
            let carried = old_audio.get(&d.key).filter(|_| cx.carry).copied();
            match carried {
                // An old main output's channel in a main output crossfade's bank starts
                // with the history the main output's own channel had.
                None => match copy_origin(plan, &d.key, &old_audio, &old_by_source)
                    .filter(|_| cx.carry)
                {
                    Some(from) => {
                        audio.push(Some(AudioRing::new(d.len)));
                        audio_from.push(AudioCarry::Copy { from });
                    }
                    None => {
                        audio.push(Some(AudioRing::new(d.len)));
                        audio_from.push(AudioCarry::New);
                    }
                },
                Some(from)
                    if cx
                        .old
                        .is_some_and(|p| p.program.audio_delays[from].len == d.len) =>
                {
                    audio.push(None);
                    audio_from.push(AudioCarry::Keep { from });
                }
                Some(from) => {
                    audio.push(Some(AudioRing::new(d.len)));
                    audio_from.push(AudioCarry::Move { from });
                }
            }
        }
        let mut caps = BTreeMap::new();
        let mut owing = BTreeSet::new();
        let mut with_params = BTreeSet::new();
        let mut event_fifos = Vec::with_capacity(plan.program.event_delays.len());
        let mut from = Vec::with_capacity(plan.program.event_delays.len());
        let mut extra = vec![0; plan.program.event_delays.len()];
        for (i, d) in plan.program.event_delays.iter().enumerate() {
            let carried = old_events.get(&d.key).filter(|_| cx.carry).copied();
            let src = d.key.origin();
            let rate = plan.event_tap_rate(src, d.len, cx.max_block, cx.cap);
            let want = EventFifo::bound(d.len.get(), rate, cx.max_block);
            let have = carried.map(|j| (j, self.fifo_capacity(&d.key)));
            let (mut how, mut cap, over) = fit_fifo(have, want, rate);
            // A FIFO that owes closes has a table for them: one that
            // has none yet is built afresh and takes the queue over.
            let has_table = carried.is_some() && self.owing.contains(&d.key);
            let table = has_table || to_close.contains(&d.key);
            if let (EventCarry::Keep { from: j, .. }, true, false) = (how, table, has_table) {
                how = EventCarry::Move { from: j };
                cap = cap.max(want);
            }
            // A FIFO for a source that writes param events has room
            // for them: one kept from before the source did is built
            // afresh and takes the queue over.
            let params = plan.event_tap_params(src);
            let had_params = carried.is_some() && self.param_fifos.contains(&d.key);
            if let (EventCarry::Keep { from: j, .. }, true, false) = (how, params, had_params) {
                how = EventCarry::Move { from: j };
                cap = cap.max(want);
            }
            event_fifos.push((!matches!(how, EventCarry::Keep { .. })).then(|| {
                let f = EventFifo::sized_for(d.len, rate, cx.max_block, cap);
                let f = if params { f.with_params() } else { f };
                if table { f.with_owed() } else { f }
            }));
            if table {
                owing.insert(d.key);
            }
            if params {
                with_params.insert(d.key);
            }
            from.push(how);
            extra[i] = over;
            caps.insert(d.key, cap);
        }
        Delays {
            audio,
            audio_from,
            event_fifos,
            events: FifoPlan { from, extra },
            caps,
            owing,
            with_params,
        }
    }

    /// The capacity the executor's FIFO for `key` has, as of the last commit.
    /// Every carried key was in that commit's plan, so it has an entry; a
    /// missing one counts as nothing, and what the new FIFO cannot take is
    /// counted lost (`EventFifo::transfer_from`).
    fn fifo_capacity(&self, key: &EventDelayKey) -> usize {
        let have = self.fifos.get(key).copied();
        debug_assert!(have.is_some(), "a carried FIFO was in the last plan");
        have.unwrap_or(0)
    }

    /// Feedback delays, by key: exactly the edge's delay long.
    #[allow(clippy::type_complexity)]
    fn feedback<F: Float>(
        &self,
        cx: &Cx<'_>,
    ) -> (
        Vec<Option<AudioRing<F>>>,
        Vec<Option<u32>>,
        Vec<Option<EventFifo>>,
        FifoPlan<EventFeedbackId>,
        BTreeMap<EventFeedbackKey, usize>,
        BTreeSet<EventFeedbackKey>,
    ) {
        let plan = cx.plan;
        let old_afb = cx
            .old
            .map(|p| keyed(&p.program.audio_feedback, |f| f.key))
            .unwrap_or_default();
        let old_efb = cx
            .old
            .map(|p| keyed(&p.program.event_feedback, |f| f.key))
            .unwrap_or_default();
        let mut audio_from = Vec::with_capacity(plan.program.audio_feedback.len());
        let audio = plan
            .program
            .audio_feedback
            .iter()
            .map(|f| {
                let from = old_afb.get(&f.key).filter(|_| cx.carry).map(|j| j.get());
                audio_from.push(from);
                from.is_none().then(|| AudioRing::new(f.key.delay))
            })
            .collect();
        let mut caps = BTreeMap::new();
        let mut with_params = BTreeSet::new();
        let mut from = Vec::with_capacity(plan.program.event_feedback.len());
        let mut extra = vec![0; plan.program.event_feedback.len()];
        let event = plan
            .program
            .event_feedback
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let src = f.key.from;
                let rate =
                    plan.event_tap_rate(EventTapPort::Out(src), f.key.delay, cx.max_block, cx.cap);
                let want = EventFifo::bound(f.key.delay.get(), rate, cx.max_block);
                let carried = old_efb.get(&f.key).filter(|_| cx.carry).map(|&j| {
                    let have = self.feedback.get(&f.key).copied();
                    debug_assert!(have.is_some(), "a carried FIFO was in the last plan");
                    (j, have.unwrap_or(0))
                });
                let (mut how, mut cap, over) = fit_fifo(carried, want, rate);
                // As a delay's FIFO: one kept from before its source wrote
                // param events is built afresh.
                let params = plan.event_tap_params(EventTapPort::Out(src));
                let had_params = carried.is_some() && self.param_feedback.contains(&f.key);
                if let (EventCarry::Keep { from: j, .. }, true, false) = (how, params, had_params) {
                    how = EventCarry::Move { from: j };
                    cap = cap.max(want);
                }
                from.push(how);
                extra[i] = over;
                caps.insert(f.key, cap);
                if params {
                    with_params.insert(f.key);
                }
                (!matches!(how, EventCarry::Keep { .. })).then(|| {
                    let fifo = EventFifo::sized_for(f.key.delay, rate, cx.max_block, cap);
                    if params { fifo.with_params() } else { fifo }
                })
            })
            .collect();
        (
            audio,
            audio_from,
            event,
            FifoPlan { from, extra },
            caps,
            with_params,
        )
    }

    /// What disappears flushes to sinks that survive, and held keys of an
    /// edge that ended are closed at the sink that lost it: each sink gets a
    /// buffer for everything that can flow into it.
    fn flush<F: Float>(&self, c: &Commit<F>, cx: &Cx<'_>, closers: Vec<Closer>) -> Flush {
        let plan = cx.plan;
        let mut rooms: BTreeMap<(PlanUnitId, u16), Room> = BTreeMap::new();
        // What replaced nodes wrote for their successors.
        for (old_p, new_p) in &c.delta.replace {
            let chase = cx
                .old
                .and_then(|o| o.unit(old_p.key))
                .and_then(|u| u.shape.chase);
            let sink = cx.sink_of(EventInputPort::new(new_p.key, 0));
            if let (Some(n), Some(u)) = (chase, sink) {
                rooms.entry((u, 0)).or_default().flush += n.get() as usize;
            }
        }
        // Injections not yet delivered: each buffer counts once, with what
        // flushed into it before, and its room for a fade's end.
        for s in &self.shells.list {
            let at = EventInputPort::new(s.key, s.port);
            if let Some(u) = cx.sink_of(at) {
                let r = rooms.entry((u, s.port)).or_default();
                r.flush += s.flush;
                r.room = r.room.max(s.room);
                r.carried += s.owed;
            }
        }
        // Event delays and feedback that no longer exist, in key order.
        let old_delays = cx
            .old
            .map(|p| keyed(&p.program.event_delays, |d| d.key))
            .unwrap_or_default();
        let plan_delay_keys: BTreeSet<EventDelayKey> =
            plan.program.event_delays.iter().map(|d| d.key).collect();
        let gone_delays = old_delays
            .iter()
            .filter(|(k, _)| !(cx.carry && plan_delay_keys.contains(k)))
            .filter_map(|(k, &j)| match k {
                EventDelayKey::Edge { at, .. } => {
                    if let Some(u) = cx.sink_of(*at) {
                        let r = rooms.entry((u, at.port)).or_default();
                        r.flush += self.fifos.get(k).copied().unwrap_or(0);
                        // What the FIFO still owes moves to the sink.
                        r.carried += usize::from(self.owing.contains(k));
                    }
                    Some(Vanished { from: j, at: *at })
                }
                _ => None,
            })
            .collect();
        let old_efb = cx
            .old
            .map(|p| keyed(&p.program.event_feedback, |f| f.key))
            .unwrap_or_default();
        let plan_efb: BTreeSet<EventFeedbackKey> =
            plan.program.event_feedback.iter().map(|f| f.key).collect();
        let gone_feedback = old_efb
            .iter()
            .filter(|(k, _)| !(cx.carry && plan_efb.contains(k)))
            .map(|(k, &j)| {
                if let Some(u) = cx.sink_of(k.at) {
                    rooms.entry((u, k.at.port)).or_default().flush +=
                        self.feedback.get(k).copied().unwrap_or(0);
                }
                Vanished { from: j, at: k.at }
            })
            .collect();
        for k in &closers {
            if let (Route::Sink(_), Some(u)) = (&k.route, cx.sink_of(k.at)) {
                rooms.entry((u, k.at.port)).or_default().closers += 1;
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
            gone_delays,
            gone_feedback,
            closers,
            queued,
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
        // or cuts, and, after a re-prepare, every unit that does not come
        // back.
        let mut maybe_gone: BTreeSet<NodeKey> = c
            .delta
            .retire
            .iter()
            .chain(c.delta.replace.iter().map(|(o, _)| o))
            .chain(&c.delta.cuts)
            .map(|p| p.key)
            .collect();
        if cx.resumed {
            maybe_gone.extend(
                old.units
                    .iter()
                    .filter(|u| {
                        plan.unit(u.key)
                            .is_none_or(|n| n.generation != u.generation)
                    })
                    .map(|u| u.key),
            );
        }
        let replaced: BTreeSet<NodeKey> = c.delta.replace.iter().map(|(_, n)| n.key).collect();
        let mut closers = Vec::new();
        for e in &old.event_edges {
            let &(from, at) = e;
            let in_plan = plan.event_edges.binary_search(e).is_ok();
            if (in_plan && !maybe_gone.contains(&from.node)) || replaced.contains(&at.node) {
                continue;
            }
            if cx.sink_of(at).is_none() || !old.unit(from.node).is_some_and(|f| f.shape.note_events)
            {
                continue;
            }
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
            closers.push(Closer { from, at, route });
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
    /// crossfade's chase.
    fn prepare_incoming<F: Float>(&self, c: &mut Commit<F>, cx: &Cx<'_>) {
        for inc in &mut c.incoming {
            let Some(shape) = cx.plan.unit(inc.key).map(|u| u.shape) else {
                continue;
            };
            let ports = shape.event_out as usize;
            if ports > 0 && !cx.parked_back(inc.key) {
                inc.keys.size(ports, shape.note_events, false);
            }
            if ports > 0 && inc.fade.is_some() {
                inc.staging = Keys::staging(ports, shape.note_events);
            }
            if let Some(f) = &mut inc.fade {
                let n = cx
                    .old
                    .and_then(|o| o.unit(inc.key))
                    .and_then(|u| u.shape.chase)
                    .map_or(0, |n| n.get() as usize);
                f.chase = CappedVec::with_capacity(n);
            }
        }
    }
}

/// The sinks a commit can flush to, and what each is flushed from.
struct Flush {
    rooms: BTreeMap<(PlanUnitId, u16), Room>,
    gone_delays: Vec<Vanished<EventDelayId>>,
    gone_feedback: Vec<Vanished<EventFeedbackId>>,
    closers: Vec<Closer>,
    queued: Vec<PlanUnitId>,
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
        let ports = plan.nodes.recs[unit].ports(&plan.nodes);
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
/// last commit (which wins over a carried one of the same id) or `None`,
/// with the old index a carried one comes from.
struct Outlets<F: Float> {
    taps: Vec<Option<TapWriter<F>>>,
    taps_from: Vec<Option<u32>>,
    event_taps: Vec<Option<EventTapWriter>>,
    event_taps_from: Vec<Option<u32>>,
    meters: Vec<Option<MeterWriter>>,
    meters_from: Vec<Option<u32>>,
}

/// Taps, event taps and meters, by id. A writer the plan does not name stays
/// in the old state (or the commit) and is freed on the control side.
fn outlets<F: Float>(c: &mut Commit<F>, cx: &Cx<'_>) -> Outlets<F> {
    let plan = cx.plan;
    fn open<I: PartialEq, W>(opened: &mut Vec<(I, W)>, id: &I) -> Option<W> {
        let at = opened.iter().position(|(o, _)| o == id)?;
        Some(opened.swap_remove(at).1)
    }
    fn index<I: Ord + Copy>(ids: impl Iterator<Item = I>) -> BTreeMap<I, usize> {
        ids.enumerate().map(|(i, k)| (k, i)).collect()
    }
    let old_taps = cx
        .old
        .map(|p| index(p.program.taps.iter().copied()))
        .unwrap_or_default();
    let old_event_taps = cx
        .old
        .map(|p| index(p.program.event_taps.iter().copied()))
        .unwrap_or_default();
    let old_meters = cx
        .old
        .map(|p| index(p.program.meters.iter().map(|(k, _)| *k)))
        .unwrap_or_default();
    let carried =
        |opened: bool, from: Option<&usize>| (!opened).then(|| from.map(|&i| i as u32)).flatten();
    let mut out = Outlets {
        taps: Vec::with_capacity(plan.program.taps.len()),
        taps_from: Vec::with_capacity(plan.program.taps.len()),
        event_taps: Vec::with_capacity(plan.program.event_taps.len()),
        event_taps_from: Vec::with_capacity(plan.program.event_taps.len()),
        meters: Vec::with_capacity(plan.program.meters.len()),
        meters_from: Vec::with_capacity(plan.program.meters.len()),
    };
    for k in &plan.program.taps {
        let w = open(&mut c.opened.taps, k);
        out.taps_from.push(carried(w.is_some(), old_taps.get(k)));
        out.taps.push(w);
    }
    for k in &plan.program.event_taps {
        let w = open(&mut c.opened.event_taps, k);
        out.event_taps_from
            .push(carried(w.is_some(), old_event_taps.get(k)));
        out.event_taps.push(w);
    }
    for (k, lanes) in &plan.program.meters {
        let w = open(&mut c.opened.meters, k);
        if let Some(w) = &w {
            w.set_width(*lanes);
        }
        out.meters_from
            .push(carried(w.is_some(), old_meters.get(k)));
        out.meters.push(w);
    }
    out
}

/// Weighted lanes, by edge: what a lane holds carries across a commit that
/// keeps the edge (not a sample-rate change's reset: a weight is not time),
/// and a new edge starts at its value. A lane a source drives has its state
/// built here.
fn lanes<F: Float>(c: &Commit<F>, cx: &Cx<'_>) -> (Vec<SumLane<F>>, Vec<Option<u32>>) {
    let plan = cx.plan;
    let old_lanes: BTreeMap<crate::EdgeId, usize> = cx
        .old
        .map(|p| {
            p.program
                .sum_lanes
                .iter()
                .enumerate()
                .filter_map(|(i, id)| Some(((*id)?, i)))
                .collect()
        })
        .unwrap_or_default();
    let mut driven = vec![false; plan.program.sum_lanes.len()];
    for op in &plan.program.ops {
        if let Op::WeightedSum { lanes, gains, .. } = *op {
            for g in &plan.program.gain_ports[gains.range()] {
                driven[lanes.range().start + usize::from(g.lane)] = true;
            }
        }
    }
    let mut from = Vec::with_capacity(plan.program.sum_lanes.len());
    let lanes = plan
        .program
        .sum_lanes
        .iter()
        .zip(&driven)
        .map(|(id, &driven)| {
            from.push(id.and_then(|id| old_lanes.get(&id)).map(|&i| i as u32));
            match id {
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
            }
        })
        .collect();
    (lanes, from)
}

/// The lanes that drive a unit's mix: for a unit placed now, or one no
/// earlier plan drove.
fn drives<F: Float>(
    c: &Commit<F>,
    cx: &Cx<'_>,
    mix_lane: &dyn Fn(NodeKey) -> Option<ParamState<F>>,
) -> Vec<(UnitIdx, ParamState<F>)> {
    let driven_of = |p: &Plan, rec: &crate::compile::plan::NodeRec| {
        p.program.param_ports[rec.params.range()]
            .last()
            .is_some_and(ParamPortOp::is_mix)
    };
    let old_driven: BTreeSet<UnitIdx> = cx
        .old
        .map(|p| {
            p.nodes
                .recs
                .iter()
                .filter(|r| driven_of(p, r))
                .map(|r| r.store)
                .collect()
        })
        .unwrap_or_default();
    let placed: BTreeSet<NodeKey> = c.incoming.iter().map(|i| i.key).collect();
    cx.plan
        .nodes
        .recs
        .iter()
        .filter(|r| driven_of(cx.plan, r))
        .filter_map(|r| {
            let key = cx.plan.units[r.unit].key;
            (placed.contains(&key) || !old_driven.contains(&r.store))
                .then(|| mix_lane(key).map(|lane| (r.store, lane)))
                .flatten()
        })
        .collect()
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
                let rec = &plan.nodes.recs[unit];
                let ports = rec.ports(&plan.nodes);
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
                let ports = plan.nodes.recs[unit].ports(&plan.nodes);
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
    let replaces = c.delta.replace.len();
    let parked = if cx.resumed {
        cx.old.map_or(0, |o| o.units.len())
    } else {
        0
    };
    let held = c.delta.retire.len() + replaces + c.delta.cuts.len() + parked;
    c.gone = Gone {
        held: CappedVec::with_capacity(held),
        spare: CappedVec::with_capacity(2 * (c.delta.retire.len() + replaces + parked)),
        replaced: CappedVec::with_capacity(replaces),
        chased: CappedVec::with_capacity(replaces),
        held_refused: CappedVec::with_capacity(replaces),
        chase_events: CappedVec::default(),
    };
    c.spare_held = CappedVec::with_capacity(held + c.gone.spare.capacity() + 4 * c.incoming.len());
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

/// The longest ring that aligns each source to an output, whichever channel it
/// fed; of equal lengths, the lowest key. The choice is a function of the set
/// of rings, not of their order in the plan: what a source on a new channel
/// starts from, with none of its history zero-filled.
fn ring_by_source(
    delays: &TableSlice<AudioDelayId, AudioDelay>,
) -> BTreeMap<crate::PortSource, AudioDelayId> {
    let mut by_source: BTreeMap<crate::PortSource, AudioDelayId> = BTreeMap::new();
    for (i, d) in delays.iter_enumerated() {
        let AudioDelayKey::Output { from, .. } = d.key else {
            continue;
        };
        let better = |j: &AudioDelayId| {
            let best = &delays[*j];
            (d.len, std::cmp::Reverse(d.key)) > (best.len, std::cmp::Reverse(best.key))
        };
        if by_source.get(&from).is_none_or(better) {
            by_source.insert(from, i);
        }
    }
    by_source
}

/// The old ring an output channel's ring of this plan starts from when no old
/// ring has its own key: the old main output's channel `c` for the bank's channel
/// `base + c`, and, in a main output change, for a main output channel of a source the old
/// plan aligned on another channel, that channel's ring (a source's delayed
/// signal does not depend on the channel it feeds).
fn copy_origin(
    plan: &Plan,
    key: &AudioDelayKey,
    old: &BTreeMap<AudioDelayKey, AudioDelayId>,
    by_source: &BTreeMap<crate::PortSource, AudioDelayId>,
) -> Option<AudioDelayId> {
    let AudioDelayKey::Output { channel, from } = *key else {
        return None;
    };
    match plan.main_output_bank() {
        Some(bank) if usize::from(channel) >= bank.base => {
            let was = channel - u16::try_from(bank.base).ok()?;
            old.get(&AudioDelayKey::Output { channel: was, from })
                .copied()
        }
        // Only in a main output change: any other recompile keeps a ring by its key.
        Some(_) if !old.contains_key(key) => by_source.get(&from).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::{Table, id_at};
    use crate::{NodeKey, OutputPort, PortSource};
    use tutti_units::Samples;

    fn ring(channel: u16, from: PortSource, len: usize) -> AudioDelay {
        AudioDelay {
            key: AudioDelayKey::Output { channel, from },
            len: Samples::new(len),
        }
    }

    fn source(node: u64) -> PortSource {
        PortSource::Node(OutputPort::new(NodeKey(node), 0))
    }

    /// The longest ring of a source is the one a new channel copies, and of
    /// equal lengths the lowest key, whatever order the plan lists them in:
    /// two renders of one commit never copy different rings.
    #[test]
    fn the_longest_ring_of_a_source_wins_and_the_lowest_key_breaks_a_tie() {
        let s = source(1);
        let delays = [
            ring(3, s, 20),
            ring(2, s, 30),
            ring(1, s, 30),
            ring(0, s, 10),
            ring(0, source(2), 5),
        ];
        let by = ring_by_source(&Table::from(delays.to_vec()));
        assert_eq!(
            by[&s],
            id_at(2),
            "channel 1: of the two 30-frame rings, the lower key"
        );
        assert_eq!(
            by[&source(2)],
            id_at(4),
            "a source of its own has its own ring"
        );
        let mut order: Vec<usize> = (0..delays.len()).collect();
        for _ in 0..delays.len() {
            order.rotate_left(1);
            let shuffled: Table<AudioDelayId, AudioDelay> =
                order.iter().map(|&i| delays[i]).collect();
            let by = ring_by_source(&shuffled);
            assert_eq!(
                shuffled[by[&s]].key, delays[2].key,
                "the same ring in any listing order"
            );
        }
    }

    /// A ring that is not an output's is no source's origin.
    #[test]
    fn only_output_rings_are_origins() {
        let at = crate::InputPort::new(NodeKey(9), 0);
        let delays = [AudioDelay {
            key: AudioDelayKey::Edge {
                at,
                from: source(1),
            },
            len: Samples::new(40),
        }];
        assert!(ring_by_source(&Table::from(delays.to_vec())).is_empty());
    }
}
