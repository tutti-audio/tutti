//! The op executor: a block's ops run on the calling thread alone, or its
//! tasks across a [`Pool`]'s threads.
//!
//! The plan already carries everything a spread block needs, computed by the
//! pure compiler: the ops fused into **tasks** (chains run back to back on
//! one thread), the task DAG as CSR successor rows, and each task's
//! **activation count** (its distinct predecessor tasks). Nothing is
//! derived per block.
//!
//! The app chooses the threads by the pool it sets: with a pool of more than
//! one participant, every block of a plan with more than one task runs on
//! all of them. Nothing else decides.
//!
//! One op code, `Ctx::run_op`, serves every block. How a participant
//! borrows what an op touches is a [`Mode`]: through claims
//! when several threads run at once, through plain `&mut` slices when the
//! calling thread is the only participant (`access.rs`).
//!
//! # A block alone
//!
//! The calling thread (the audio thread) is the one participant. A block
//! runs alone with no pool, a pool of one participant, or a plan of one
//! task. It walks the plan's ops in
//! order through plain `&mut` borrows: no task graph, no claim, no pool call.
//!
//! # A block spread over a pool
//!
//! 1. The caller arms the block's [`TaskGraph`] — built from the plan's
//!    tables when the plan was applied — and asks the pool to run one job on
//!    every participant of the pool, itself as participant 0.
//! 2. Every participant takes ready tasks and runs each task's ops through
//!    the same op code, on **claimed views** of the arena and event slots
//!    (`slots.rs`) and claimed units, delay rings, feedback state and output
//!    channels. The first successor a task makes ready runs inline on the
//!    same thread.
//! 3. The pool returns once every task has run and every helper has left the
//!    job; the caller sums the workers' dropped-event counts and ended fades.
//!
//! # Why the render does not depend on the participants
//!
//! Every op reads only values its DAG predecessors wrote (the compiler's
//! op DAG), every slot is shared only between values whose lifetimes cannot
//! overlap under *any* order the DAG allows (the colouring pass, checked by
//! the verifier), and each unit, ring, feedback delay and output channel is
//! touched by exactly one op. So each op sees the same inputs whatever order
//! the participants run it in, and does the same arithmetic. The only
//! cross-op sums (dropped events) are integer counts. The plan verifier
//! proves the schedule independence; `tests/executor/parallel.rs` holds one
//! participant against several, sample for sample, across worker counts and
//! block lengths.
//!
//! A node that shares mutable state with another node *outside* the graph's
//! ports (two nodes behind one `Arc<Mutex<_>>`, say) is not covered: the
//! executor orders only what the graph declares.
//!
//! # Safety without `unsafe`
//!
//! This crate stays `#![forbid(unsafe_code)]`. The claims come from
//! `tutti_rt::{SplitRw, SplitMut}`, whose borrows are checked at run time:
//! if two ops that share a slot ever ran at once (a compiler bug), the second
//! claim panics — it is never a second reference. The task dispatch
//! (`tutti_rt::TaskGraph`) and the pool handshake
//! (`tutti_rt::JobGate`, used by `tutti_graph::WorkerPool`) are loom-checked
//! there.
//!
//! # Real time
//!
//! Per block neither path allocates, takes a lock or makes a system call on
//! the audio thread, except a spread block's pool wake of its sleeping
//! helpers: the claim tables, the ready list and each worker's scratch (its
//! claim lists) are sized when a plan is applied. The
//! caller never waits for a helper that has not started; it waits only for
//! work a started helper is finishing. See `tests/rt_no_alloc.rs`.
//!
//! # Panics on a worker
//!
//! A node that panics on any participant aborts the block's task graph (so
//! the others stop taking tasks), and the caller resumes the first panic's
//! own payload (in participant order) once every participant has left, so the
//! caller unwinds with the node's message. The participant moves the payload
//! into its [`Worker`] without allocating, and it is freed while the caller
//! unwinds, never inside the block; so is a second participant's payload, which
//! is not reported. Only when no payload was stored (the pool
//! returned before the tasks were done) does the caller panic with a message
//! naming the cause. The node's own panic message has already been printed by
//! the panic hook.

mod access;

