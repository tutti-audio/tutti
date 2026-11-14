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

use tutti_curve::declick_frames;

use super::commit::Commit;
use super::node_op::{CONSTANT, SILENT};
use super::par_for;
use super::state::{Gone, Inject, Ring, State, Unit};
use crate::NodeKey;
use crate::arena::Arena;
use crate::command::overlay_capacity;
use crate::event::Event;
use crate::gain::SumLane;
use crate::held::Keys;
use crate::kernels::{AudioRing, EventFifo};
use crate::param::ParamState;
use crate::plan::{DelayKey, FeedbackKey, Op, ParamPortOp, Plan};
use crate::spec::{EventIn, EventOut};
use crate::{EventTapId, EventTapSource, MeterId, TapId};

/// Events the held keys of one removed edge can close into its sink at most:
/// a close of more is refused, and counted as lost at the sink. (A port
/// holds at most a key per note of a 16-channel group in practice; the
/// bitset's whole range would make every cut edge reserve megabytes.)
pub(crate) const CLOSE_ROOM: usize = 1024;

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
}

/// A delay or feedback FIFO that goes away: its queue flushes into `at`.
pub(super) struct Vanished {
    pub(super) from: u32,
    pub(super) at: EventIn,
}

/// Everything a commit's apply installs, built on the control side.
pub(super) struct Staged {
    /// Whether time-based state carries (no sample-rate change).
    pub(super) carry: bool,
    /// The pool participants the parallel state was built for (0: none).
    pub(super) participants: usize,
    /// The new state, shell: every ring and writer that comes from the
    /// commit is in place, the rest waits for what carries.
    pub(super) state: Option<State>,
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
    pub(super) closers: Vec<(EventOut, EventIn)>,
    /// Units (plan indices) that may hold a crossfade queued behind a
    /// running one.
    pub(super) queued: Vec<u32>,
    /// The store index and the lane state of each unit whose mix a plan
    /// drives and that may have none.
    pub(super) drives: Vec<(u32, ParamState)>,
    /// Room for what a held-key close writes before it moves on.
    pub(super) closes: Vec<Event>,
    pub(super) spare_rings: Vec<Ring>,
    pub(super) spare_drives: Vec<ParamState>,
    /// The executor's chase buffer from this plan on.
    pub(super) chase: Vec<Event>,
    /// A store with room for the plan's units, when the executor's has none.
    pub(super) store: Option<Vec<Option<Unit>>>,
    /// The store length the plan needs.
    store_len: usize,
}

impl Staged {
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

/// One injection buffer the executor holds: for the next commit's flush.
struct Shell {
    key: NodeKey,
    port: u16,
    cap: usize,
}

/// What the editor knows of the executor's state, as of the last commit it
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

/// What [`Stager::stage`] needs from the commit's editor.
pub(crate) struct StageIn<'a> {
    /// The plan the executor runs before this commit.
    pub(crate) old: Option<&'a Plan>,
    /// Whether time-based state carries: false for the resume of a re-prepare
    /// that changed the sample rate.
    pub(crate) carry: bool,
    /// This commit is a re-prepare's resume: every unit comes back, and the
    /// held-key tables of those the plan keeps go back to them.
    pub(crate) resumed: bool,
    /// The lane state for the mix of the unit at a key.
    pub(crate) mix_lane: &'a dyn Fn(NodeKey) -> Option<ParamState>,
}

