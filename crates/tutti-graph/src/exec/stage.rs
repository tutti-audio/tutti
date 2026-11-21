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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tutti_buffers::Float;
use tutti_rt::CappedVec;

use tutti_curve::declick_frames;

use super::commit::Commit;
use super::node_op::{CONSTANT, SILENT};
use super::par_for;
use super::state::{Gone, Inject, Ring, State, Unit};
use crate::EventTapSource;
use crate::NodeKey;
use crate::arena::Arena;
use crate::command::{Overlay, overlay_capacity};
use crate::event::Event;
use crate::event_tap::EventTapWriter;
use crate::gain::SumLane;
use crate::held::{Held, Keys};
use crate::kernels::{AudioRing, EventFifo};
use crate::meter::MeterWriter;
use crate::par::Par;
use crate::param::ParamState;
use crate::plan::{DelayKey, FeedbackKey, Op, ParamPortOp, Plan};
use crate::slots::EventSlot;
use crate::spec::{EventIn, EventOut};
use crate::tap::TapWriter;

/// How a ring or FIFO of the new plan gets its state from the executor's old
/// state.
#[derive(Clone, Copy, Debug)]
pub(super) enum RingFrom {
    /// Nothing carries: the ring built for it starts empty.
    New,
    /// The old ring at this index moves in whole; a FIFO is retuned to the
    /// new length at `rate` (built with room for it, so nothing allocates).
    Keep { from: u32, rate: usize },
    /// The old ring at this index is copied into the one built for the new
    /// length, and goes back in the commit.
    Move { from: u32 },
    /// The old ring at this index is copied into the one built for it and
    /// stays where it is, for the ring that takes it over: an old root's
    /// channel, kept sounding while the new root's channel of the same source
    /// goes on with the same history.
    Copy { from: u32 },
}

/// An old event edge whose source's held keys are closed at its sink.
pub(super) struct Closer {
    pub(super) from: EventOut,
    pub(super) at: EventIn,
    pub(super) route: Route,
}

/// Where the close of a cut edge's held keys goes. Either way the keys go
/// into a table that holds every key and pedal there is, so it cannot
/// overflow and no note off is lost; the table is drained as room frees,
/// pedal up first (with several cut edges into one sink, each table does).
pub(super) enum Route {
    /// The edge goes on through the delay at this index of the plan: its FIFO
    /// was built with a table for owed closes, poured into the queue block by
    /// block, behind what is in flight.
    Delay(u32),
    /// The edge ended: the table the source's held and owed keys merge into,
    /// drained into the sink's injection buffer as far as it takes now and
    /// block by block after.
    Sink(Held),
    /// The table has been placed.
    Placed,
}

/// A delay or feedback FIFO that goes away: its queue flushes into `at`.
pub(super) struct Vanished {
    pub(super) from: u32,
    pub(super) at: EventIn,
}

/// Everything a commit's apply installs, built on the control side.
pub(super) struct Staged<F: Float = f32> {
    /// Whether time-based state carries (no sample-rate change).
    pub(super) carry: bool,
    /// The pool participants the parallel state was built for (0: none).
    pub(super) participants: usize,
    /// The new state, shell: every ring and writer that comes from the
    /// commit is in place, the rest waits for what carries.
    pub(super) state: Option<State<F>>,
    pub(super) rings: Vec<RingFrom>,
    pub(super) event_fb: Vec<RingFrom>,
    /// The old index each audio feedback ring, tap, event tap, meter and
    /// lane of the new plan takes over, if any.
    pub(super) audio_fb: Vec<Option<u32>>,
    pub(super) taps: Vec<Option<u32>>,
    pub(super) event_taps: Vec<Option<u32>>,
    pub(super) meters: Vec<Option<u32>>,
    pub(super) lanes: Vec<Option<u32>>,
    /// Old event delays and event feedback that no longer exist: their
    /// queues flush, in key order.
    pub(super) gone_delays: Vec<Vanished>,
    pub(super) gone_feedback: Vec<Vanished>,
    /// Old event edges whose held keys may need closing at the sink.
    pub(super) closers: Vec<Closer>,
    /// Units (plan indices) that may hold a crossfade queued behind a
    /// running one.
    pub(super) queued: Vec<u32>,
    /// The store index and the lane state of each unit whose mix a plan
    /// drives and that may have none.
    pub(super) drives: Vec<(u32, ParamState<F>)>,
    pub(super) spare_rings: CappedVec<Ring<F>>,
    /// Tables of owed closes merged away, to be freed on the control side.
    pub(super) spare_tables: CappedVec<Held>,
    pub(super) spare_drives: CappedVec<ParamState<F>>,
    /// The parallel state a pool set since this was staged left unused.
    pub(super) spare_par: Option<Par>,
    /// The executor's chase buffer from this plan on.
    pub(super) chase: Vec<Event>,
    /// A store with room for the plan's units, when the executor's has none.
    pub(super) store: Option<Vec<Option<Unit<F>>>>,
    /// The store length the plan needs.
    store_len: usize,
}