use crate::compile::ids::{
    AudioSlotId, EventSlotId, PlanUnitId, Table as IdTable, TableSlice as IdTableSlice,
};
use access::{Claimed, ClaimedAudio, ClaimedEvents, Exclusive, ExclusiveAudio, Mode, Table};
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use tutti_buffers::Float;
use tutti_units::Samples;

use tutti_platform::with_no_denormals;
use tutti_rt::{BlockWaits, ClaimTable, Held, Participant, SplitMut, SplitRw, TaskGraph};

use crate::compile::plan::{Op, Plan};
use crate::edges::rtlog::LogLane;
use crate::exec::Flags;
use crate::exec::arena::{Arena, Line};
use crate::exec::command::Deliveries;
use crate::exec::history::TransportHistory;
use crate::exec::loss::Dropped;
use crate::exec::slots::{EventSlot, EventSlots, SharedFlags};
use crate::exec::{
    Head, Inject, OpState, Unit, capture_op, delay_op, event_delay_op, event_merge_op, main_input,
    node_dispatch, output_op, sum_op,
};
use crate::model::gain::SumLane;
use crate::node::MAX_PORTS;
use crate::node::time::Env;
use tutti_buffers::MaxBlock;
use tutti_music::MeterMap;
use tutti_tempo::TempoMap;

/// A pool of threads that runs a job on several of them — what the parallel
/// executor (and a CLAP plugin's `clap.thread-pool` request) asks of a
/// worker pool. `tutti_graph::WorkerPool` implements it; a host with its own
/// pool (or a test) can too.
///
/// # Contract
///
/// [`run`](Self::run) calls `job(0)` on the calling thread and `job(i)` on at
/// most [`participants`](Self::participants)` - 1` other threads, each `i`
/// distinct and in `1..participants`, and returns only once no call of `job`
/// is still running. It may run `job(0)` alone — when its threads are busy
/// with another caller's job, say — and the executor's job is correct with
/// any number of participants, one included. A `run` called from inside a
/// job of this pool must not wait for that job: it may run `job(0)` alone.
/// On the audio thread it must not allocate, lock or wait for a thread that
/// has not started the job; waking sleeping threads is the one system call
/// it may make.
///
/// [`try_run`](Self::try_run) is the same call for a caller with a fallback
/// of its own: where `run` would run `job(0)` alone because the pool is
/// busy, it may instead run nothing and return `false`.
pub trait Pool: Send + Sync + 'static {
    /// Participants a job can have, the caller included. At least 1.
    fn participants(&self) -> usize;

    /// Run `job` as described in the trait docs. `wake` is how many
    /// participants (the caller included) the job can keep busy at once: a
    /// pool need not wake more threads than that.
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync));

    /// Run `job` as [`run`](Self::run) does and return `true`, or, when the
    /// pool is busy (another caller's job, or the job this call is made
    /// from), run nothing and return `false`. The default always runs.
    fn try_run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
        self.run(wake, job);
        true
    }

    /// How long a participant of the executor's block waits before it gives
    /// way: the caller by yielding its core, a helper by leaving the block
    /// (`tutti_rt::TaskGraph`'s "Waiting"). Read once per block, on the
    /// audio thread, so it must not allocate or lock. Required, so a pool
    /// that wraps another forwards its inner pool's rather than the
    /// defaults; one with no settings of its own returns
    /// [`BlockWaits::DEFAULT`].
    fn waits(&self) -> BlockWaits;
}

impl std::fmt::Debug for dyn Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("participants", &self.participants())
            .finish()
    }
}

/// One participant's scratch, sized when the plan is applied.
pub(crate) struct Worker {
    /// The audio slots an op's view holds.
    held_audio: Held,
    /// The event slots an op's view holds.
    held_events: Held,
    /// Events refused this block, and where.
    dropped: Dropped,
    /// Whether a crossfade ended this block.
    fade_ended: bool,
    /// The payload of a node's panic on this participant, moved here without
    /// allocating; the caller resumes it once every participant has left.
    panic: Option<Box<dyn Any + Send>>,
}

/// The workers of a block whose first panic the caller is resuming: dropped as
/// the caller unwinds, it frees every other participant's payload then, never
/// inside a block.
struct Unclaimed<'w>(&'w mut [Worker]);

