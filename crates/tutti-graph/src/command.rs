//! Timestamped control commands: an event, or a param ramp or segment,
//! delivered into a node's event input **on an exact frame**.
//!
//! # The path
//!
//! ```text
//!   Editor::schedule(At, EventInputPort, EventKind | ParamKind)
//!       │  checked against the plan sent last; stamped with its sequence
//!       ▼
//!   [ command ring, COMMAND_CAPACITY ]   plain `Copy` values: nothing boxed,
//!       │                                nothing freed on the audio thread
//!       ▼
//!   Executor: at the start of every block, pull new commands into a
//!   preallocated pending list, resolve each against the block's `Env`
//!   (`Env::due`), and deliver what falls in this block at its offset
//! ```
//!
//! **Every command says when** — an [`At`]: a [`Frame`](tutti_units::Frame)
//! on the executor's clock, a [`Beat`](tutti_units::Beat) resolved against
//! the transport snapshot of the block it falls in, or
//! [`At::NextBlock`], which is a visible choice, not a default.
//!
//! **Frames: late is not lost.** A frame already past when the executor sees
//! the command lands at offset 0 of that block and is counted
//! ([`Executor::late_command_count`](crate::Executor::late_command_count)).
//!
//! **Beats: reached, not passed.** An `At::Beat` fires when the playhead
//! reaches or crosses it **through continuous playback** — a loop wrap that
//! lands at or after it counts as reaching it (see
//! [`BeatCrossings`]). A seek, or a loop that jumps *over* it,
//! does not fire it: it stays pending until reached, or cancelled. It is late
//! only when continuous playback crossed it before the command was
//! processed; it then lands at offset 0 of the block and is counted. So a
//! beat-timed command can wait indefinitely (stopped transport, a beat past
//! the loop end), holding its credit; and a note-on that fired with its
//! note-off still waiting is a stuck note — pairing is the caller's job, and
//! [`Editor::cancel`](crate::Editor::cancel) is how to take one back.
//!
//! **Cancel.** [`Editor::cancel`](crate::Editor::cancel) and
//! [`cancel_all`](crate::Editor::cancel_all) travel on their own small ring
//! ([`CANCEL_CAPACITY`]), which needs no credit, so even a full set of
//! commands that will never fall due can be taken back. The executor applies
//! cancels after pulling new commands, so a cancel always sees the command
//! it names; the freed credit returns like a delivery's.
//!
//! **Rate changes.** `Frame` means samples at the current rate since start.
//! When a re-prepare changes the rate, every pending `At::Frame` scheduled
//! before it is rescaled to the same wall-clock time at the new rate,
//! rounded to the nearest frame (`Editor::reprepare`).
//!
//! **Where it lands.** Into the named event input of whichever unit holds the
//! key when the command falls due — found by a binary search of the plan's
//! key-ordered units, never a hash. A command resolved against a commit the
//! executor has not applied yet waits for it (the two travel on separate
//! rings, so the command can be seen first). One whose node or port is gone by
//! then cannot be delivered, and is counted
//! ([`Executor::unrouted_command_count`](crate::Executor::unrouted_command_count)).
//!
//! **Order.** A scheduled event joins the port's own events as one more
//! source *after* the port's edges (and after events flushed from a vanished
//! delay): on a tie at one offset, the graph's events come first, then
//! scheduled ones by their timeline position, then in the order they were
//! scheduled. A scheduled param event joins the port's param events the same
//! way; the two lists are ordered apart, as a node reads them apart. The position is unrounded, so two frames that a rate change
//! rounds to one frame keep their order (frames 1 000 and 1 001, halved,
//! both land on 500 — 1 000 first); a late command's position is its past
//! frame, so it sorts before one due exactly at that offset.
//!
//! **Param events.** A [`ParamKind`] command lands in the port's param buffer
//! ([`SortedEvents::params`](crate::SortedEvents::params)), and only on a node
//! whose shape declares [`Shape::param_events`](crate::Shape::param_events):
//! [`Editor::schedule`](crate::Editor::schedule) refuses any other with
//! [`ScheduleError::NoParamEvents`]. A bypassed node is sent none, as it is
//! sent no scheduled event.
//!
//! **Timing and PDC.** A time is a **timeline** time, compensated like any
//! upstream event: a sink with compiled arrival latency `a` hears timeline
//! frame `F` at its own frame `F + a`, so an `At::Frame(F)` command lands
//! there — on the same sample of the node's audio it would have met had it
//! come down the latent path. An `At::Beat` is resolved to its timeline
//! frame first, then shifted the same way. `At::NextBlock` has no timeline
//! position and lands at offset 0, uncompensated. Late means late after
//! compensation.
//!
//! # Back-pressure
//!
//! At most [`COMMAND_CAPACITY`] commands are outstanding — sent and not yet
//! delivered, cancelled, or counted unroutable. The editor counts what it sent; the
//! executor counts what it finished, in one shared atomic; `schedule` refuses
//! with [`ScheduleError::Backpressure`] at the limit rather than letting the
//! ring or the executor's pending list grow. A command waiting for a far
//! future frame holds its credit until it lands, so the limit is on
//! *outstanding* commands, not on commands per block.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tutti_rt::{Overflow, PushError, RingReader, RingWriter, ring};
use tutti_units::{At, Latency};