impl Stager {
    pub(crate) fn new(event_cap: usize, pool: Arc<AtomicUsize>, rendered: Arc<AtomicU64>) -> Self {
        Self {
            event_cap,
            pool,
            rendered,
            fifos: BTreeMap::new(),
            feedback: BTreeMap::new(),
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
    #[allow(clippy::too_many_lines)]
    pub(crate) fn stage(&mut self, c: &mut Commit, input: StageIn<'_>) {
        let plan = Arc::clone(
            c.plan
                .as_ref()
                .expect("a commit carries its plan until applied"),
        );
        let plan: &Plan = &plan;
        let StageIn {
            old,
            carry,
            resumed,
            mix_lane,
        } = input;
        let max_block = plan.prepare.max_block().get();
        let cap = self.event_cap;
        let declick = declick_frames(plan.prepare.sample_rate()).get();

        // Crossfades in flight, and how long a chase can be, by key.
        for (key, f) in &c.delta.fades {
            if !f.is_cut() {
                *self.fading.entry(*key).or_default() += 1;
            }
        }
        for u in plan
            .units
            .iter()
            .chain(old.iter().flat_map(|p| p.units.iter()))
        {
            if let Some(n) = u.shape.chase {
                let e = self.chase.entry(u.key).or_default();
                *e = (*e).max(n.get() as usize);
            }
        }
        self.chase
            .retain(|k, _| plan.unit(*k).is_some() || self.fading.contains_key(k));

        // PDC rings and FIFOs, by key.
        let old_delays: BTreeMap<DelayKey, usize> = old
            .map(|p| {
                p.delays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (d.key, i))
                    .collect()
            })
            .unwrap_or_default();
        let mut fifos: BTreeMap<DelayKey, usize> = BTreeMap::new();
        let mut rings: Vec<Option<Ring>> = Vec::with_capacity(plan.delays.len());
        let mut ring_from: Vec<RingFrom> = Vec::with_capacity(plan.delays.len());
        let mut ring_extra: Vec<usize> = vec![0; plan.delays.len()];
        for (i, d) in plan.delays.iter().enumerate() {
            let from = old_delays.get(&d.key).filter(|_| carry).copied();
            match (d.key.event_origin(), from) {
                (Some(src), from) => {
                    let rate = plan.event_tap_rate(src, cap);
                    let want = EventFifo::bound(d.len.get(), rate, max_block);
                    let have = from.map(|_| self.fifos.get(&d.key).copied().unwrap_or(want));
                    match (from, have) {
                        (Some(j), Some(have)) if have >= want => {
                            rings.push(None);
                            ring_from.push(RingFrom::Keep {
                                from: j as u32,
                                rate,
                            });
                            ring_extra[i] = have - want;
                            fifos.insert(d.key, have);
                        }
                        (Some(j), Some(have)) => {
                            rings.push(Some(Ring::Event(EventFifo::sized_for(
                                d.len, rate, max_block, have,
                            ))));
                            ring_from.push(RingFrom::Move { from: j as u32 });
                            fifos.insert(d.key, want);
                        }
                        _ => {
                            rings.push(Some(Ring::Event(EventFifo::sized(d.len, rate, max_block))));
                            ring_from.push(RingFrom::New);
                            fifos.insert(d.key, want);
                        }
                    }
                }
                (None, Some(j)) if old.is_some_and(|p| p.delays[j].len == d.len) => {
                    rings.push(None);
                    ring_from.push(RingFrom::Keep {
                        from: j as u32,
                        rate: 0,
                    });
                }
                (None, from) => {
                    rings.push(Some(Ring::Audio(AudioRing::new(d.len))));
                    ring_from
                        .push(from.map_or(RingFrom::New, |j| RingFrom::Move { from: j as u32 }));
                }
            }
        }

        // Feedback delays, by key: exactly the edge's delay long.
        let keyed = |kind: &[crate::plan::FeedbackSpec]| -> BTreeMap<FeedbackKey, usize> {
            kind.iter().enumerate().map(|(i, f)| (f.key, i)).collect()
        };
        let old_afb = old.map(|p| keyed(&p.audio_feedback)).unwrap_or_default();
        let old_efb = old.map(|p| keyed(&p.event_feedback)).unwrap_or_default();
        let mut audio_fb_from: Vec<Option<u32>> = Vec::with_capacity(plan.audio_feedback.len());
        let audio_fb: Vec<Option<AudioRing>> = plan
            .audio_feedback
            .iter()
            .map(|f| {
                let from = old_afb.get(&f.key).filter(|_| carry).map(|&j| j as u32);
                audio_fb_from.push(from);
                from.is_none().then(|| AudioRing::new(f.key.delay()))
            })
            .collect();
        let mut feedback: BTreeMap<FeedbackKey, usize> = BTreeMap::new();
        let mut efb_from: Vec<RingFrom> = Vec::with_capacity(plan.event_feedback.len());
        let mut efb_extra: Vec<usize> = vec![0; plan.event_feedback.len()];
        let event_fb: Vec<Option<EventFifo>> = plan
            .event_feedback
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let FeedbackKey::Event { from, .. } = f.key else {
                    unreachable!("event feedback keys are events")
                };
                let rate = plan.event_tap_rate(EventTapSource::Out(from), cap);
                let want = EventFifo::bound(f.key.delay().get(), rate, max_block);
                let carried = old_efb.get(&f.key).filter(|_| carry).map(|&j| j as u32);
                let have = carried.map(|_| self.feedback.get(&f.key).copied().unwrap_or(want));
                match (carried, have) {
                    (Some(j), Some(have)) if have >= want => {
                        efb_from.push(RingFrom::Keep { from: j, rate });
                        efb_extra[i] = have - want;
                        feedback.insert(f.key, have);
                        None
                    }
                    (Some(j), Some(have)) => {
                        efb_from.push(RingFrom::Move { from: j });
                        feedback.insert(f.key, want);
                        Some(EventFifo::sized_for(f.key.delay(), rate, max_block, have))
                    }
                    _ => {
                        efb_from.push(RingFrom::New);
                        feedback.insert(f.key, want);
                        Some(EventFifo::sized(f.key.delay(), rate, max_block))
                    }
                }
            })
            .collect();