impl Drop for Unclaimed<'_> {
    fn drop(&mut self) {
        for w in self.0.iter_mut() {
            drop(w.panic.take());
        }
    }
}

/// The parallel executor's per-plan state: the task graph and every claim
/// table, built when the plan is applied (with the rest of the executor's
/// state, `exec.rs`'s `rebuild`) for the pool's participant count.
pub(crate) struct Par {
    /// The pool participants this was built for (0: no pool).
    participants: usize,
    /// Whether a block spreads over the pool: a pool of more than one
    /// participant, and a plan of more than one task.
    spreads: bool,
    graph: TaskGraph,
    /// The per-slot flags, shared by the block's workers: seeded from the
    /// executor's own before each block (slot 0, the feedback reads), and
    /// every other slot written by its op before it is read.
    flags: Vec<AtomicU8>,
    claims: Claims,
    /// Per plan unit, its run of the (unit-sorted) inject list.
    inject_spans: IdTable<PlanUnitId, std::ops::Range<usize>>,
    workers: Vec<Worker>,
    worker_claims: ClaimTable,
}

/// One claim table per table of the executor's state, in its order, each
/// sized to the table it guards: an op claims the entries it touches, and a
/// second claim of one entry in a block fails.
struct Claims {
    audio: ClaimTable,
    events: ClaimTable,
    units: ClaimTable,
    audio_delays: ClaimTable,
    event_delays: ClaimTable,
    audio_fb: ClaimTable,
    event_fb: ClaimTable,
    taps: ClaimTable,
    event_taps: ClaimTable,
    meters: ClaimTable,
    lanes: ClaimTable,
    inject: ClaimTable,
    has_inject: ClaimTable,
    has_due: ClaimTable,
    outputs: ClaimTable,
}

impl Claims {
    /// The tables for `plan`, with `store_len` units in the unit store and
    /// `injects` entries in the inject list. Allocates.
    fn for_plan(plan: &Plan, store_len: usize, injects: usize) -> Self {
        let units = plan.units.len();
        Self {
            audio: ClaimTable::new(plan.program.sizing.audio.len()),
            events: ClaimTable::new(plan.program.sizing.events.len()),
            units: ClaimTable::new(store_len),
            audio_delays: ClaimTable::new(plan.program.audio_delays.len()),
            event_delays: ClaimTable::new(plan.program.event_delays.len()),
            audio_fb: ClaimTable::new(plan.program.audio_feedback.len()),
            event_fb: ClaimTable::new(plan.program.event_feedback.len()),
            taps: ClaimTable::new(plan.program.taps.len()),
            event_taps: ClaimTable::new(plan.program.event_taps.len()),
            meters: ClaimTable::new(plan.program.meters.len()),
            lanes: ClaimTable::new(plan.program.sum_lanes.len()),
            inject: ClaimTable::new(injects),
            has_inject: ClaimTable::new(units),
            has_due: ClaimTable::new(units),
            outputs: ClaimTable::new(plan.rendered_outputs()),
        }
    }
}

impl Par {
    /// For `plan`, with `inject` the executor's (unit-sorted) flushed-event
    /// list and `store_len` its unit store's length. Allocates.
    pub(crate) fn new(
        plan: &Plan,
        participants: usize,
        store_len: usize,
        inject_units: impl Iterator<Item = PlanUnitId>,
    ) -> Self {
        let successors: Vec<u32> = plan.task_succ.targets.iter().map(|t| t.get()).collect();
        let graph = TaskGraph::new(
            plan.task_activation.as_slice(),
            &plan.task_succ.offsets,
            &successors,
        );
        let units = plan.units.len();
        let mut inject_spans = IdTable::filled(units, 0..0);
        let mut n_inject = 0u32;
        for (i, u) in inject_units.enumerate() {
            let run = &mut inject_spans[u];
            if run.start == run.end {
                *run = i..i + 1;
            } else {
                debug_assert_eq!(run.end, i, "injects sorted by unit");
                run.end = i + 1;
            }
            n_inject += 1;
        }
        let spreads = participants > 1 && plan.tasks.len() > 1;
        // One worker per participant a block wakes.
        let workers = if spreads { participants } else { 1 };
        // What one op's views can hold at once: a node's ports and the
        // event source of each param it declares, or an event merge's
        // inputs and output.
        let held = 2 * MAX_PORTS + crate::node::param::MAX_PARAM_PORTS + 1;
        // A node's event slots also include the source of each input beside
        // its view.
        let held_events = held + MAX_PORTS;
        Self {
            participants,
            spreads,
            graph,
            flags: (0..plan.program.sizing.audio.len())
                .map(|_| AtomicU8::new(0))
                .collect(),
            claims: Claims::for_plan(plan, store_len, n_inject as usize),
            inject_spans,
            workers: (0..workers)
                .map(|_| Worker {
                    held_audio: Held::new(held),
                    held_events: Held::new(held_events),
                    dropped: Dropped::new(),
                    fade_ended: false,
                    panic: None,
                })
                .collect(),
            worker_claims: ClaimTable::new(workers),
        }
    }
}

