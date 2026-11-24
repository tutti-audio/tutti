//! Running a node op: its params, its mode and any crossfade, and the call
//! into the node.

use crate::compile::ids::AudioSlotId;
use std::cell::Cell;
use tutti_buffers::Float;

use tutti_buffers::MaxBlock;
use tutti_units::{Samples, Tail};

use super::state::{Inject, Unit};
use super::tail_elapsed;
use crate::compile::plan::{Direct, Form, NodePorts, NodeRec, NodeTables, Plan};
use crate::edges::rtlog::{LogSink, LogSlots};
use crate::event::merge::{merge_into, merge_params, merge_walk};
use crate::event::sorted::SortedEvents;
use crate::event::types::Event;
use crate::event::writer::EventWriter;
use crate::event::writer::PortCount;
use crate::exec::arena::Role;
use crate::exec::command::Deliveries;
use crate::exec::held::Held;
use crate::exec::history::TransportHistory;
use crate::exec::lane::fade_gains;
use crate::exec::loss::Dropped;
use crate::exec::slots::{AudioSlots, EventSlot, EventSlots, SlotFlags};
use crate::node::io::Io;
use crate::node::param::{CellValue, ParamInput};
use crate::node::param_event::ParamEvent;
use crate::node::time::Env;
use crate::node::{ConstantMask, Cx, InPlaceMask, MAX_PORTS, Node, SilenceMask, Status};
use tutti_music::MeterMap;
use tutti_tempo::TempoMap;

/// [`Op::Node`](crate::Op::Node): run `u` for `h.rec`: the plain op for a unit whose mode asks
/// nothing of it, else the one that applies its mode. Returns whether a
/// crossfade running there reached its end this block.
#[inline(always)]
pub(crate) fn node_dispatch<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
) -> bool {
    if u.mode.is_plain() {
        return run_node_op(u, head, st);
    }
    super::mode_op::moded_node_op(u, head, st)
}

/// The node op by its borrow form (a crossfade running there takes its own).
/// Returns whether that crossfade reached its end this block.
#[inline(always)]
pub(super) fn run_node_op<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
) -> bool {
    let (rec, plan, frames) = (head.rec, head.plan, head.frames);
    debug_assert_eq!(
        u.generation, rec.generation,
        "unit generation matches the plan"
    );
    let nodes = &plan.nodes;
    if u.fade.is_some() {
        // A crossfade runs here: both units, then the blend. Off the hot
        // path, whatever the form.
        let ports = rec.ports(nodes);
        return fading_node_op(u, head, st, ports, nodes);
    }
    // Each arm hands `node_op` its port slots. For the three direct forms
    // they are literal one-element arrays, so once `node_op` is inlined every
    // per-port loop in it — masks, skip, finish — folds to straight-line
    // code.
    match rec.form {
        Form::Source { out } => {
            node_op(
                u,
                head,
                st,
                NodePorts::audio(&[], &[out]),
                |call, node, st| {
                    let mut outs = [st.arena.slot_mut(out, frames)];
                    call.process(node, &[], &mut outs)
                },
            );
        }
        Form::Split { input, out } => node_op(
            u,
            head,
            st,
            NodePorts::audio(&[input], &[out]),
            |call, node, st| {
                let (i, o) = st.arena.pair(input, out, frames);
                call.process(node, &[i], &mut [o])
            },
        ),
        Form::InPlace { slot } => node_op(
            u,
            head,
            st,
            NodePorts::audio(&[slot], &[slot]),
            |call, node, st| {
                let mut outs = [st.arena.slot_mut(slot, frames)];
                call.process(node, &[&[]], &mut outs)
            },
        ),
        // The stereo shapes: fixed widths, so `direct_op` gets the same
        // straight-line call path.
        Form::Direct(d) => match (d.ins, d.outs) {
            (0, 2) => direct_op::<0, 2, F, A, Fl, E>(u, head, st, &d, nodes),
            (1, 2) => direct_op::<1, 2, F, A, Fl, E>(u, head, st, &d, nodes),
            (2, 1) => direct_op::<2, 1, F, A, Fl, E>(u, head, st, &d, nodes),
            (2, 2) => direct_op::<2, 2, F, A, Fl, E>(u, head, st, &d, nodes),
            _ => unreachable!("a direct record has one of `Direct::SHAPES`"),
        },
        // Port tables are stack arrays; pick the smallest bucket that fits
        // so a two-port node does not initialise 64 entries per call. An
        // event-free node builds no event table at all, whatever its width.
        Form::Audio => {
            let ports = rec.ports(nodes);
            let borrows = &nodes.audio_borrows[..];
            node_op(u, head, st, ports, |call, node, st| {
                match rec.ain.max(rec.aout) {
                    0..=4 => call.run_audio::<4, A>(node, &mut *st.arena, borrows),
                    5..=16 => call.run_audio::<16, A>(node, &mut *st.arena, borrows),
                    _ => call.run_audio::<MAX_PORTS, A>(node, &mut *st.arena, borrows),
                }
            });
        }
        Form::General => {
            let ports = rec.ports(nodes);
            node_op(u, head, st, ports, |call, node, st| {
                let a = rec.ain.max(rec.aout);
                let e = rec.ein.max(rec.eout);
                let (arena, events) = (&mut *st.arena, &mut *st.events);
                let dropped = &mut *st.dropped;
                match (a, e) {
                    (0..=4, 0..=4) => call.run::<4, 4, A, E>(node, arena, events, nodes, dropped),
                    (0..=16, 0..=4) => call.run::<16, 4, A, E>(node, arena, events, nodes, dropped),
                    _ => {
                        call.run::<MAX_PORTS, MAX_PORTS, A, E>(node, arena, events, nodes, dropped)
                    }
                }
            });
        }
    }
    false
}