use crate::event::{Event, EventKind, merge_into, merge_params, merge_walk};
use crate::node::Env;
use crate::node::MAX_PORTS;
use crate::param_event::{ParamEvent, ParamKind};
use crate::plan::Plan;
use crate::spec::EventInputPort;
use crate::time::{BeatCrossings, Due, Offset};

/// Commands that may be outstanding at once: sent, and not yet delivered
/// or cancelled.
pub const COMMAND_CAPACITY: usize = 256;

/// Cancellations that may wait in their ring for the executor's next block.
/// Past it [`Editor::cancel`](crate::Editor::cancel) refuses with
/// [`ScheduleError::Backpressure`] — a refusal on the control side, so
/// nothing is lost; retry after a block.
pub const CANCEL_CAPACITY: usize = 64;

/// A scheduled command, as [`Editor::schedule`](crate::Editor::schedule)
/// returns it — the handle [`Editor::cancel`](crate::Editor::cancel) takes.
///
/// Carries the identity of the editor/executor pair that issued it, so an id
/// kept past a rebuild (the recovery from a poisoned editor is a new pair)
/// cannot cancel the new pair's command that happens to share its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId {
    pair: u64,
    seq: u64,
}

/// Pair identities: every command channel takes the next one.
static NEXT_PAIR: tutti_rt::Mint = tutti_rt::Mint::new();

/// Why [`Editor::schedule`](crate::Editor::schedule) refused a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// Nothing has been committed yet, so there is no node to address.
    #[error("nothing committed yet")]
    NoPlan,
    /// The plan sent last has no such node, or the node no such event input.
    #[error("node {} has no event input {}", .to.node.0, .to.port)]
    NoSuchPort {
        /// The port asked for.
        to: EventInputPort,
    },
    /// The node has an event input of that number but does not declare
    /// [`Shape::param_events`](crate::Shape::param_events), so a param ramp or
    /// segment sent there would reach no one. Nothing was sent.
    #[error("node {} does not take param events: its shape does not declare them", .to.node.0)]
    NoParamEvents {
        /// The port asked for.
        to: EventInputPort,
    },
    /// [`COMMAND_CAPACITY`] commands are outstanding (or, for a cancel,
    /// [`CANCEL_CAPACITY`] cancels are queued). Nothing was sent; retry once
    /// some have landed.
    #[error("{} commands outstanding", COMMAND_CAPACITY)]
    Backpressure,
    /// The editor is poisoned (see
    /// [`CommitError::Poisoned`](crate::CommitError::Poisoned)).
    #[error("the editor is poisoned; build a new pair")]
    Poisoned,
    /// A cancel named a command this editor never issued: one from another
    /// editor/executor pair (an id kept across a rebuild), or one not
    /// scheduled yet. Nothing was sent.
    #[error("{id:?} was not issued by this editor")]
    UnknownCommand {
        /// The id.
        id: CommandId,
    },
}