impl Par {
    /// Whether blocks spread over the pool.
    pub(crate) fn spreads(&self) -> bool {
        self.spreads
    }

    /// The participants a block wakes on `pool`, or `None` when the calling
    /// thread runs it alone: every participant of the pool this was built
    /// for.
    fn wake(&self, pool: &dyn Pool) -> Option<usize> {
        (self.spreads && self.participants == pool.participants()).then_some(self.participants)
    }
}

/// What one block reads and never writes, built once per block and the same
/// for every participant: `Sync`, so the pool hands `&BlockInputs` to its
/// threads.
pub(crate) struct BlockInputs<'b, F: Float = f32> {
    pub(crate) plan: &'b Plan,
    pub(crate) env: &'b Env,
    /// Observed before the block, read-only while it runs.
    pub(crate) history: &'b TransportHistory,
    /// The project's meter for the block, lent by the host.
    pub(crate) meter: Option<&'b MeterMap>,
    /// The project's tempo map for the block, lent by the host.
    pub(crate) tempo: Option<&'b TempoMap>,
    pub(crate) frames: usize,
    pub(crate) max: MaxBlock,
    pub(crate) cap: usize,
    /// The length of every declick fade of this block (`Prepare::declick`).
    pub(crate) declick: Samples,
    pub(crate) inputs: &'b [&'b [F]],
    pub(crate) due: Deliveries<'b>,
}

/// What one block's parallel run claims: the executor's state it writes,
/// and the [`BlockInputs`] it reads.
pub(crate) struct Block<'b, 'o, F: Float = f32> {
    pub(crate) read: BlockInputs<'b, F>,
    pub(crate) outputs: &'b mut [&'o mut [F]],
    pub(crate) arena: &'b mut Arena<F>,
    pub(crate) flags: &'b mut [Flags],
    pub(crate) events: &'b mut [EventSlot],
    pub(crate) store: &'b mut [Option<Unit<F>>],
    pub(crate) audio_delays: &'b mut [Option<crate::exec::rings::AudioRing<F>>],
    pub(crate) event_delays: &'b mut [Option<crate::exec::rings::EventFifo>],
    pub(crate) audio_fb: &'b mut [Option<crate::exec::rings::AudioRing<F>>],
    pub(crate) event_fb: &'b mut [Option<crate::exec::rings::EventFifo>],
    pub(crate) taps: &'b mut [Option<crate::edges::tap::TapWriter<F>>],
    pub(crate) event_taps: &'b mut [Option<crate::edges::event_tap::EventTapWriter>],
    pub(crate) meters: &'b mut [Option<crate::edges::meter::MeterWriter>],
    pub(crate) lanes: &'b mut [SumLane<F>],
    pub(crate) inject: &'b mut [Inject],
    pub(crate) has_inject: &'b mut IdTableSlice<PlanUnitId, bool>,
    pub(crate) has_due: &'b mut IdTableSlice<PlanUnitId, bool>,
    /// Where the block's lost events are added up.
    pub(crate) dropped: &'b mut Dropped,
    /// The executor's log lanes, one per participant, and the table they are
    /// claimed through: both the executor's, not this plan's.
    pub(crate) logs: &'b mut [LogLane],
    pub(crate) log_claims: &'b mut ClaimTable,
}