impl<F: Float> Staged<F> {
    /// The parallel state for `participants` (the pool the executor has now),
    /// built for the plan this was staged for: the pool was set after the
    /// commit was sent. Control side: allocates.
    pub(super) fn set_participants(&mut self, plan: &Plan, participants: Option<usize>) {
        self.participants = participants.unwrap_or(0);
        if let Some(state) = &mut self.state {
            state.par = par_for(
                plan,
                participants,
                self.store_len,
                state.inject.iter().map(|i| i.unit),
                state.overlay.capacity(),
            );
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
    declick: usize,
}

impl Cx<'_> {
    /// The plan unit index of the input `at`, when the plan has it.
    fn sink_of(&self, at: EventIn) -> Option<u32> {
        let u = self
            .plan
            .units
            .binary_search_by_key(&at.node, |u| u.key)
            .ok()?;
        (at.port < self.plan.units[u].shape.event_in).then_some(u as u32)
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
    pool: Arc<AtomicUsize>,
    /// Blocks the executor has rendered (`Executor::render_applied`).
    rendered: Arc<AtomicU64>,
    /// The capacity of each event delay's FIFO, by key.
    fifos: BTreeMap<DelayKey, usize>,
    /// The same for event feedback.
    feedback: BTreeMap<FeedbackKey, usize>,
    /// The event delays whose FIFO holds a table for owed closes.
    owing: BTreeSet<DelayKey>,
    /// The event delays whose FIFO has room for param events, and the same
    /// for event feedback.
    param_fifos: BTreeSet<DelayKey>,
    param_feedback: BTreeSet<FeedbackKey>,
    /// The injection buffers of the last commit, with the commit that made
    /// them, once it was seen applied, and the block count then.
    shells: Vec<Shell>,
    shells_seq: u64,
    shells_seen: Option<u64>,
    /// The longest a replaced node's chase can be, by key.
    chase: BTreeMap<NodeKey, usize>,
    /// Crossfades sent and not yet returned, by key.
    fading: BTreeMap<NodeKey, u32>,
    /// The executor's store: its length, and its capacity.
    store_len: usize,
    store_cap: usize,
}

/// The ring and FIFO plan of one delay or feedback kind.
struct FifoPlan {
    from: Vec<RingFrom>,
    /// The most each FIFO's queue can hold past its new bound.
    extra: Vec<usize>,
}

/// Which way a FIFO of bound `want` takes over an old one of capacity
/// `have`, and the capacity it has afterwards.
fn fit_fifo(from: Option<(u32, usize)>, want: usize, rate: usize) -> (RingFrom, usize, usize) {
    match from {
        // Room enough: the old FIFO is retuned in place, and keeps its
        // capacity; what it holds past the new bound is its excess.
        Some((j, have)) if have >= want => (RingFrom::Keep { from: j, rate }, have, have - want),
        Some((j, _)) => (RingFrom::Move { from: j }, want, 0),
        None => (RingFrom::New, want, 0),
    }
}

impl Stager {
    pub(crate) fn new(event_cap: usize, pool: Arc<AtomicUsize>, rendered: Arc<AtomicU64>) -> Self {
        Self {
            event_cap,
            pool,
            rendered,
            fifos: BTreeMap::new(),
            feedback: BTreeMap::new(),
            owing: BTreeSet::new(),
            param_fifos: BTreeSet::new(),
            param_feedback: BTreeSet::new(),
            shells: Vec::new(),
            shells_seq: 0,
            shells_seen: None,
            chase: BTreeMap::new(),
            fading: BTreeMap::new(),
            store_len: 0,
            store_cap: 0,
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
        if self.shells.is_empty() || acked < self.shells_seq {
            return;
        }
        let now = self.rendered.load(Ordering::Acquire);
        match self.shells_seen {
            None => self.shells_seen = Some(now),
            Some(seen) if now >= seen + 2 => {
                self.shells.clear();
                self.shells_seen = None;
            }
            Some(_) => {}
        }
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
            declick: declick_frames(plan.prepare.sample_rate()).get(),
        };
        self.track(c, &cx);
        let closers = self.closers(c, &cx);
        let to_close: BTreeSet<DelayKey> = closers
            .iter()
            .filter_map(|k| match k.route {
                Route::Delay(i) => Some(plan.delays[i as usize].key),
                _ => None,
            })
            .collect();
        let (rings, ring_fifo, ring_caps, owing, ring_params) = self.delays(&cx, &to_close);
        let (audio_fb, audio_fb_from, efb, efb_fifo, efb_caps, efb_params) = self.feedback(&cx);
        let outlets = outlets(c, &cx);
        let (lanes, lanes_from) = lanes(c, &cx);
        let drives = drives(c, &cx, input.mix_lane);
        let mut flush = self.flush(c, &cx, closers);
        let sizes = Sizes::of(&cx, &ring_fifo, &efb_fifo, &flush);
        let (inject, shells) = self.shells(&cx, &flush, sizes.widest);
        let slot_capacity = |i: usize| plan.event_slot_capacity[i].events(cx.cap) + sizes.extra[i];
        let param_slots = param_slots(&plan);
        let overlay = overlay_capacity(&plan, cx.cap, sizes.flowing());
        let store_len = self.store_len.max(c.delta.store_len as usize);
        let participants = self.pool.load(Ordering::Acquire);
        let par = par_for(
            &plan,
            (participants > 0).then_some(participants),
            store_len,
            inject.iter().map(|i| i.unit),
            overlay,
        );
        let state = State {
            arena: Arena::new(plan.audio_slots as usize, cx.max_block),
            flags: {
                let mut v = vec![0u8; plan.audio_slots as usize];
                v[0] = SILENT | CONSTANT;
                v
            },
            events: (0..plan.event_slot_capacity.len())
                .map(|i| {
                    let params = if param_slots[i] { slot_capacity(i) } else { 0 };
                    EventSlot::with_capacity(slot_capacity(i), params)
                })
                .collect(),
            rings,
            audio_fb,
            event_fb: efb,
            taps: outlets.taps,
            event_taps: outlets.event_taps,
            meters: outlets.meters,
            lanes,
            inject,
            has_inject: vec![false; plan.units.len()],
            has_due: vec![false; plan.units.len()],
            overlay: Overlay::with_capacity(overlay),
            par,
        };
        // The store grows by moving into one built here.
        let store = (store_len > self.store_cap).then(|| {
            let cap = store_len.max(self.store_cap * 2);
            self.store_cap = cap;
            Vec::with_capacity(cap)
        });
        self.store_len = store_len;
        self.prepare_incoming(c, &cx);
        let tables = flush
            .rooms
            .values()
            .map(|r| r.closers + r.carried)
            .sum::<usize>();
        let spare_drives = lanes_from.iter().flatten().count() + drives.len();
        let moved = ring_fifo
            .from
            .iter()
            .chain(&efb_fifo.from)
            .filter(|r| matches!(r, RingFrom::Move { .. }))
            .count();
        reserve_returns(c, &cx);
        let wanted: usize = plan
            .units
            .iter()
            .filter_map(|u| u.shape.chase)
            .map(|n| n.get() as usize)
            .sum();
        c.staged = Some(Box::new(Staged {
            carry: cx.carry,
            participants,
            state: Some(state),
            rings: ring_fifo.from,
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
            spare_rings: CappedVec::with_capacity(moved),
            spare_tables: CappedVec::with_capacity(tables),
            spare_par: None,
            chase: Vec::with_capacity(wanted),
            store,
            store_len,
        }));
        // The bounds the next commit builds from.
        self.fifos = ring_caps;
        self.owing = owing;
        self.param_fifos = ring_params;
        self.param_feedback = efb_params;
        self.feedback = efb_caps;
        self.shells = shells;
        self.shells_seq = c.seq;
        self.shells_seen = None;
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
    fn delays<F: Float>(
        &self,
        cx: &Cx<'_>,
        to_close: &BTreeSet<DelayKey>,
    ) -> (
        Vec<Option<Ring<F>>>,
        FifoPlan,
        BTreeMap<DelayKey, usize>,
        BTreeSet<DelayKey>,
        BTreeSet<DelayKey>,
    ) {
        let plan = cx.plan;
        let old_delays: BTreeMap<DelayKey, usize> = cx
            .old
            .map(|p| {
                p.delays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (d.key, i))
                    .collect()
            })
            .unwrap_or_default();
        let old_by_source = ring_by_source(cx.old.map(|p| p.delays.as_slice()).unwrap_or_default());
        let mut caps = BTreeMap::new();
        let mut owing = BTreeSet::new();
        let mut with_params = BTreeSet::new();
        let mut rings = Vec::with_capacity(plan.delays.len());
        let mut from = Vec::with_capacity(plan.delays.len());
        let mut extra = vec![0; plan.delays.len()];
        for (i, d) in plan.delays.iter().enumerate() {
            let carried = old_delays.get(&d.key).filter(|_| cx.carry).copied();
            match d.key.event_origin() {
                Some(src) => {
                    let rate = plan.event_tap_rate(src, cx.cap);
                    let want = EventFifo::bound(d.len.get(), rate, cx.max_block);
                    let have = carried.map(|j| (j as u32, self.fifo_capacity(&d.key)));
                    let (mut how, mut cap, over) = fit_fifo(have, want, rate);
                    // A FIFO that owes closes has a table for them: one that
                    // has none yet is built afresh and takes the queue over.
                    let has_table = carried.is_some() && self.owing.contains(&d.key);
                    let table = has_table || to_close.contains(&d.key);
                    if let (RingFrom::Keep { from: j, .. }, true, false) = (how, table, has_table) {
                        how = RingFrom::Move { from: j };
                        cap = cap.max(want);
                    }
                    // A FIFO for a source that writes param events has room
                    // for them: one kept from before the source did is built
                    // afresh and takes the queue over.
                    let params = plan.event_tap_params(src);
                    let had_params = carried.is_some() && self.param_fifos.contains(&d.key);
                    if let (RingFrom::Keep { from: j, .. }, true, false) = (how, params, had_params)
                    {
                        how = RingFrom::Move { from: j };
                        cap = cap.max(want);
                    }
                    rings.push((!matches!(how, RingFrom::Keep { .. })).then(|| {
                        let f = EventFifo::sized_for(d.len, rate, cx.max_block, cap);
                        let f = if params { f.with_params() } else { f };
                        Ring::Event(if table { f.with_owed() } else { f })
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
                None => match carried {
                    // An old root's channel in a root crossfade's bank starts
                    // with the history the root's own channel had.
                    None if copy_origin(plan, &d.key, &old_delays, &old_by_source)
                        .filter(|_| cx.carry)
                        .is_some() =>
                    {
                        let j = copy_origin(plan, &d.key, &old_delays, &old_by_source);
                        rings.push(Some(Ring::Audio(AudioRing::new(d.len))));
                        from.push(RingFrom::Copy {
                            from: j.map_or(0, |j| j as u32),
                        });
                    }
                    Some(j) if cx.old.is_some_and(|p| p.delays[j].len == d.len) => {
                        rings.push(None);
                        from.push(RingFrom::Keep {
                            from: j as u32,
                            rate: 0,
                        });
                    }
                    carried => {
                        rings.push(Some(Ring::Audio(AudioRing::new(d.len))));
                        from.push(
                            carried.map_or(RingFrom::New, |j| RingFrom::Move { from: j as u32 }),
                        );
                    }
                },
            }
        }
        (rings, FifoPlan { from, extra }, caps, owing, with_params)
    }

    /// The capacity the executor's FIFO for `key` has, as of the last commit.
    /// Every carried key was in that commit's plan, so it has an entry; a
    /// missing one counts as nothing, and what the new FIFO cannot take is
    /// counted lost (`EventFifo::transfer_from`).
    fn fifo_capacity(&self, key: &DelayKey) -> usize {
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
        FifoPlan,
        BTreeMap<FeedbackKey, usize>,
        BTreeSet<FeedbackKey>,
    ) {
        let plan = cx.plan;
        let keyed = |kind: &[crate::plan::FeedbackSpec]| -> BTreeMap<FeedbackKey, usize> {
            kind.iter().enumerate().map(|(i, f)| (f.key, i)).collect()
        };
        let old_afb = cx.old.map(|p| keyed(&p.audio_feedback)).unwrap_or_default();
        let old_efb = cx.old.map(|p| keyed(&p.event_feedback)).unwrap_or_default();
        let mut audio_from = Vec::with_capacity(plan.audio_feedback.len());
        let audio = plan
            .audio_feedback
            .iter()
            .map(|f| {
                let from = old_afb.get(&f.key).filter(|_| cx.carry).map(|&j| j as u32);
                audio_from.push(from);
                from.is_none().then(|| AudioRing::new(f.key.delay()))
            })
            .collect();
        let mut caps = BTreeMap::new();
        let mut with_params = BTreeSet::new();
        let mut from = Vec::with_capacity(plan.event_feedback.len());
        let mut extra = vec![0; plan.event_feedback.len()];
        let event = plan
            .event_feedback
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let FeedbackKey::Event { from: src, .. } = f.key else {
                    unreachable!("event feedback keys are events")
                };
                let rate = plan.event_tap_rate(EventTapSource::Out(src), cx.cap);
                let want = EventFifo::bound(f.key.delay().get(), rate, cx.max_block);
                let carried = old_efb.get(&f.key).filter(|_| cx.carry).map(|&j| {
                    let have = self.feedback.get(&f.key).copied();
                    debug_assert!(have.is_some(), "a carried FIFO was in the last plan");
                    (j as u32, have.unwrap_or(0))
                });
                let (mut how, mut cap, over) = fit_fifo(carried, want, rate);
                // As a delay's FIFO: one kept from before its source wrote
                // param events is built afresh.
                let params = plan.event_tap_params(EventTapSource::Out(src));
                let had_params = carried.is_some() && self.param_feedback.contains(&f.key);
                if let (RingFrom::Keep { from: j, .. }, true, false) = (how, params, had_params) {
                    how = RingFrom::Move { from: j };
                    cap = cap.max(want);
                }
                from.push(how);
                extra[i] = over;
                caps.insert(f.key, cap);
                if params {
                    with_params.insert(f.key);
                }
                (!matches!(how, RingFrom::Keep { .. })).then(|| {
                    let fifo = EventFifo::sized_for(f.key.delay(), rate, cx.max_block, cap);
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
        let mut rooms: BTreeMap<(u32, u16), Room> = BTreeMap::new();
        // What replaced nodes wrote for their successors.
        for (old_p, new_p) in &c.delta.replace {
            let chase = cx
                .old
                .and_then(|o| o.unit(old_p.key))
                .and_then(|u| u.shape.chase);
            let sink = cx.sink_of(EventIn {
                node: new_p.key,
                port: 0,
            });
            if let (Some(n), Some(u)) = (chase, sink) {
                rooms.entry((u, 0)).or_default().flush += n.get() as usize;
            }
        }
        // Injections not yet delivered: each buffer counts once, with what
        // flushed into it before, and its room for a fade's end.
        for s in &self.shells {
            let at = EventIn {
                node: s.key,
                port: s.port,
            };
            if let Some(u) = cx.sink_of(at) {
                let r = rooms.entry((u, s.port)).or_default();
                r.flush += s.flush;
                r.room = r.room.max(s.room);
                r.carried += s.owed;
            }
        }
        // Event delays and feedback that no longer exist, in key order.
        let old_delays: BTreeMap<DelayKey, usize> = cx
            .old
            .map(|p| {
                p.delays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (d.key, i))
                    .collect()
            })
            .unwrap_or_default();
        let plan_delay_keys: BTreeSet<DelayKey> = plan.delays.iter().map(|d| d.key).collect();
        let gone_delays = old_delays
            .iter()
            .filter(|(k, _)| !(cx.carry && plan_delay_keys.contains(k)))
            .filter_map(|(k, &j)| match k {
                DelayKey::Event { at, .. } => {
                    if let Some(u) = cx.sink_of(*at) {
                        let r = rooms.entry((u, at.port)).or_default();
                        r.flush += self.fifos.get(k).copied().unwrap_or(0);
                        // What the FIFO still owes moves to the sink.
                        r.carried += usize::from(self.owing.contains(k));
                    }
                    Some(Vanished {
                        from: j as u32,
                        at: *at,
                    })
                }
                _ => None,
            })
            .collect();
        let old_efb: BTreeMap<FeedbackKey, usize> = cx
            .old
            .map(|p| {
                p.event_feedback
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (f.key, i))
                    .collect()
            })
            .unwrap_or_default();
        let plan_efb: BTreeSet<FeedbackKey> = plan.event_feedback.iter().map(|f| f.key).collect();
        let gone_feedback = old_efb
            .iter()
            .filter(|(k, _)| !(cx.carry && plan_efb.contains(k)))
            .filter_map(|(k, &j)| {
                let FeedbackKey::Event { at, .. } = k else {
                    return None;
                };
                if let Some(u) = cx.sink_of(*at) {
                    rooms.entry((u, at.port)).or_default().flush +=
                        self.feedback.get(k).copied().unwrap_or(0);
                }
                Some(Vanished {
                    from: j as u32,
                    at: *at,
                })
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
        for (u, unit) in plan.units.iter().enumerate() {
            let Some(&n) = self.chase.get(&unit.key) else {
                continue;
            };
            if !self.fading.contains_key(&unit.key) {
                continue;
            }
            queued.push(u as u32);
            if unit.shape.event_in > 0 {
                let r = rooms.entry((u as u32, 0)).or_default();
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
                .delays
                .iter()
                .position(|d| d.key == DelayKey::Event { at, from });
            let route = match delay {
                Some(i) if in_plan => Route::Delay(i as u32),
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
                    key: cx.plan.units[unit as usize].key,
                    port,
                    flush: r.flush + drain,
                    room: r.room,
                    owed,
                });
                Inject {
                    unit,
                    port,
                    events: Vec::with_capacity(events),
                    merged: Vec::with_capacity(events + widest),
                    live: false,
                    closed_from: 0,
                    owed: Vec::with_capacity(owed),
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
    rooms: BTreeMap<(u32, u16), Room>,
    gone_delays: Vec<Vanished>,
    gone_feedback: Vec<Vanished>,
    closers: Vec<Closer>,
    queued: Vec<u32>,
}

/// The sizes a plan's slots and buffers are built to.
struct Sizes {
    /// Per event slot, what a FIFO carried past its new bound can add.
    extra: Vec<usize>,
    /// The widest event slot.
    widest: usize,
    extra_total: usize,
    flushed_total: usize,
    held_total: usize,
    /// Room for closes drained a block at a time, over every sink owed some.
    drained_total: usize,
}

impl Sizes {
    fn of(cx: &Cx<'_>, rings: &FifoPlan, feedback: &FifoPlan, flush: &Flush) -> Self {
        let plan = cx.plan;
        let extra = widen(cx, &rings.extra, &feedback.extra);
        let widest = (0..plan.event_slot_capacity.len())
            .map(|i| plan.event_slot_capacity[i].events(cx.cap) + extra[i])
            .max()
            .unwrap_or(0);
        let drained = flush
            .rooms
            .values()
            .filter(|r| r.closers + r.carried > 0)
            .count();
        Self {
            extra_total: extra.iter().sum(),
            flushed_total: flush.rooms.values().map(|r| r.flush).sum(),
            held_total: flush.rooms.values().map(|r| r.room).sum(),
            drained_total: drained * widest.max(cx.cap),
            extra,
            widest,
        }
    }

    /// Everything that can flow into one node's inputs at once, past the
    /// slots: the overlay's room.
    fn flowing(&self) -> usize {
        self.flushed_total + self.held_total + self.extra_total + self.drained_total
    }
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
        .map(|p| index(p.taps.iter().copied()))
        .unwrap_or_default();
    let old_event_taps = cx
        .old
        .map(|p| index(p.event_taps.iter().copied()))
        .unwrap_or_default();
    let old_meters = cx
        .old
        .map(|p| index(p.meters.iter().map(|(k, _)| *k)))
        .unwrap_or_default();
    let carried =
        |opened: bool, from: Option<&usize>| (!opened).then(|| from.map(|&i| i as u32)).flatten();
    let mut out = Outlets {
        taps: Vec::with_capacity(plan.taps.len()),
        taps_from: Vec::with_capacity(plan.taps.len()),
        event_taps: Vec::with_capacity(plan.event_taps.len()),
        event_taps_from: Vec::with_capacity(plan.event_taps.len()),
        meters: Vec::with_capacity(plan.meters.len()),
        meters_from: Vec::with_capacity(plan.meters.len()),
    };
    for k in &plan.taps {
        let w = open(&mut c.opened.taps, k);
        out.taps_from.push(carried(w.is_some(), old_taps.get(k)));
        out.taps.push(w);
    }
    for k in &plan.event_taps {
        let w = open(&mut c.opened.event_taps, k);
        out.event_taps_from
            .push(carried(w.is_some(), old_event_taps.get(k)));
        out.event_taps.push(w);
    }
    for (k, lanes) in &plan.meters {
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
            p.sum_lanes
                .iter()
                .enumerate()
                .filter_map(|(i, id)| Some(((*id)?, i)))
                .collect()
        })
        .unwrap_or_default();
    let mut driven = vec![false; plan.sum_lanes.len()];
    for op in &plan.ops {
        if let Op::WeightedSum { lanes, gains, .. } = *op {
            for g in &plan.gain_ports[gains.range()] {
                driven[lanes.start as usize + usize::from(g.lane)] = true;
            }
        }
    }
    let mut from = Vec::with_capacity(plan.sum_lanes.len());
    let lanes = plan
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
                    cx.declick,
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
) -> Vec<(u32, ParamState<F>)> {
    let driven_of = |p: &Plan, rec: &crate::plan::NodeRec| {
        p.param_ports[rec.params.range()]
            .last()
            .is_some_and(ParamPortOp::is_mix)
    };
    let old_driven: BTreeSet<u32> = cx
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
            let key = cx.plan.units[r.unit as usize].key;
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
fn param_slots(plan: &Plan) -> Vec<bool> {
    let mut has = vec![false; plan.event_slot_capacity.len()];
    for spec in &plan.event_feedback {
        if let FeedbackKey::Event { from, .. } = spec.key {
            has[spec.slot as usize] |= plan.event_port_params(from);
        }
    }
    // Ops run in topological order, so a merge comes after what feeds it.
    for op in &plan.ops {
        match *op {
            Op::Node { unit, .. } if plan.units[unit as usize].shape.param_events => {
                let rec = &plan.nodes.recs[unit as usize];
                let [_, _, _, eout] = rec.ports(&plan.nodes.slots);
                for &s in eout {
                    has[s as usize] = true;
                }
            }
            Op::EventDelay { src, dst, .. } => {
                let from = has[src as usize];
                has[dst as usize] |= from;
            }
            Op::EventMerge { srcs, dst, .. } => {
                let any = plan.event_list[srcs.range()]
                    .iter()
                    .any(|&s| s != crate::plan::EMPTY_SLOT && has[s as usize]);
                has[dst as usize] |= any;
            }
            _ => {}
        }
    }
    has
}

/// A FIFO carried across a commit that lowered its bound can hold more than
/// the bound its output slot was priced at: each slot is widened by the most
/// that can be, and so is every merge downstream of it.
fn widen(cx: &Cx<'_>, ring_extra: &[usize], efb_extra: &[usize]) -> Vec<usize> {
    let plan = cx.plan;
    let mut extra = vec![0usize; plan.event_slot_capacity.len()];
    for (spec, &x) in plan.event_feedback.iter().zip(efb_extra) {
        let e = &mut extra[spec.slot as usize];
        *e = (*e).max(x);
    }
    // Ops run in topological order, so a merge comes after whatever feeds it.
    for op in &plan.ops {
        match *op {
            Op::EventDelay { delay, dst, .. } => {
                let e = &mut extra[dst as usize];
                *e = (*e).max(ring_extra[delay as usize]);
            }
            Op::EventMerge { srcs, dst, .. } => {
                let sum = plan.event_list[srcs.range()]
                    .iter()
                    .map(|&s| extra[s as usize])
                    .sum::<usize>();
                let e = &mut extra[dst as usize];
                *e = (*e).max(sum);
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
        chase_events: Vec::new(),
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
/// fed, as an index into `delays`; of equal lengths, the lowest key. The choice
/// is a function of the set of rings, not of their order in the plan: what a
/// source on a new channel starts from, with none of its history zero-filled.
fn ring_by_source(delays: &[crate::DelaySpec]) -> BTreeMap<crate::PortSource, usize> {
    let mut by_source: BTreeMap<crate::PortSource, usize> = BTreeMap::new();
    for (i, d) in delays.iter().enumerate() {
        let DelayKey::Output { from, .. } = d.key else {
            continue;
        };
        let better = |j: &usize| {
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
/// ring has its own key: the old root's channel `c` for the bank's channel
/// `base + c`, and, in a root change, for a root channel of a source the old
/// plan aligned on another channel, that channel's ring (a source's delayed
/// signal does not depend on the channel it feeds).
fn copy_origin(
    plan: &Plan,
    key: &DelayKey,
    old: &BTreeMap<DelayKey, usize>,
    by_source: &BTreeMap<crate::PortSource, usize>,
) -> Option<usize> {
    let DelayKey::Output { channel, from } = *key else {
        return None;
    };
    match plan.root_bank() {
        Some(bank) if usize::from(channel) >= bank.base => {
            let was = channel - u16::try_from(bank.base).ok()?;
            old.get(&DelayKey::Output { channel: was, from }).copied()
        }
        // Only in a root change: any other recompile keeps a ring by its key.
        Some(_) if !old.contains_key(key) => by_source.get(&from).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DelaySpec, NodeKey, OutPort, PortSource};
    use tutti_units::Samples;

    fn ring(channel: u16, from: PortSource, len: usize) -> DelaySpec {
        DelaySpec {
            key: DelayKey::Output { channel, from },
            len: Samples::new(len),
        }
    }

    fn source(node: u64) -> PortSource {
        PortSource::Node(OutPort {
            node: NodeKey(node),
            port: 0,
        })
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
        let by = ring_by_source(&delays);
        assert_eq!(
            by[&s], 2,
            "channel 1: of the two 30-frame rings, the lower key"
        );
        assert_eq!(by[&source(2)], 4, "a source of its own has its own ring");
        let mut order: Vec<usize> = (0..delays.len()).collect();
        for _ in 0..delays.len() {
            order.rotate_left(1);
            let shuffled: Vec<DelaySpec> = order.iter().map(|&i| delays[i]).collect();
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
        let at = crate::InPort {
            node: NodeKey(9),
            port: 0,
        };
        let delays = [DelaySpec {
            key: DelayKey::Audio {
                at,
                from: source(1),
            },
            len: Samples::new(40),
        }];
        assert!(ring_by_source(&delays).is_empty());
    }
}