/// A node op with a crossfade running: the outgoing unit on the op's inputs
/// into its scratch, then the incoming unit as any op runs (events and all,
/// through the general borrow), then the blend into the op's output slots.
/// Returns whether the fade reached its end in this block.
///
/// The outgoing unit runs **first**: an input aliased in place is also the
/// incoming unit's output, and holds the input only until that unit runs.
/// It is handed its own output buffers (never in place), no events, and
/// event writers that accept nothing — see the `fade` module (`src/model/fade.rs`).
/// Only the op's own slots are touched, so the verifier's rules about the op
/// cover the fade too.
#[cold]
#[inline(never)]
pub(super) fn fading_node_op<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    ports @ NodePorts {
        ain,
        aout,
        ein,
        eout,
    }: NodePorts<'_>,
    nodes: &NodeTables,
) -> bool {
    let frames = h.frames;
    // The key's params, once for both units: the outgoing one hears the
    // modulation the incoming one does.
    let modulated = !h.rec.params.is_empty() || u.params.busy();
    if modulated {
        run_params(u, h, st);
    }
    let params = u.params.inputs(frames);
    let params = &params[..u.params.port_count()];
    let x = u.fade.as_mut().expect("a fading unit");
    let status = {
        let (silent, constant) = in_masks(ain, &*st.flags);
        let mut ins: [&[F]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (i, &s) in ins.iter_mut().zip(ain) {
            *i = st.arena.slot(s, frames);
        }
        let mut outs: [&mut [F]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        x.scratch.leading_mut(frames, &mut outs[..aout.len()]);
        for o in &mut outs[..aout.len()] {
            o.fill(F::ZERO);
        }
        let evin = [SortedEvents::EMPTY; MAX_PORTS];
        let mut evout: [EventWriter<'_>; MAX_PORTS] =
            std::array::from_fn(|_| EventWriter::detached());
        let io = Io::new(
            h.max,
            frames,
            &ins[..ain.len()],
            &mut outs[..aout.len()],
            silent,
            constant,
            InPlaceMask::NONE,
            &evin[..ein.len()],
            &mut evout[..eout.len()],
        )
        .with_main(h.rec.main)
        .with_wired(h.rec.wired)
        .params(params);
        let cx = Cx {
            env: h.env,
            arrival: h.rec.arrival,
            history: h.history,
            meter: h.meter,
            tempo: h.tempo,
        };
        x.out
            .as_mut()
            .expect("a running fade holds its outgoing unit")
            .process(&cx, io)
    };
    // The outgoing unit's status, applied to its scratch as `finish` applies
    // one to the arena.
    for c in 0..aout.len() {
        let o = x.scratch.channel_mut(c, frames);
        match status {
            Status::Modified | Status::Masked { .. } => {}
            Status::Silent | Status::Idle => o.fill(F::ZERO),
            Status::Constant => {
                let v = o[0];
                o.fill(v);
            }
            Status::Bypass => match ain.get(c) {
                Some(&i) => o.copy_from_slice(st.arena.slot(i, frames)),
                None => o.fill(F::ZERO),
            },
        }
    }

    node_op_with(u, h, st, ports, !modulated, |call, node, st| {
        let (arena, events) = (&mut *st.arena, &mut *st.events);
        let dropped = &mut *st.dropped;
        call.run::<MAX_PORTS, MAX_PORTS, A, E>(node, arena, events, nodes, dropped)
    });

    // The blend: `outgoing * g_out + incoming * g_in` while the fade lasts,
    // the incoming unit alone after it (`FadeLaw::gains`).
    let x = u.fade.as_mut().expect("still fading");
    let n = (x.len - x.done).min(frames);
    for (c, &s) in aout.iter().enumerate() {
        let y = st.arena.slot_mut(s, frames);
        let out = x.scratch.channel(c, frames);
        for (i, (y, &o)) in y[..n].iter_mut().zip(&out[..n]).enumerate() {
            let (g_out, g_in) =
                fade_gains::<F>(x.law, Samples::new(x.done + i), Samples::new(x.len));
            *y = *y * g_in + o * g_out;
        }
        st.flags.set(s, Flags::NONE);
    }
    x.done += n;
    // Never skipped while fading: the next block's skip test reads these.
    u.last_quiet = false;
    u.last_idle = false;
    x.done >= x.len
}

/// Per audio slot: every sample of the block is exact `0.0` (silent), and the
/// block holds one value throughout (constant). One byte, so a node's input
/// masks are built with a load and two shifts per channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Flags(u8);

impl Flags {
    const SILENT: u8 = 1;
    const CONSTANT: u8 = 2;

    /// Nothing is known about the block.
    pub(crate) const NONE: Self = Self(0);
    /// Exact silence, which is constant.
    pub(crate) const SILENCE: Self = Self(Self::SILENT | Self::CONSTANT);

    #[inline]
    pub(crate) const fn new(silent: bool, constant: bool) -> Self {
        Self((silent as u8 * Self::SILENT) | (constant as u8 * Self::CONSTANT))
    }

    /// Every sample of the block is exact `0.0`.
    #[inline]
    pub(crate) const fn is_silent(self) -> bool {
        self.0 & Self::SILENT != 0
    }

    /// The block holds one value throughout.
    #[inline]
    pub(crate) const fn is_constant(self) -> bool {
        self.0 & Self::CONSTANT != 0
    }

    /// The byte a shared atomic holds.
    #[inline]
    pub(crate) const fn to_byte(self) -> u8 {
        self.0
    }

    /// The flags a shared atomic's byte holds (`to_byte`'s inverse).
    #[inline]
    pub(crate) const fn from_byte(byte: u8) -> Self {
        Self(byte)
    }
}

/// A node's input masks from its input slots' flags: one load and two
/// shifts per channel. Channels past 63 are never set (the masks are 64
/// wide, and `compile` refuses a wider node anyway).
#[inline]
pub(super) fn in_masks<Fl: SlotFlags + ?Sized>(
    ain: &[AudioSlotId],
    flags: &Fl,
) -> (SilenceMask, ConstantMask) {
    let (mut silent, mut constant) = (0u64, 0u64);
    for (c, &s) in (0..64u32).zip(ain) {
        let f = flags.get(s);
        silent |= u64::from(f.is_silent()) << c;
        constant |= u64::from(f.is_constant()) << c;
    }
    (SilenceMask(silent), ConstantMask(constant))
}

/// A node op's constants: its record and the block's.
pub(crate) struct Head<'p, 'e> {
    pub(crate) rec: &'p NodeRec,
    /// The plan, for the param tables.
    pub(crate) plan: &'p Plan,
    pub(crate) frames: usize,
    pub(crate) max: MaxBlock,
    pub(crate) cap: usize,
    pub(crate) env: &'e Env,
    /// The executor's transport history, observed before any op runs this
    /// block: every participant reads the same one, and nobody writes it
    /// until the block is done.
    pub(crate) history: &'e TransportHistory,
    /// The project's meter for the block: lent by the host, read-only.
    pub(crate) meter: Option<&'e MeterMap>,
    /// The project's tempo map for the block: lent by the host, read-only.
    pub(crate) tempo: Option<&'e TempoMap>,
}

/// What a node op may write besides its own unit: every buffer through `A`
/// and `E` (the arena and event slots themselves on the calling thread alone,
/// a claimed view of them on a pool), and only its own unit's per-block flags.
pub(crate) struct OpState<'s, A: ?Sized, Fl: ?Sized, E: ?Sized> {
    pub(crate) arena: &'s mut A,
    pub(crate) flags: &'s mut Fl,
    pub(crate) events: &'s mut E,
    /// Flushed events waiting for their sinks: this unit's. Entries are
    /// matched by unit.
    pub(crate) inject: &'s mut [Inject],
    /// Whether flushed events wait for this unit. Read only for a node with
    /// event inputs, so an event-free call pays nothing.
    pub(crate) has_inject: &'s mut bool,
    /// Whether a scheduled command lands on this unit this block.
    pub(crate) has_due: &'s mut bool,
    pub(crate) due: Deliveries<'s>,
    pub(crate) dropped: &'s mut Dropped,
    /// The slots a node's [`Io::log`](crate::Io::log) fills, and where they
    /// are flushed after its call: this participant's own.
    pub(crate) log: &'s LogSlots,
    pub(crate) sink: &'s mut LogSink,
}

/// Run one node op: the silence skip, flushed-event injection, the call
/// itself (`call_node` borrows the buffers its [`Form`] names and runs the
/// node), and the status bookkeeping.
///
/// `#[inline(always)]` so each [`Form`] gets its own copy: the direct forms
/// pass literal one-element port arrays, and every per-port loop here then
/// folds to straight-line code. That, and not the borrow alone, is most of
/// what the direct forms save.
#[inline(always)]
pub(super) fn node_op<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    ports: NodePorts<'_>,
    call_node: impl FnOnce(&Call<'_, '_, F>, &mut dyn Node<F>, &mut OpState<'_, A, Fl, E>) -> Status,
) {
    node_op_with(u, h, st, ports, false, call_node);
}

/// Run a unit's fused param step (see the `param` module docs,
/// `src/node/param.rs`): each declared param, from its source this plan, into
/// the unit's param buffers. Off the hot path: only a unit with a driven
/// param, or one mid-declick, gets here.
#[inline(never)]
pub(super) fn run_params<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    h: &Head<'_, '_>,
    st: &OpState<'_, A, Fl, E>,
) {
    let (rec, frames, plan) = (h.rec, h.frames, h.plan);
    let Unit { node, params, .. } = u;
    params.begin();
    let ports = &plan.program.param_ports[rec.params.range()];
    let mut next = ports.iter().peekable();
    let declared = rec.declared;
    let declick = plan.prepare.declick();
    // The state was sized from the unit's shape; a fade's two units declare
    // the same params (checked where it is asked for), so the two agree.
    let n = declared.len().min(params.port_count());
    for (k, &param) in declared.as_slice().iter().enumerate().take(n) {
        let base = node.param_base(k).map(|b| F::from_unit(CellValue(b)));
        match next.next_if(|p| p.port as usize == k) {
            Some(p) => {
                let events = st.events.pev(p.slot);
                params.port(k, param, frames, declick, Some((p.from, events)), base);
            }
            None => params.port(k, param, frames, declick, None, base),
        }
    }
}

/// [`node_op`], with the unit's params already run this block when
/// `params_done` (a crossfade runs them once for both units).
#[inline(always)]
pub(super) fn node_op_with<
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    NodePorts {
        ain,
        aout,
        ein,
        eout,
    }: NodePorts<'_>,
    params_done: bool,
    call_node: impl FnOnce(&Call<'_, '_, F>, &mut dyn Node<F>, &mut OpState<'_, A, Fl, E>) -> Status,
) {
    let (rec, frames) = (h.rec, h.frames);
    let unit = rec.unit;
    // Flushed events and scheduled commands only ever go to an event input.
    let injected = !ein.is_empty() && *st.has_inject;
    // A bypassed node is sent no events but the closes of what it holds
    // (`mode.rs`): its scheduled commands are dropped.
    let discards = !ein.is_empty() && u.mode.discards_events();
    let scheduled = !ein.is_empty() && !discards && *st.has_due;
    // Whether a bypassed node still owes the close of something it holds,
    // before this block's views drain it.
    let owes_closes = discards && u.mode.holds_input();

    // Every event input is assembled once, into its view: the node, the
    // held-key tables, the quiet test and the input taps all read that.
    let arrived = if ein.is_empty() {
        false
    } else {
        let flow = Flow {
            discards,
            injected,
            scheduled,
        };
        let (held_in, inject, due) = (u.mode.held_in(), &mut *st.inject, st.due);
        let (events, dropped) = (&mut *st.events, &mut *st.dropped);
        let arrived = match rec.ein {
            0..=4 => assemble::<4, E>(h, flow, held_in, inject, due, events, dropped),
            5..=16 => assemble::<16, E>(h, flow, held_in, inject, due, events, dropped),
            _ => assemble::<MAX_PORTS, E>(h, flow, held_in, inject, due, events, dropped),
        };
        // What was flushed and what was scheduled is in the views now.
        if injected {
            let mut owing = false;
            for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
                inj.consumed();
                owing |= inj.live;
            }
            *st.has_inject = owing;
        }
        *st.has_due = false;
        arrived
    };

    let (in_silent, in_constant) = in_masks(ain, &*st.flags);
    // A notified `Wake` counts as input. Taken for every node every block, so a
    // notify on a node that is not parked is consumed and cannot wake it later.
    let woken = u.node.take_wake();
    // Nothing is sent: the views are empty, and a bypassed node discarded
    // nothing.
    let quiet_inputs = !woken
        && !injected
        && !scheduled
        && !(discards && arrived)
        && in_silent.covers(ain.len())
        && ein
            .iter()
            .all(|&s| st.events.ev(s).is_empty() && st.events.pev(s).is_empty());

    // See the module docs: a node with event inputs parks only on its own
    // say-so (`Status::Idle`); one without parks when its inputs, its last
    // output and its latency plus tail agree.
    let skip = quiet_inputs
        && if ein.is_empty() {
            !ain.is_empty()
                && u.last_quiet
                && tail_elapsed(
                    match u.reported {
                        None => rec.park_after,
                        Some(t) => Tail::Finite(rec.latency).then(t),
                    },
                    u.quiet,
                )
        } else {
            u.last_idle
        };
    // A bypassed node is called until it has been sent the close of what
    // it holds, whatever it says of itself.
    let skip = skip && !owes_closes;
    u.quiet = if quiet_inputs {
        u.quiet + Samples::new(frames)
    } else {
        Samples::ZERO
    };
    // The fused param step: only for a unit with a modulated param, or one
    // still declicking off one; every other unit pays this one branch. Run
    // whether or not the node is then skipped, so its ramps and declick
    // advance with the timeline.
    if !params_done && (!rec.params.is_empty() || u.params.busy()) {
        run_params(u, h, st);
    }
    if skip {
        for &s in aout {
            st.arena.slot_mut(s, frames).fill(F::ZERO);
            st.flags.set(s, Flags::SILENCE);
        }
        for &s in eout {
            st.events.slot_mut(s).clear();
        }
        return;
    }

    // The table only for a unit whose step delivered frames this block;
    // every other node reads `Base` for every param (`Io::param`).
    let table;
    let params: &[ParamInput<'_, F>] = if u.params.busy() {
        table = u.params.inputs(frames);
        &table[..u.params.port_count()]
    } else {
        &[]
    };
    let reported = Cell::new(None);
    let log = st.log;
    let call = Call {
        reported: &reported,
        log,
        env: h.env,
        history: h.history,
        meter: h.meter,
        tempo: h.tempo,
        max: h.max,
        frames,
        rec,
        silent: in_silent,
        constant: in_constant,
        cap: rec.event_capacity.map_or(h.cap, |n| n.get() as usize),
        params,
    };
    let status = call_node(&call, &mut *u.node, st);
    log.flush(h.plan.units[unit].key, h.env.frame, &mut *st.sink);
    // This call's report, or the ceiling when it made none.
    u.reported = reported
        .get()
        .and_then(|r| crate::node::narrow_tail(rec.tail, r));

    u.last_idle = status == Status::Idle;
    finish(
        status,
        frames,
        ain,
        aout,
        rec.in_place,
        &mut *st.arena,
        st.flags,
    );
    // A node with no outputs claims nothing by being "all silent": it is a
    // sink, called for its side effects, and a skip would starve them.
    u.last_quiet = !(aout.is_empty() && eout.is_empty())
        && aout.iter().all(|&s| st.flags.get(s).is_silent())
        && eout
            .iter()
            .all(|&s| st.events.ev(s).is_empty() && st.events.pev(s).is_empty());
    if u.keys.is_closing() {
        // A replace took over from a unit that held keys: close them ahead
        // of this block's events, as many as the slot takes; the rest next
        // block.
        for (port, &s) in eout.iter().enumerate() {
            u.keys.close_port(port, st.events.ev_mut(s));
        }
        u.keys.settle();
    }
    for (port, &s) in eout.iter().enumerate() {
        u.keys.observe(port, st.events.ev(s));
    }
}