/// What every participant shares while the block runs on a pool: `Sync`, so
/// the pool can hand `&Shared` to its threads.
struct Shared<'s, 'o, F: Float = f32> {
    read: BlockInputs<'s, F>,
    /// Per plan unit, its run of the (unit-sorted) inject list.
    inject_spans: &'s IdTableSlice<PlanUnitId, std::ops::Range<usize>>,
    graph: &'s TaskGraph,
    arena: SplitRw<'s, Line>,
    flags: &'s [AtomicU8],
    events: SplitRw<'s, EventSlot>,
    store: SplitMut<'s, Option<Unit<F>>>,
    audio_delays: SplitMut<'s, Option<crate::exec::rings::AudioRing<F>>>,
    event_delays: SplitMut<'s, Option<crate::exec::rings::EventFifo>>,
    audio_fb: SplitMut<'s, Option<crate::exec::rings::AudioRing<F>>>,
    event_fb: SplitMut<'s, Option<crate::exec::rings::EventFifo>>,
    taps: SplitMut<'s, Option<crate::edges::tap::TapWriter<F>>>,
    event_taps: SplitMut<'s, Option<crate::edges::event_tap::EventTapWriter>>,
    /// Atomics only, one writer a lane by construction: shared, no claim.
    meters: SplitMut<'s, Option<crate::edges::meter::MeterWriter>>,
    /// A weighted lane belongs to one op, which claims its run.
    lanes: SplitMut<'s, SumLane<F>>,
    inject: SplitMut<'s, Inject>,
    has_inject: SplitMut<'s, bool>,
    has_due: SplitMut<'s, bool>,
    outputs: SplitMut<'s, &'o mut [F]>,
    workers: SplitMut<'s, Worker>,
    /// Each participant's log slots and ring, claimed by its index for the
    /// whole block.
    logs: SplitMut<'s, LogLane>,
    /// The pool's wait budgets, for each participant's `work`.
    waits: BlockWaits,
}

/// Run one block's ops: spread over `pool`'s participants when the plan
/// spreads ([`Par::spreads`]), else on the calling thread alone. The events
/// dropped are added to `b.dropped`; returns
/// whether a crossfade ended.
///
/// # Panics
///
/// If a node panicked on any participant (after every participant has left
/// the block): the first stored payload, in participant order, is resumed.
pub(crate) fn run_block<F: Float>(
    par: &mut Par,
    pool: Option<&Arc<dyn Pool>>,
    b: Block<'_, '_, F>,
) -> bool {
    match pool.and_then(|p| par.wake(&**p).map(|wake| (p, wake))) {
        Some((pool, wake)) => run_spread(par, pool, wake, b),
        None => run_alone(par, b),
    }
}

/// The block on the calling thread alone: the plan's ops in order, each
/// through plain `&mut` borrows. No task graph, no claim.
fn run_alone<F: Float>(par: &mut Par, b: Block<'_, '_, F>) -> bool {
    let mut ended = false;
    let mut cx = Ctx::<F, Exclusive> {
        store: b.store,
        audio_delays: b.audio_delays,
        event_delays: b.event_delays,
        audio_fb: b.audio_fb,
        event_fb: b.event_fb,
        taps: b.taps,
        event_taps: b.event_taps,
        meters: b.meters,
        lanes: b.lanes,
        inject: b.inject,
        has_inject: b.has_inject.as_mut_slice(),
        has_due: b.has_due.as_mut_slice(),
        outputs: b.outputs,
        audio: ExclusiveAudio {
            arena: b.arena,
            flags: b.flags,
        },
        events: b.events,
        dropped: b.dropped,
        fade_ended: &mut ended,
        inject_spans: &par.inject_spans,
        log: &mut b.logs[0],
    };
    for op in &b.read.plan.program.ops {
        cx.run_op(&b.read, op);
    }
    ended
}