        // Taps, by id: one opened since the last commit arrives in the
        // commit and wins over a carried one of the same id (opening an id
        // again replaces it). A writer the plan does not name stays in the
        // old state (or the commit) and is freed on the control side.
        let old_taps: BTreeMap<TapId, usize> = old
            .map(|p| p.taps.iter().enumerate().map(|(i, k)| (*k, i)).collect())
            .unwrap_or_default();
        let mut taps_from = Vec::with_capacity(plan.taps.len());
        let taps = plan
            .taps
            .iter()
            .map(|k| {
                let opened = c
                    .opened
                    .taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| c.opened.taps.swap_remove(at).1);
                taps_from.push(
                    opened
                        .is_none()
                        .then(|| old_taps.get(k).map(|&i| i as u32))
                        .flatten(),
                );
                opened
            })
            .collect();
        let old_event_taps: BTreeMap<EventTapId, usize> = old
            .map(|p| {
                p.event_taps
                    .iter()
                    .enumerate()
                    .map(|(i, k)| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        let mut event_taps_from = Vec::with_capacity(plan.event_taps.len());
        let event_taps = plan
            .event_taps
            .iter()
            .map(|k| {
                let opened = c
                    .opened
                    .event_taps
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| c.opened.event_taps.swap_remove(at).1);
                event_taps_from.push(
                    opened
                        .is_none()
                        .then(|| old_event_taps.get(k).map(|&i| i as u32))
                        .flatten(),
                );
                opened
            })
            .collect();
        let old_meters: BTreeMap<MeterId, usize> = old
            .map(|p| {
                p.meters
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (*k, i))
                    .collect()
            })
            .unwrap_or_default();
        let mut meters_from = Vec::with_capacity(plan.meters.len());
        let meters = plan
            .meters
            .iter()
            .map(|(k, lanes)| {
                let opened = c
                    .opened
                    .meters
                    .iter()
                    .position(|(o, _)| o == k)
                    .map(|at| c.opened.meters.swap_remove(at).1);
                if let Some(w) = &opened {
                    w.set_width(*lanes);
                }
                meters_from.push(
                    opened
                        .is_none()
                        .then(|| old_meters.get(k).map(|&i| i as u32))
                        .flatten(),
                );
                opened
            })
            .collect();

        // Weighted lanes, by edge: what a lane holds carries across a commit
        // that keeps the edge (not a sample-rate change's reset: a weight is
        // not time), and a new edge starts at its value. A lane a source
        // drives has its state built here.
        let old_lanes: BTreeMap<crate::EdgeId, usize> = old
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
        let mut lanes_from = Vec::with_capacity(plan.sum_lanes.len());
        let lanes: Vec<SumLane> = plan
            .sum_lanes
            .iter()
            .zip(&driven)
            .map(|(id, &driven)| {
                lanes_from.push(id.and_then(|id| old_lanes.get(&id)).map(|&i| i as u32));
                match id {
                    None => SumLane::plain(),
                    Some(id) => SumLane::weighted(
                        c.opened
                            .gains
                            .get(id)
                            .cloned()
                            .expect("the editor sends a cell for every weighted lane"),
                        driven,
                        max_block,
                        declick,
                    ),
                }
            })
            .collect();
        let spare_drives = lanes_from.iter().flatten().count();

        // The lanes that drive a unit's mix: for a unit placed now, or one
        // no earlier plan drove.
        let driven_of = |p: &Plan, rec: &crate::plan::NodeRec| {
            p.param_ports[rec.params.range()]
                .last()
                .is_some_and(ParamPortOp::is_mix)
        };
        let old_driven: BTreeSet<u32> = old
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
        let drives: Vec<(u32, ParamState)> = plan
            .nodes
            .recs
            .iter()
            .filter(|r| driven_of(plan, r))
            .filter_map(|r| {
                let key = plan.units[r.unit as usize].key;
                (placed.contains(&key) || !old_driven.contains(&r.store))
                    .then(|| mix_lane(key).map(|lane| (r.store, lane)))
                    .flatten()
            })
            .collect();

        // What disappears flushes to sinks that survive, so each sink has a
        // buffer for it, sized for everything that can flow in.
        let sink_of = |at: EventIn| -> Option<u32> {
            let u = plan.units.binary_search_by_key(&at.node, |u| u.key).ok()?;
            (at.port < plan.units[u].shape.event_in).then_some(u as u32)
        };
        #[derive(Default)]
        struct Room {
            /// What flushes in.
            flush: usize,
            /// What a fade's end delivers later.
            room: usize,
        }
        let mut rooms: BTreeMap<(u32, u16), Room> = BTreeMap::new();
        // What replaced nodes wrote for their successors.
        for (old_p, new_p) in &c.delta.replace {
            let chase = old
                .and_then(|o| o.unit(old_p.key))
                .and_then(|u| u.shape.chase);
            if let (Some(n), Some(u)) = (
                chase,
                sink_of(EventIn {
                    node: new_p.key,
                    port: 0,
                }),
            ) {
                rooms.entry((u, 0)).or_default().flush += n.get() as usize;
            }
        }
        // Injections not yet delivered.
        for s in &self.shells {
            if let Some(u) = sink_of(EventIn {
                node: s.key,
                port: s.port,
            }) {
                rooms.entry((u, s.port)).or_default().flush += s.cap;
            }
        }
        // Event delays and feedback that no longer exist, in key order.
        let plan_delay_keys: BTreeSet<DelayKey> = plan.delays.iter().map(|d| d.key).collect();
        let gone_delays: Vec<(DelayKey, usize, EventIn)> = old_delays
            .iter()
            .filter(|(k, _)| !(carry && plan_delay_keys.contains(k)))
            .filter_map(|(k, &j)| match k {
                DelayKey::Event { at, .. } => Some((*k, j, *at)),
                _ => None,
            })
            .collect();
        let gone_delays: Vec<Vanished> = gone_delays
            .into_iter()
            .map(|(k, j, at)| {
                if let Some(u) = sink_of(at) {
                    rooms.entry((u, at.port)).or_default().flush +=
                        self.fifos.get(&k).copied().unwrap_or(0);
                }
                Vanished { from: j as u32, at }
            })
            .collect();
        let plan_efb: BTreeSet<FeedbackKey> = plan.event_feedback.iter().map(|f| f.key).collect();
        let gone_feedback: Vec<Vanished> = old_efb
            .iter()
            .filter(|(k, _)| !(carry && plan_efb.contains(k)))
            .filter_map(|(k, &j)| {
                let FeedbackKey::Event { at, .. } = k else {
                    return None;
                };
                if let Some(u) = sink_of(*at) {
                    rooms.entry((u, at.port)).or_default().flush +=
                        self.feedback.get(k).copied().unwrap_or(0);
                }
                Some(Vanished {
                    from: j as u32,
                    at: *at,
                })
            })
            .collect();

        // Held keys of an edge that ended, closed at the sink that lost it.
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
        let parked_gone: Vec<NodeKey> = match old {
            Some(o) if resumed => o
                .units
                .iter()
                .filter(|u| {
                    plan.unit(u.key)
                        .is_none_or(|n| n.generation != u.generation)
                })
                .map(|u| u.key)
                .collect(),
            _ => Vec::new(),
        };
        maybe_gone.extend(parked_gone.iter().copied());
        let replaced: BTreeSet<NodeKey> = c.delta.replace.iter().map(|(_, n)| n.key).collect();
        let mut closers = Vec::new();
        if let Some(o) = old {
            for e in &o.event_edges {
                let &(from, at) = e;
                let in_plan = plan.event_edges.binary_search(e).is_ok();
                if (in_plan && !maybe_gone.contains(&from.node)) || replaced.contains(&at.node) {
                    continue;
                }
                let Some(u) = sink_of(at) else {
                    continue;
                };
                if !o.unit(from.node).is_some_and(|f| f.shape.note_events) {
                    continue;
                }
                let delay = plan
                    .delays
                    .iter()
                    .any(|d| d.key == DelayKey::Event { at, from });
                if !(in_plan && delay) {
                    rooms.entry((u, at.port)).or_default().flush += CLOSE_ROOM;
                }
                closers.push((from, at));
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
                rooms.entry((u as u32, 0)).or_default().room += n;
            }
        }

        // A FIFO carried across a commit that lowered its bound can hold
        // more than the bound its output slot was priced at (`excess`): the
        // slot is widened by the most it can hold past it, and so is every
        // merge downstream of it.
        let mut extra = vec![0usize; plan.event_slot_capacity.len()];
        for (spec, &x) in plan.event_feedback.iter().zip(&efb_extra) {
            let e = &mut extra[spec.slot as usize];
            *e = (*e).max(x);
        }
        for op in &plan.ops {
            match *op {
                Op::EventDelay { delay, dst, .. } => {
                    let e = &mut extra[dst as usize];
                    *e = (*e).max(ring_extra[delay as usize]);
                }
                Op::EventMerge { srcs, dst } => {
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
        let extra_total: usize = extra.iter().sum();
        let slot_capacity = |i: usize| plan.event_slot_capacity[i].events(cap) + extra[i];
        let widest = (0..plan.event_slot_capacity.len())
            .map(slot_capacity)
            .max()
            .unwrap_or(0);
        let flushed_total: usize = rooms.values().map(|r| r.flush).sum();
        let held_total: usize = rooms.values().map(|r| r.room).sum();

        let mut shells = Vec::with_capacity(rooms.len());
        let inject: Vec<Inject> = rooms
            .iter()
            .map(|(&(unit, port), r)| {
                let events = r.flush + r.room;
                shells.push(Shell {
                    key: plan.units[unit as usize].key,
                    port,
                    cap: events,
                });
                Inject {
                    unit,
                    port,
                    events: Vec::with_capacity(events),
                    merged: Vec::with_capacity(events + widest),
                    live: false,
                    closed_from: 0,
                }
            })
            .collect();

        // The overlay and the parallel state.
        let overlay = overlay_capacity(plan, cap, flushed_total + held_total + extra_total);
        let store_len = self.store_len.max(c.delta.store_len as usize);
        let participants = self.pool.load(Ordering::Acquire);
        let par = par_for(
            plan,
            (participants > 0).then_some(participants),
            store_len,
            inject.iter().map(|i| i.unit),
            overlay,
        );
        let state = State {
            arena: Arena::new(plan.audio_slots as usize, max_block),
            flags: {
                let mut v = vec![0u8; plan.audio_slots as usize];
                v[0] = SILENT | CONSTANT;
                v
            },
            events: (0..plan.event_slot_capacity.len())
                .map(|i| Vec::with_capacity(slot_capacity(i)))
                .collect(),
            rings,
            audio_fb,
            event_fb,
            taps,
            event_taps,
            meters,
            lanes,
            inject,
            has_inject: vec![false; plan.units.len()],
            has_due: vec![false; plan.units.len()],
            overlay: Vec::with_capacity(overlay),
            par,
        };

        // The store grows by moving into one built here.
        let store = (store_len > self.store_cap).then(|| {
            let cap = store_len.max(self.store_cap * 2);
            self.store_cap = cap;
            Vec::with_capacity(cap)
        });
        self.store_len = store_len;

        // What the units that arrive hold.
        let parked_back = |key: NodeKey| {
            resumed
                && old
                    .and_then(|o| o.unit(key))
                    .zip(plan.unit(key))
                    .is_some_and(|(a, b)| a.generation == b.generation)
        };
        for inc in &mut c.incoming {
            let Some(shape) = plan.unit(inc.key).map(|u| u.shape) else {
                continue;
            };
            let ports = shape.event_out as usize;
            if ports > 0 && !parked_back(inc.key) {
                inc.keys.size(ports, shape.note_events, false);
            }
            if ports > 0 && inc.fade.is_some() {
                inc.staging = Keys::staging(ports, shape.note_events);
            }
            if let Some(f) = &mut inc.fade {
                let n = old
                    .and_then(|o| o.unit(inc.key))
                    .and_then(|u| u.shape.chase)
                    .map_or(0, |n| n.get() as usize);
                f.chase.reserve_exact(n);
            }
        }

        // Room for what applying hands back.
        let replaces = c.delta.replace.len();
        let parked = if resumed {
            old.map_or(0, |o| o.units.len())
        } else {
            0
        };
        let held = c.delta.retire.len() + replaces + c.delta.cuts.len() + parked;
        c.gone = Gone {
            held: Vec::with_capacity(held),
            spare: Vec::with_capacity(2 * (c.delta.retire.len() + replaces + parked)),
            replaced: Vec::with_capacity(replaces),
            chased: Vec::with_capacity(replaces),
            held_refused: Vec::with_capacity(replaces),
            chase_events: Vec::new(),
        };
        c.spare_held
            .reserve_exact(held + c.gone.spare.capacity() + 4 * c.incoming.len());
        let moved = ring_from
            .iter()
            .chain(&efb_from)
            .filter(|r| matches!(r, RingFrom::Move { .. }))
            .count();
        let wanted: usize = plan
            .units
            .iter()
            .filter_map(|u| u.shape.chase)
            .map(|n| n.get() as usize)
            .sum();
        let staged = Staged {
            carry,
            participants,
            state: Some(state),
            rings: ring_from,
            event_fb: efb_from,
            audio_fb: audio_fb_from,
            taps: taps_from,
            event_taps: event_taps_from,
            meters: meters_from,
            lanes: lanes_from,
            gone_delays,
            gone_feedback,
            closes: Vec::with_capacity(if closers.is_empty() { 0 } else { CLOSE_ROOM }),
            closers,
            queued,
            spare_drives: Vec::with_capacity(spare_drives + drives.len()),
            drives,
            spare_rings: Vec::with_capacity(moved),
            chase: Vec::with_capacity(wanted),
            store,
            store_len,
        };
        c.staged = Some(Box::new(staged));

        // The bounds the next commit builds from.
        self.fifos = fifos;
        self.feedback = feedback;
        self.shells = shells;
        self.shells_seq = c.seq;
        self.shells_seen = None;
    }
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