impl ScheduleError {
    /// Whether retrying the same command later can succeed: no plan yet, or
    /// the queue full.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::NoPlan | Self::Backpressure => true,
            Self::NoSuchPort { .. }
            | Self::NoParamEvents { .. }
            | Self::Poisoned
            | Self::UnknownCommand { .. } => false,
        }
    }
}

/// What a command delivers: an event, or a param ramp or segment. `Copy`,
/// so the command ring moves it without a box.
///
/// [`Editor::schedule`](crate::Editor::schedule) takes either kind through
/// `Into`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scheduled {
    /// An event, delivered into the port's events.
    Event(EventKind),
    /// A param ramp or segment, delivered into the port's param events. Only
    /// a node that declares [`Shape::param_events`](crate::Shape::param_events)
    /// takes one.
    Param(ParamKind),
}

impl From<EventKind> for Scheduled {
    fn from(kind: EventKind) -> Self {
        Self::Event(kind)
    }
}

impl From<ParamKind> for Scheduled {
    fn from(kind: ParamKind) -> Self {
        Self::Param(kind)
    }
}

/// A command as it travels: `Copy`, so the ring moves it without a box.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Command {
    /// Scheduling order, for ties at one offset.
    seq: u64,
    /// The commit it was checked against: it waits until that is applied.
    plan_seq: u64,
    at: At,
    to: EventInputPort,
    payload: Scheduled,
    /// Its timeline position, unrounded — the tie-break after the offset:
    /// the frame it was scheduled for (rescaled, unrounded, on a rate
    /// change, so two frames that round to one keep their order), where a
    /// beat resolved, or where a `NextBlock` landed.
    pos: f64,
}

/// A cancellation, on its own ring: it needs no credit, so a full set of
/// commands that will never fall due can still be cancelled.
#[derive(Clone, Copy, Debug)]
enum Cancel {
    /// The command with this `seq`, if it is still pending.
    One(u64),
    /// Every command with a `seq` below this — every command scheduled
    /// before the cancel, and none after.
    Below(u64),
}

/// The editor's end.
pub(crate) struct CommandTx {
    /// This pair's identity, stamped into every `CommandId`.
    pair: u64,
    tx: RingWriter<Command>,
    cancel: RingWriter<Cancel>,
    sent: u64,
    done: Arc<AtomicU64>,
}

/// One command falling due this block, resolved to a plan unit: `T` is
/// the [`EventKind`] or [`ParamKind`] it delivers.
#[derive(Clone, Copy, Debug)]
struct DueItem<T> {
    unit: u32,
    port: u16,
    seq: u64,
    /// `Command::pos`, as bits (non-negative, so they sort like the value).
    pos: u64,
    offset: Offset,
    kind: T,
}

impl<T> DueItem<T> {
    /// `cmd` landing on plan unit `unit` at `offset`.
    fn of(cmd: &Command, unit: usize, offset: Offset, kind: T) -> Self {
        Self {
            unit: unit as u32,
            port: cmd.to.port,
            seq: cmd.seq,
            pos: cmd.pos.to_bits(),
            offset,
            kind,
        }
    }

    /// The order ties at one offset go in: by unit and port, then offset,
    /// then timeline position, then scheduling order. Unique (`seq` is), so
    /// the unstable sort is deterministic, and it sorts in place, where the
    /// stable one would allocate.
    fn order(&self) -> (u32, u16, Offset, u64, u64) {
        (self.unit, self.port, self.offset, self.pos, self.seq)
    }
}

/// The executor's end: the ring, the commands waiting for their frame, and
/// this block's deliveries. Every buffer is sized at construction.
pub(crate) struct CommandRx {
    rx: RingReader<Command>,
    cancel: RingReader<Cancel>,
    /// Cancels that arrived before the commands they name (see `pull`).
    held: Vec<Cancel>,
    /// One past the highest `seq` pulled so far.
    next_seq: u64,
    pending: Vec<Command>,
    due: Vec<DueItem<EventKind>>,
    due_events: Vec<Event>,
    due_params: Vec<DueItem<ParamKind>>,
    due_param_events: Vec<ParamEvent>,
    done: Arc<AtomicU64>,
    late: u64,
    unrouted: u64,
    cancelled: u64,
    /// What continuous playback has crossed: tells a late beat from one a
    /// seek jumped over.
    crossings: BeatCrossings,
}