/// The block's tasks across `pool`, `wake` participants, the caller one of
/// them.
fn run_spread<F: Float>(
    par: &mut Par,
    pool: &Arc<dyn Pool>,
    wake: usize,
    b: Block<'_, '_, F>,
) -> bool {
    par.graph.begin();
    for w in &mut par.workers {
        w.dropped.reset();
        w.fade_ended = false;
    }
    // Slot 0 and the feedback reads were set before the ops; the rest is
    // written by the op that writes each slot. A byte per slot.
    for (a, &f) in par.flags.iter().zip(b.flags.iter()) {
        a.store(f.to_byte(), std::sync::atomic::Ordering::Relaxed);
    }
    let Par {
        participants: _,
        spreads: _,
        graph,
        flags,
        claims,
        inject_spans,
        workers,
        worker_claims,
    } = par;
    let Claims {
        audio,
        events,
        units,
        audio_delays,
        event_delays,
        audio_fb,
        event_fb,
        taps,
        event_taps,
        meters,
        lanes,
        inject,
        has_inject,
        has_due,
        outputs,
    } = claims;
    let aborted = {
        let shared = Shared {
            read: b.read,
            inject_spans,
            graph,
            arena: b.arena.split(audio),
            flags,
            events: SplitRw::new(b.events, 1, events),
            store: SplitMut::new(b.store, units),
            audio_delays: SplitMut::new(b.audio_delays, audio_delays),
            event_delays: SplitMut::new(b.event_delays, event_delays),
            audio_fb: SplitMut::new(b.audio_fb, audio_fb),
            event_fb: SplitMut::new(b.event_fb, event_fb),
            taps: SplitMut::new(b.taps, taps),
            event_taps: SplitMut::new(b.event_taps, event_taps),
            meters: SplitMut::new(b.meters, meters),
            lanes: SplitMut::new(b.lanes, lanes),
            inject: SplitMut::new(b.inject, inject),
            has_inject: SplitMut::new(b.has_inject.as_mut_slice(), has_inject),
            has_due: SplitMut::new(b.has_due.as_mut_slice(), has_due),
            outputs: SplitMut::new(b.outputs, outputs),
            workers: SplitMut::new(workers, worker_claims),
            logs: SplitMut::new(b.logs, b.log_claims),
            // The pool's, not the defaults
            // (`tests/executor/parallel.rs`'s `the_executor_waits_by_its_pools_budget`).
            waits: pool.waits(),
        };
        pool.run(wake, &|w| shared.participate(w));
        shared.graph.is_aborted() || !shared.graph.done()
    };
    if aborted && let Some(payload) = workers.iter_mut().find_map(|w| w.panic.take()) {
        // Every other participant's payload is freed by this guard as the
        // caller unwinds, so none is left for a later block to drop in scope.
        let _rest = Unclaimed(workers);
        resume_unwind(payload);
    }
    assert!(
        !aborted,
        "a node panicked on a parallel worker (its message is above), or the pool \
         returned before the block's tasks were done"
    );
    let mut ended = false;
    for w in workers.iter_mut() {
        b.dropped.absorb(&mut w.dropped);
        ended |= w.fade_ended;
    }
    ended
}

impl<F: Float> Shared<'_, '_, F> {
    /// One participant's share of the block.
    fn participate(&self, w: usize) {
        if w >= self.workers.len() || w >= self.logs.len() {
            return;
        }
        let _rt = self.read.plan.prepare.enter_rt_scope();
        // Per participant, per block: a plugin on this thread may have
        // cleared it since the last one.
        with_no_denormals(|| {
            let Ok(mut ws) = self.workers.try_claim(w) else {
                return;
            };
            let Ok(mut lane) = self.logs.try_claim(w) else {
                return;
            };
            let who = if w == 0 {
                Participant::Caller
            } else {
                Participant::Helper(w)
            };
            let Worker {
                held_audio,
                held_events,
                dropped,
                fade_ended,
                panic,
            } = &mut *ws;
            let mut cx = Ctx::<F, Claimed> {
                store: &self.store,
                audio_delays: &self.audio_delays,
                event_delays: &self.event_delays,
                audio_fb: &self.audio_fb,
                event_fb: &self.event_fb,
                taps: &self.taps,
                event_taps: &self.event_taps,
                meters: &self.meters,
                lanes: &self.lanes,
                inject: &self.inject,
                has_inject: &self.has_inject,
                has_due: &self.has_due,
                outputs: &self.outputs,
                audio: ClaimedAudio {
                    split: &self.arena,
                    held: held_audio,
                    flags: SharedFlags(self.flags),
                },
                events: ClaimedEvents {
                    split: &self.events,
                    held: held_events,
                },
                dropped,
                fade_ended,
                inject_spans: self.inject_spans,
                log: &mut lane,
            };
            let r = catch_unwind(AssertUnwindSafe(|| {
                self.graph.work(who, self.waits, |t| {
                    let span = self.read.plan.tasks.as_slice()[t as usize];
                    for &op in &self.read.plan.task_ops[span.range()] {
                        cx.run_op(&self.read, &self.read.plan.program.ops[op]);
                    }
                });
            }));
            if let Err(payload) = r {
                *panic = Some(payload);
                self.graph.abort();
            }
        });
    }
}