/// A [`Form::Direct`] node op with `I` inputs and `O` outputs: its port
/// slots as fixed-size arrays, so `node_op`'s per-port loops fold as they do
/// for the one-channel forms, and its buffers borrowed by
/// [`Arena::direct`](crate::exec::arena::Arena::direct).
#[inline(always)]
pub(super) fn direct_op<
    const I: usize,
    const O: usize,
    F: Float,
    A: AudioSlots<F>,
    Fl: SlotFlags + ?Sized,
    E: EventSlots + ?Sized,
>(
    u: &mut Unit<F>,
    h: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    d: &Direct,
    nodes: &NodeTables,
) {
    let NodePorts { ain, aout, .. } = h.rec.ports(nodes);
    let ain: &[AudioSlotId; I] = ain.try_into().expect("a direct record's input count");
    let aout: &[AudioSlotId; O] = aout.try_into().expect("a direct record's output count");
    let frames = h.frames;
    node_op(u, h, st, NodePorts::audio(ain, aout), |call, node, st| {
        let (ins, mut outs) = st.arena.direct::<I, O>(frames, d);
        call.process(node, &ins, &mut outs)
    });
}

/// What reaches a node's event inputs this block, besides its source slots.
#[derive(Clone, Copy)]
struct Flow {
    /// The node is bypassed: it is handed only the closes of what it holds.
    discards: bool,
    /// Events were flushed to it.
    injected: bool,
    /// Commands landed on it.
    scheduled: bool,
}