impl CommandTx {
    /// Whether `rx` is this end's other half.
    pub(crate) fn same_pair(&self, rx: &CommandRx) -> bool {
        Arc::ptr_eq(&self.done, &rx.done)
    }
}

/// Build the command ring pair.
pub(crate) fn command_channel() -> (CommandTx, CommandRx) {
    let (tx, rx) = ring::<Command>(COMMAND_CAPACITY, Overflow::Refuse);
    let (cancel_tx, cancel_rx) = ring::<Cancel>(CANCEL_CAPACITY, Overflow::Refuse);
    let done = Arc::new(AtomicU64::new(0));
    (
        CommandTx {
            pair: NEXT_PAIR.mint().get(),
            tx,
            cancel: cancel_tx,
            sent: 0,
            done: Arc::clone(&done),
        },
        CommandRx {
            rx,
            cancel: cancel_rx,
            held: Vec::with_capacity(CANCEL_CAPACITY),
            next_seq: 0,
            pending: Vec::with_capacity(COMMAND_CAPACITY),
            due: Vec::with_capacity(COMMAND_CAPACITY),
            due_events: Vec::with_capacity(COMMAND_CAPACITY),
            due_params: Vec::with_capacity(COMMAND_CAPACITY),
            due_param_events: Vec::with_capacity(COMMAND_CAPACITY),
            done,
            late: 0,
            unrouted: 0,
            cancelled: 0,
            crossings: BeatCrossings::new(),
        },
    )
}

impl CommandTx {
    /// Commands sent and not yet finished by the executor.
    pub(crate) fn outstanding(&self) -> u64 {
        self.sent - self.done.load(Ordering::Acquire)
    }

    /// Send one command, already checked by the caller, stamped with the
    /// commit it was checked against.
    pub(crate) fn send(
        &mut self,
        plan_seq: u64,
        at: At,
        to: EventInputPort,
        payload: Scheduled,
    ) -> Result<CommandId, ScheduleError> {
        if self.outstanding() >= COMMAND_CAPACITY as u64 {
            return Err(ScheduleError::Backpressure);
        }
        let pos = match at {
            At::Frame(f) => f.get() as f64,
            _ => 0.0,
        };
        let cmd = Command {
            seq: self.sent,
            plan_seq,
            at,
            to,
            payload,
            pos,
        };
        // Full cannot happen: at most `outstanding` commands sit in the ring,
        // and that is below its capacity. With the executor gone the command
        // is dropped and counts as sent all the same, so the credits run out
        // as they would behind a stalled one.
        if let Err(refused) = self.tx.push(cmd) {
            assert_eq!(
                refused.error,
                PushError::Disconnected,
                "the command ring has a free slot for every credit"
            );
        }
        self.sent += 1;
        Ok(CommandId {
            pair: self.pair,
            seq: cmd.seq,
        })
    }

    /// Cancel `id` if it has not landed yet (a no-op if it has). Refuses an
    /// id this pair never issued — another pair's, or one past the last it
    /// sent — which the executor would otherwise hold for a command that
    /// never comes, or match against an unrelated one of its own.
    pub(crate) fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        if id.pair != self.pair || id.seq >= self.sent {
            return Err(ScheduleError::UnknownCommand { id });
        }
        self.cancel
            .push(Cancel::One(id.seq))
            .map_err(|_| ScheduleError::Backpressure)
    }

    /// Cancel every command scheduled so far that has not landed.
    pub(crate) fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        self.cancel
            .push(Cancel::Below(self.sent))
            .map_err(|_| ScheduleError::Backpressure)
    }
}