/// One participant's borrows of the block's state, and its own scratch.
struct Ctx<'c, 'o, F: Float, M: Mode<F>>
where
    'o: 'c,
{
    store: M::Table<'c, Option<Unit<F>>>,
    audio_delays: M::Table<'c, Option<crate::exec::rings::AudioRing<F>>>,
    event_delays: M::Table<'c, Option<crate::exec::rings::EventFifo>>,
    audio_fb: M::Table<'c, Option<crate::exec::rings::AudioRing<F>>>,
    event_fb: M::Table<'c, Option<crate::exec::rings::EventFifo>>,
    taps: M::Table<'c, Option<crate::edges::tap::TapWriter<F>>>,
    event_taps: M::Table<'c, Option<crate::edges::event_tap::EventTapWriter>>,
    meters: M::Table<'c, Option<crate::edges::meter::MeterWriter>>,
    lanes: M::Table<'c, SumLane<F>>,
    inject: M::Table<'c, Inject>,
    has_inject: M::Table<'c, bool>,
    has_due: M::Table<'c, bool>,
    outputs: M::Table<'c, &'o mut [F]>,
    audio: M::Audio<'c>,
    events: M::Events<'c>,
    /// Events refused, and where.
    dropped: &'c mut Dropped,
    /// Whether a crossfade ended.
    fade_ended: &'c mut bool,
    /// Per plan unit, its run of the (unit-sorted) inject list.
    inject_spans: &'c IdTableSlice<PlanUnitId, std::ops::Range<usize>>,
    /// The log slots and sink of this participant.
    log: &'c mut LogLane,
}