/// Fill the views of `h.rec`'s event inputs: what each input is handed this
/// block. A bypassed node (`discards`) is handed only the closes of what it
/// holds. Any other node is handed, per port and merged by `(offset, source
/// order)` with ties to the older: the events flushed to it, the port's
/// source slot, and the commands scheduled to it; under the node's declared
/// input bound they are cut together over all ports, the earliest by frame,
/// note-offs first. What a cut or a short view refuses is charged to the
/// node. The held-key tables observe the result.
///
/// `N` must hold the node's event inputs. Returns whether any source slot held
/// an event or a param event this block.
#[inline(never)]
fn assemble<const N: usize, E: EventSlots + ?Sized>(
    h: &Head<'_, '_>,
    flow: Flow,
    held_in: &mut [Held],
    inject: &mut [Inject],
    due: Deliveries<'_>,
    events: &mut E,
    dropped: &mut Dropped,
) -> bool {
    let rec = h.rec;
    let (unit, frames, ports) = (rec.unit, h.frames, usize::from(rec.ein));
    debug_assert!(ports <= N);
    let bound = h.plan.units[unit].shape.event_input_capacity;

    let mut sourced: [&[Event]; N] = [&[]; N];
    let mut sourced_params: [&[ParamEvent]; N] = [&[]; N];
    let mut views: [Option<&mut EventSlot>; N] = std::array::from_fn(|_| None);
    events.borrow_sorted(
        &h.plan.nodes.event_assemble[rec.event_assemble.range()],
        |port, v| {
            sourced[usize::from(port)] = &v.events[..];
            sourced_params[usize::from(port)] = &v.params[..];
        },
        |port, v| views[usize::from(port)] = Some(v),
    );
    let arrived = (0..ports).any(|p| !sourced[p].is_empty() || !sourced_params[p].is_empty());
    for view in views[..ports].iter_mut().flatten() {
        view.clear();
    }
    // Nothing reaches the node: its views are empty.
    if !arrived && !flow.discards && !flow.injected && !flow.scheduled {
        return false;
    }

    if flow.discards {
        let mut written = 0;
        for (view, held) in views.iter_mut().flatten().zip(held_in.iter_mut()) {
            // A node with an input bound is sent at most that many closes a
            // block, all its inputs together; the rest stays held for the
            // next block.
            let budget = bound.map_or(usize::MAX, |n| n.get() as usize - written);
            written += held.drain_upto(&mut view.events, budget);
        }
        return arrived;
    }

    // Flushed events keep their spacing where it fits in this block; what
    // does not is clamped to its last frame. Their closes drain in behind.
    let mut flushed: [&[Event]; N] = [&[]; N];
    if flow.injected {
        for inj in inject.iter_mut().filter(|i| i.live && i.unit == unit) {
            for e in inj.events.iter_mut() {
                e.offset = e.offset.clamp_to(frames);
            }
            inj.drain_owed();
        }
        for inj in inject.iter().filter(|i| i.live && i.unit == unit) {
            flushed[usize::from(inj.port)] = &inj.events[..];
        }
    }
    let mut commanded: [&[Event]; N] = [&[]; N];
    let mut commanded_params: [&[ParamEvent]; N] = [&[]; N];
    if flow.scheduled {
        for port in 0..ports {
            commanded[port] = due.events.port(unit, port as u16);
            commanded_params[port] = due.params.port(unit, port as u16);
        }
    }

    let sources = |port: usize| [flushed[port], sourced[port], commanded[port]];
    let total: usize = (0..ports)
        .map(|p| sources(p).iter().map(|s| s.len()).sum::<usize>())
        .sum();
    let mut lost = 0u32;
    match bound.filter(|b| total > b.get() as usize) {
        Some(bound) => {
            // The same walk for every port: each pass keeps its own.
            let mut all: [&[Event]; 3 * MAX_PORTS] = [&[]; 3 * MAX_PORTS];
            for port in 0..ports {
                all[3 * port..3 * port + 3].copy_from_slice(&sources(port));
            }
            for (port, view) in views.iter_mut().flatten().enumerate() {
                let room = (bound.get() as usize).min(view.events.capacity());
                lost = merge_walk(&all[..3 * ports], room, |i, e| {
                    if i / 3 == port {
                        view.events.push(e);
                    }
                });
            }
        }
        None => {
            for (port, view) in views.iter_mut().flatten().enumerate() {
                lost += merge_into(&sources(port), &mut view.events);
            }
        }
    }
    // Param events are never bounded by the input bound: the view holds
    // every one its source and the commands can bring.
    for (port, view) in views.iter_mut().flatten().enumerate() {
        if !sourced_params[port].is_empty() || !commanded_params[port].is_empty() {
            lost += merge_params(
                &[sourced_params[port], commanded_params[port]],
                &mut view.params,
            );
        }
    }
    if lost > 0 {
        dropped.at_node(unit, lost);
    }
    for (view, held) in views.iter().flatten().zip(held_in.iter_mut()) {
        held.observe(&view.events);
    }
    arrived
}