impl CommandRx {
    /// Commands that landed late (at offset 0 of the first block that saw
    /// them past due).
    pub(crate) fn late(&self) -> u64 {
        self.late
    }

    /// Commands whose node or port was gone when they fell due.
    pub(crate) fn unrouted(&self) -> u64 {
        self.unrouted
    }

    /// Commands cancelled before they landed.
    pub(crate) fn cancelled(&self) -> u64 {
        self.cancelled
    }

    /// Pull new commands, then apply cancellations. Frees the credit of what
    /// it cancels. Audio thread: no allocation — `Vec::remove`/`retain`/
    /// `swap_remove` shift in place, and `held` never passes its capacity.
    ///
    /// **A cancel can arrive before the command it names.** With the editor
    /// on another thread, the executor can find the command ring empty, the
    /// editor then push command N and its cancel, and the executor pop the
    /// cancel. So a cancel naming a command not pulled yet (`seq >=
    /// next_seq`) is *held* and re-applied after every pull until the
    /// command has arrived; a `Below(n)` is held until every `seq < n` has.
    fn pull(&mut self) {
        while let Some(cmd) = self.rx.pop() {
            debug_assert!(self.pending.len() < self.pending.capacity());
            self.next_seq = cmd.seq + 1;
            self.pending.push(cmd);
        }
        let before = self.pending.len();
        let mut i = 0;
        while i < self.held.len() {
            if self.apply_cancel(self.held[i]) {
                self.held.swap_remove(i);
            } else {
                i += 1;
            }
        }
        // Past the held list's capacity, cancels wait in their ring.
        while self.held.len() < self.held.capacity() {
            let Some(c) = self.cancel.pop() else {
                break;
            };
            if !self.apply_cancel(c) {
                self.held.push(c);
            }
        }
        let gone = (before - self.pending.len()) as u64;
        if gone > 0 {
            self.cancelled += gone;
            self.done.fetch_add(gone, Ordering::Release);
        }
    }

    /// Apply `c` to what has been pulled. Returns whether it is finished
    /// with — false while a command it names may still be on its way.
    fn apply_cancel(&mut self, c: Cancel) -> bool {
        match c {
            Cancel::One(id) => {
                if id >= self.next_seq {
                    return false;
                }
                if let Some(i) = self.pending.iter().position(|p| p.seq == id) {
                    self.pending.remove(i);
                }
                true
            }
            Cancel::Below(below) => {
                self.pending.retain(|p| p.seq >= below);
                below <= self.next_seq
            }
        }
    }

    /// Record a block the executor rendered without resolving commands (a
    /// suspended one): the transport moved, and the crossing tracker must know.
    pub(crate) fn observe(&mut self, env: &Env) {
        self.crossings.observe(env);
    }

    /// The sample rate changed by `ratio` (new / old): every pending
    /// `At::Frame` scheduled before commit `before` — in frames at the old
    /// rate — moves to the same wall-clock time at the new one, rounded to
    /// the nearest frame. Commands scheduled from `before` on already speak
    /// the new rate.
    pub(crate) fn rescale(&mut self, ratio: f64, before: u64) {
        self.pull();
        for cmd in &mut self.pending {
            if cmd.plan_seq < before
                && let At::Frame(_) = cmd.at
            {
                cmd.pos *= ratio;
                cmd.at = At::Frame(tutti_units::Frame::nearest(cmd.pos));
            }
        }
    }