impl<F: Float, M: Mode<F>> Ctx<'_, '_, F, M> {
    /// One op, on this participant's borrows.
    #[inline]
    fn run_op(&mut self, fx: &BlockInputs<'_, F>, op: &Op<AudioSlotId, EventSlotId>) {
        let frames = fx.frames;
        match *op {
            Op::MainInput { channel, dst } => {
                let (mut a, flags) = M::audio(&mut self.audio);
                main_input(&mut a, flags, fx.inputs, channel, dst, frames);
            }
            Op::Delay { delay, src, dst } => {
                let mut ring = self.audio_delays.one(delay.index());
                let ring = ring.as_mut().expect("built by apply");
                let (mut a, flags) = M::audio(&mut self.audio);
                delay_op(ring, &mut a, flags, src, dst, frames);
            }
            Op::Sum { srcs, dst } => {
                let (mut a, flags) = M::audio(&mut self.audio);
                let e = M::events(&mut self.events);
                sum_op::<F, _, _, _>(
                    &mut a,
                    flags,
                    &e,
                    &fx.plan.program.audio_list[srcs.range()],
                    &mut [],
                    &[],
                    dst,
                    frames,
                    fx.declick,
                );
            }
            Op::WeightedSum {
                srcs,
                dst,
                lanes: span,
                gains,
            } => {
                let (mut a, flags) = M::audio(&mut self.audio);
                // The events of its driven lanes' sources, read-only.
                let e = M::events(&mut self.events);
                // The lanes of this op are its own: no other op names them.
                let mut lanes = self.lanes.run(span.range());
                sum_op::<F, _, _, _>(
                    &mut a,
                    flags,
                    &e,
                    &fx.plan.program.audio_list[srcs.range()],
                    &mut lanes,
                    &fx.plan.program.gain_ports[gains.range()],
                    dst,
                    frames,
                    fx.declick,
                );
            }
            Op::EventDelay { delay, src, dst } => {
                let mut fifo = self.event_delays.one(delay.index());
                let fifo = fifo.as_mut().expect("built by apply");
                let mut e = M::events(&mut self.events);
                let lost = event_delay_op(fifo, &mut e, src, dst, frames);
                self.dropped.charge_delay(fx.plan, delay, lost);
            }
            Op::EventMerge { srcs, dst, bound } => {
                let mut e = M::events(&mut self.events);
                let lost = event_merge_op(
                    &fx.plan.program.event_list[srcs.range()],
                    dst,
                    bound,
                    &mut e,
                );
                self.dropped.charge_merge(bound, lost);
            }
            Op::Node { unit, .. } => {
                let pu = &fx.plan.units[unit];
                let mut u = self.store.one(pu.idx.0 as usize);
                let u = u
                    .as_mut()
                    .expect("the delta placed every unit the plan runs");
                let mut has_inject = self.has_inject.one(unit.index());
                let mut has_due = self.has_due.one(unit.index());
                let mut inject = self.inject.run(self.inject_spans[unit].clone());
                let (mut a, flags) = M::audio(&mut self.audio);
                let mut e = M::events(&mut self.events);
                let head = Head {
                    unit,
                    pu,
                    ports: &fx.plan.ports[unit],
                    plan: fx.plan,
                    frames,
                    max: fx.max,
                    cap: fx.cap,
                    env: fx.env,
                    history: fx.history,
                    meter: fx.meter,
                    tempo: fx.tempo,
                    declick: fx.declick,
                };
                let mut st = OpState {
                    arena: &mut a,
                    flags,
                    events: &mut e,
                    inject: &mut inject,
                    has_inject: &mut has_inject,
                    has_due: &mut has_due,
                    due: fx.due,
                    dropped: self.dropped,
                    log: &self.log.slots,
                    sink: &mut self.log.sink,
                };
                *self.fade_ended |= node_dispatch(u, &head, &mut st);
            }
            Op::Output {
                channel,
                src,
                delay,
            } => {
                let mut out = self.outputs.one(channel as usize);
                let mut ring = delay.map(|d| self.audio_delays.one(d.index()));
                let (a, flags) = M::audio(&mut self.audio);
                output_op(
                    ring.as_mut().map(|r| r.as_mut().expect("built by apply")),
                    &mut out[..frames],
                    &a,
                    flags,
                    src,
                );
            }
            Op::Tap { tap, srcs } => {
                let mut tap = self.taps.one(tap.index());
                if let Some(tap) = tap.as_mut() {
                    let (a, _) = M::audio(&mut self.audio);
                    crate::edges::tap::tap_op::<F, _>(
                        tap,
                        &a,
                        &fx.plan.program.audio_list[srcs.range()],
                        fx.env.frame,
                        frames,
                    );
                }
            }
            Op::EventTap { tap, src } => {
                let mut tap = self.event_taps.one(tap.index());
                if let Some(tap) = tap.as_mut() {
                    let e = M::events(&mut self.events);
                    crate::edges::event_tap::event_tap_op(tap, &e, src, fx.env);
                }
            }
            Op::Meter { meter, srcs } => {
                let mut meter = self.meters.one(meter.index());
                if let Some(meter) = meter.as_mut() {
                    let (a, flags) = M::audio(&mut self.audio);
                    crate::edges::meter::meter_op::<F, _, _>(
                        meter,
                        &a,
                        flags,
                        &fx.plan.program.audio_list[srcs.range()],
                        frames,
                    );
                }
            }
            Op::Capture { feedback, src } => {
                let mut ring = self.audio_fb.one(feedback.index());
                let ring = ring.as_mut().expect("built by apply");
                let (a, flags) = M::audio(&mut self.audio);
                capture_op(ring, &a, flags, src, frames);
            }
            Op::EventCapture { feedback, src } => {
                let mut fifo = self.event_fb.one(feedback.index());
                let fifo = fifo.as_mut().expect("built by apply");
                let e = M::events(&mut self.events);
                let lost = fifo.push(e.slot(src));
                self.dropped
                    .charge_feedback(fx.plan, feedback, u64::from(lost));
            }
        }
    }
}