/// One node call's constants, shared by every borrow form.
pub(super) struct Call<'p, 'e, F: Float> {
    env: &'e Env,
    history: &'e TransportHistory,
    meter: Option<&'e MeterMap>,
    tempo: Option<&'e TempoMap>,
    max: MaxBlock,
    frames: usize,
    rec: &'p NodeRec,
    silent: SilenceMask,
    constant: ConstantMask,
    /// Events each of the node's event output writers accepts: its declared
    /// `Shape::event_capacity`, or the executor's default.
    cap: usize,
    /// What each declared param reads this block.
    params: &'p [ParamInput<'p, F>],
    /// Where the node's [`Io::report_tail`] lands.
    reported: &'p Cell<Option<Tail>>,
    /// Where the node's [`Io::log`] lands.
    log: &'p LogSlots,
}

impl<'p, F: Float> Call<'p, '_, F> {
    /// Run an event-free node on buffers already borrowed.
    #[inline]
    fn process<'a>(
        &self,
        node: &mut dyn Node<F>,
        ins: &'a [&'a [F]],
        outs: &'a mut [&'a mut [F]],
    ) -> Status
    where
        'p: 'a,
    {
        let io = Io::new(
            self.max,
            self.frames,
            ins,
            outs,
            self.silent,
            self.constant,
            self.rec.in_place,
            &[],
            &mut [],
        )
        .with_main(self.rec.main)
        .with_wired(self.rec.wired)
        .params(self.params)
        .tail(self.reported)
        .with_log(self.log);
        node.process(&self.cx(), io)
    }

    /// Built at the call, not kept in `Call`: the node takes it by
    /// reference, so whatever holds it is spilled to the stack.
    #[inline]
    fn cx(&self) -> Cx<'_> {
        Cx {
            env: self.env,
            arrival: self.rec.arrival,
            history: self.history,
            meter: self.meter,
            tempo: self.tempo,
        }
    }

    /// Borrow an event-free node's audio buffers by its presorted requests
    /// (`borrows` is the plan's whole list; the record names its run) and
    /// run it. `N` must hold the wider of its two sides.
    fn run_audio<const N: usize, A: AudioSlots<F>>(
        &self,
        node: &mut dyn Node<F>,
        arena: &mut A,
        borrows: &[(AudioSlotId, Role)],
    ) -> Status {
        let (n_in, n_out) = (self.rec.ain as usize, self.rec.aout as usize);
        debug_assert!(n_in <= N && n_out <= N);
        let mut ins: [&[F]; N] = [&[]; N];
        let mut outs: [&mut [F]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(
            self.frames,
            &borrows[self.rec.borrows.range()],
            &mut ins,
            &mut outs,
        );
        self.process(node, &ins[..n_in], &mut outs[..n_out])
    }

    /// Borrow a node's audio and event buffers and run it. `N` must hold the
    /// wider audio side and `M` the wider event side. Returns the node's
    /// status, and charges the events its writers refused to `dropped`, by
    /// port.
    fn run<const N: usize, const M: usize, A: AudioSlots<F>, E: EventSlots + ?Sized>(
        &self,
        node: &mut dyn Node<F>,
        arena: &mut A,
        events: &mut E,
        nodes: &NodeTables,
        dropped: &mut Dropped,
    ) -> Status {
        let rec = self.rec;
        let frames = self.frames;
        let (n_in, n_out) = (rec.ain as usize, rec.aout as usize);
        let (e_in, e_out) = (rec.ein as usize, rec.eout as usize);
        debug_assert!(n_in.max(n_out) <= N && e_in.max(e_out) <= M);

        let mut ins: [&[F]; N] = [&[]; N];
        let mut outs: [&mut [F]; N] = std::array::from_fn(|_| &mut [][..]);
        arena.borrow(
            frames,
            &nodes.audio_borrows[rec.borrows.range()],
            &mut ins,
            &mut outs,
        );

        let mut evin: [SortedEvents<'_>; M] = [SortedEvents::EMPTY; M];
        let mut evout_bufs: [Option<&mut EventSlot>; M] = std::array::from_fn(|_| None);
        events.borrow_sorted(
            &nodes.event_borrows[rec.event_borrows.range()],
            |port, v| {
                evin[port as usize] =
                    SortedEvents::trusted(&v.events, frames).trusted_params(&v.params, frames);
            },
            |port, v| evout_bufs[port as usize] = Some(v),
        );
        // One count per port, so a loss is charged to the port that had it.
        let drops: [PortCount; M] = std::array::from_fn(|_| PortCount::default());
        let mut evout: [EventWriter<'_>; M] = std::array::from_fn(|_| EventWriter::detached());
        for ((w, b), d) in evout
            .iter_mut()
            .zip(evout_bufs.iter_mut())
            .zip(&drops)
            .take(e_out)
        {
            let slot = b.take().expect("event output borrowed");
            slot.clear();
            let EventSlot { events, params } = slot;
            *w = EventWriter::new(events, params, self.cap, frames as u32, d);
        }
        let unplaced = Cell::new(0u32);
        let io = Io::new(
            self.max,
            frames,
            &ins[..n_in],
            &mut outs[..n_out],
            self.silent,
            self.constant,
            rec.in_place,
            &evin[..e_in],
            &mut evout[..e_out],
        )
        .with_main(rec.main)
        .with_wired(rec.wired)
        .params(self.params)
        .with_lost(&unplaced)
        .tail(self.reported)
        .with_log(self.log);
        let status = node.process(&self.cx(), io);
        if unplaced.get() != 0 {
            dropped.at_node(rec.unit, unplaced.get());
        }
        for (port, d) in drops.iter().take(e_out).enumerate() {
            let (refused, recovered) = (d.refused.get(), d.recovered.get());
            if refused != 0 || recovered != 0 {
                dropped.at_port(rec.unit, port as u16, refused, recovered);
            }
        }
        status
    }
}

/// Apply a node's [`Status`] to its output slots and their flags.
#[inline(always)]
pub(super) fn finish<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized>(
    status: Status,
    frames: usize,
    ain: &[AudioSlotId],
    aout: &[AudioSlotId],
    in_place: InPlaceMask,
    arena: &mut A,
    flags: &mut Fl,
) {
    match status {
        Status::Modified => {
            for &s in aout {
                flags.set(s, Flags::NONE);
            }
        }
        Status::Masked {
            silent: sm,
            constant: cm,
        } => {
            for (c, &s) in aout.iter().enumerate() {
                flags.set(s, Flags::new(sm.get(c), cm.get(c) || sm.get(c)));
            }
        }
        Status::Silent | Status::Idle => {
            for &s in aout {
                arena.slot_mut(s, frames).fill(F::ZERO);
                flags.set(s, Flags::SILENCE);
            }
        }
        Status::Constant => {
            for &s in aout {
                let buf = arena.slot_mut(s, frames);
                let v = buf[0];
                buf.fill(v);
                flags.set(
                    s,
                    Flags::new(
                        v == F::ZERO && Into::<f64>::into(v).is_sign_positive(),
                        true,
                    ),
                );
            }
        }
        Status::Bypass => {
            for (c, &s) in aout.iter().enumerate() {
                match ain.get(c) {
                    Some(_) if in_place.get(c) => {}
                    Some(&i) => {
                        arena.copy_slot(i, s);
                        flags.set(s, flags.get(i));
                    }
                    None => {
                        arena.slot_mut(s, frames).fill(F::ZERO);
                        flags.set(s, Flags::SILENCE);
                    }
                }
            }
        }
    }
}