    /// Pull what the editor sent and work out what falls in the block `env`
    /// describes, under `plan` (applied up to commit `applied`). Marks each
    /// plan unit with a delivery in `has_due`. Audio thread: never
    /// allocates — every list was sized to [`COMMAND_CAPACITY`], which the
    /// credit count keeps them under.
    pub(crate) fn gather(&mut self, env: &Env, plan: &Plan, applied: u64, has_due: &mut [bool]) {
        self.pull();
        self.crossings.observe(env);
        self.due.clear();
        self.due_events.clear();
        self.due_params.clear();
        self.due_param_events.clear();
        if self.pending.is_empty() {
            return;
        }
        let (due, due_params) = (&mut self.due, &mut self.due_params);
        let (late, unrouted) = (&mut self.late, &mut self.unrouted);
        let crossings = &self.crossings;
        let mut finished = 0u64;
        // `retain` keeps arrival order and shifts in place: no allocation.
        self.pending.retain_mut(|cmd| {
            // Its commit is still on the way. Only a concurrent editor can
            // make that happen (`process` applies every visible commit before
            // it pulls commands); `exec::tests` stages it deterministically.
            if cmd.plan_seq > applied {
                return true;
            }
            let node = plan
                .units
                .binary_search_by_key(&cmd.to.node, |u| u.key)
                .ok();
            // PDC: the sink hears timeline frame F at its own F + arrival
            // (see `Env::due_at_arrival`). The node's arrival even when its
            // port is gone (the reference does the same); none when the node
            // is — either way it is counted unrouted when its time comes.
            let arrival = node.map_or(Latency::ZERO, |u| plan.units[u].arrival);
            // A param command lands only on a node that declares param
            // events: the node a commit replaced may not.
            let target = node.filter(|&u| {
                let shape = &plan.units[u].shape;
                cmd.to.port < shape.event_in
                    && (shape.param_events || matches!(cmd.payload, Scheduled::Event(_)))
            });
            let was_beat = matches!(cmd.at, At::Beat(_));
            let resolved = env.due_at_arrival(&mut cmd.at, arrival, crossings);
            match cmd.at {
                At::Frame(f) if was_beat => cmd.pos = f.get() as f64,
                At::NextBlock => cmd.pos = env.frame.get() as f64,
                _ => {}
            }
            let offset = match resolved {
                Due::NotYet => return true,
                Due::In(o) => o,
                Due::Late => {
                    *late += 1;
                    Offset::ZERO
                }
            };
            finished += 1;
            match (target, cmd.payload) {
                (Some(u), Scheduled::Event(kind)) => {
                    has_due[u] = true;
                    due.push(DueItem::of(cmd, u, offset, kind));
                }
                (Some(u), Scheduled::Param(kind)) => {
                    has_due[u] = true;
                    due_params.push(DueItem::of(cmd, u, offset, kind));
                }
                (None, _) => *unrouted += 1,
            }
            false
        });
        if finished > 0 {
            self.done.fetch_add(finished, Ordering::Release);
        }
        // Ties at one offset go by timeline position, then scheduling order.
        self.due.sort_unstable_by_key(DueItem::order);
        self.due_params.sort_unstable_by_key(DueItem::order);
        self.due_events.extend(self.due.iter().map(|d| Event {
            offset: d.offset,
            kind: d.kind,
        }));
        self.due_param_events
            .extend(self.due_params.iter().map(|d| ParamEvent {
                offset: d.offset,
                kind: d.kind,
            }));
    }

    /// This block's deliveries, as a `Sync` view the executor's workers
    /// share.
    pub(crate) fn deliveries(&self) -> Deliveries<'_> {
        Deliveries {
            due: &self.due,
            events: &self.due_events,
            due_params: &self.due_params,
            param_events: &self.due_param_events,
        }
    }
}

/// This block's scheduled deliveries, sorted by unit and port: what
/// [`overlay`](Self::overlay) merges into a node's event inputs. Plain
/// slices, so every worker of a parallel block can read them at once.
#[derive(Clone, Copy)]
pub(crate) struct Deliveries<'a> {
    due: &'a [DueItem<EventKind>],
    events: &'a [Event],
    due_params: &'a [DueItem<ParamKind>],
    param_events: &'a [ParamEvent],
}

impl Deliveries<'_> {
    /// Merge this block's deliveries for plan unit `unit` into `buf`, each
    /// port's after `base(port)` (the port's events as the executor would
    /// otherwise hand them), and record `(port, range in buf)` in `views`.
    /// Returns how many views were written. Never grows `buf`: past its
    /// capacity (which the executor sizes so it cannot be reached) events
    /// are counted in the second return value instead.
    pub(crate) fn overlay<'e>(
        &self,
        unit: u32,
        base: impl Fn(u16) -> &'e [Event],
        buf: &mut Vec<Event>,
        views: &mut [(u16, u32, u32)],
    ) -> (usize, u32) {
        buf.clear();
        let start = self.due.partition_point(|d| d.unit < unit);
        let end = self.due.partition_point(|d| d.unit <= unit);
        let mut n = 0;
        let mut dropped = 0;
        let mut i = start;
        while i < end {
            let port = self.due[i].port;
            let run = self.due[i..end].partition_point(|d| d.port == port) + i;
            let from = buf.len() as u32;
            let room = buf.capacity();
            dropped += merge_into(&[base(port), &self.events[i..run]], buf, room);
            views[n] = (port, from, buf.len() as u32);
            n += 1;
            i = run;
        }
        (n, dropped)
    }
}

impl Deliveries<'_> {
    /// Merge this block's scheduled param events for plan unit `unit` into
    /// `buf`, each port's after `base(port)` (the port's param events as the
    /// executor would otherwise hand them), and record `(port, range in buf)`
    /// in `views`. Returns how many views were written, and how many param
    /// events did not fit: never grows `buf`.
    pub(crate) fn overlay_params<'e>(
        &self,
        unit: u32,
        base: impl Fn(u16) -> &'e [ParamEvent],
        buf: &mut Vec<ParamEvent>,
        views: &mut [(u16, u32, u32)],
    ) -> (usize, u32) {
        buf.clear();
        let start = self.due_params.partition_point(|d| d.unit < unit);
        let end = self.due_params.partition_point(|d| d.unit <= unit);
        let mut n = 0;
        let mut dropped = 0;
        let mut i = start;
        while i < end {
            let port = self.due_params[i].port;
            let run = self.due_params[i..end].partition_point(|d| d.port == port) + i;
            let from = buf.len() as u32;
            let room = buf.capacity();
            dropped += merge_params(&[base(port), &self.param_events[i..run]], buf, room);
            views[n] = (port, from, buf.len() as u32);
            n += 1;
            i = run;
        }
        (n, dropped)
    }
}

impl Deliveries<'_> {
    /// A node's whole event input for the block under a declared bound of
    /// `bound` events, all ports together: every port's `base(port)` with its
    /// scheduled deliveries after it, cut to the earliest `bound` by
    /// `(offset, port)` with note-offs first (`merge_walk`). Returns `None`
    /// when everything fits. Otherwise every port's events are written to
    /// `buf` and its `(port, range)` recorded in `views`, and the result is
    /// how many views and how many events were cut. Never grows `buf`.
    pub(crate) fn overlay_bounded<'e>(
        &self,
        unit: u32,
        ports: usize,
        bound: usize,
        base: impl Fn(u16) -> &'e [Event],
        buf: &mut Vec<Event>,
        views: &mut [(u16, u32, u32)],
    ) -> Option<(usize, u32)> {
        let start = self.due.partition_point(|d| d.unit < unit);
        let end = self.due.partition_point(|d| d.unit <= unit);
        let mine = &self.due[start..end];
        let mut sources: [&[Event]; 2 * MAX_PORTS] = [&[]; 2 * MAX_PORTS];
        let mut total = 0;
        for p in 0..ports {
            let port = p as u16;
            let from = start + mine.partition_point(|d| d.port < port);
            let to = start + mine.partition_point(|d| d.port <= port);
            sources[2 * p] = base(port);
            sources[2 * p + 1] = &self.events[from..to];
            total += sources[2 * p].len() + sources[2 * p + 1].len();
        }
        if total <= bound {
            return None;
        }
        let sources = &sources[..2 * ports];
        buf.clear();
        let room = bound.min(buf.capacity());
        let mut dropped = 0;
        // The walk is the same for every port: each pass keeps its own.
        for (p, view) in views.iter_mut().enumerate().take(ports) {
            let from = buf.len() as u32;
            dropped = merge_walk(sources, room, |i, e| {
                if i / 2 == p {
                    buf.push(e);
                }
            });
            *view = (p as u16, from, buf.len() as u32);
        }
        Some((ports, dropped))
    }
}

/// Where a node's scheduled events and param events are merged into its
/// inputs: preallocated, and never grown.
pub(crate) struct Overlay {
    pub(crate) events: Vec<Event>,
    pub(crate) params: Vec<ParamEvent>,
}

impl Overlay {
    /// Room for `capacity` of each ([`overlay_capacity`]). Allocates.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            params: Vec::with_capacity(capacity),
        }
    }

    /// The room of each list.
    pub(crate) fn capacity(&self) -> usize {
        self.events.capacity()
    }
}

/// Capacity of the executor's overlay buffer for `plan`: enough for the
/// widest node's event inputs at full slots, every flushed event, and every
/// outstanding command — so [`Deliveries::overlay`] can never be short.
pub(crate) fn overlay_capacity(plan: &Plan, cap: usize, flushed: usize) -> usize {
    let widest = plan
        .ops
        .iter()
        .filter_map(|op| match *op {
            crate::plan::Op::Node { event_in, .. } => Some(
                plan.event_list[event_in.range()]
                    .iter()
                    .map(|&s| plan.event_slot_capacity[s as usize].events(cap))
                    .sum::<usize>(),
            ),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    widest + flushed + COMMAND_CAPACITY
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeKey;
    use tutti_ump::Ump;
    use tutti_units::Frame;

    #[test]
    fn only_a_missing_plan_or_a_full_queue_is_transient() {
        let to = EventInputPort {
            node: NodeKey(1),
            port: 0,
        };
        assert!(ScheduleError::NoPlan.is_transient());
        assert!(ScheduleError::Backpressure.is_transient());
        assert!(!ScheduleError::NoSuchPort { to }.is_transient());
        assert!(!ScheduleError::NoParamEvents { to }.is_transient());
        assert!(!ScheduleError::Poisoned.is_transient());
    }

    fn send(tx: &mut CommandTx, frame: u64) -> CommandId {
        tx.send(
            0,
            At::Frame(Frame::new(frame)),
            EventInputPort {
                node: NodeKey(1),
                port: 0,
            },
            Scheduled::Event(EventKind::Midi(Ump([0; 4]))),
        )
        .expect("room")
    }

    /// An id this pair never issued — a number past the last it sent — is
    /// refused, not queued for a command that never comes.
    #[test]
    fn a_never_issued_id_is_refused() {
        let (mut tx, _rx) = command_channel();
        let issued = send(&mut tx, 1_000);
        let unissued = CommandId {
            pair: issued.pair,
            seq: 1,
        };
        assert_eq!(
            tx.cancel(unissued),
            Err(ScheduleError::UnknownCommand { id: unissued })
        );
        assert_eq!(tx.cancel(issued), Ok(()));
    }

    /// The lost-cancel race, staged: with the editor on another thread the
    /// executor can see a cancel before the command it names (it drained the
    /// command ring just before the editor pushed both). Staged here by
    /// pushing the cancel into its ring first. The cancel must be held and
    /// applied when the command arrives — for `One` and for `Below`.
    #[test]
    fn a_cancel_seen_before_its_command_is_held_until_it_arrives() {
        let (mut tx, mut rx) = command_channel();
        // `One`: the cancel for seq 0 is visible first.
        tx.cancel.push(Cancel::One(0)).expect("room");
        rx.pull();
        assert_eq!(rx.held.len(), 1, "held, not dropped");
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty(), "applied when the command arrived");
        assert_eq!((rx.cancelled(), tx.outstanding()), (1, 0));
        assert!(rx.held.is_empty());

        // `Below`: covers seq 1 and 2, which arrive after it.
        tx.cancel.push(Cancel::Below(3)).expect("room");
        rx.pull();
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty());
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty(), "the second one too");
        assert!(rx.held.is_empty(), "released once seq 2 was seen");
        send(&mut tx, 1_000_000); // seq 3: not covered
        rx.pull();
        assert_eq!(rx.pending.len(), 1);
        assert_eq!(rx.cancelled(), 3);
    }
}
