//! The node op of a unit whose mode asks something of it (`mode.rs`): feeding
//! its bypass line, running a fade between modes, muting, and parking.
//!
//! Written once, generic over the slots like every op, so a block alone and a block
//! spread over a pool run the same code; it touches only the op's own slots and
//! its own unit.

use super::*;
use crate::compile::ids::{AudioSlotId, EventSlotId, Id};
use crate::compile::plan::NodePorts;
use crate::model::mode::{MixState, NodeMode};

/// [`node_dispatch`] for a unit that is not plain steady Active: toggle if
/// the block's mode snapshot differs, then either park the node (the aligned
/// input path alone) or call it and blend its output with the aligned input.
/// Returns whether a crossfade running at the unit ended this block.
#[inline(never)]
pub(super) fn moded_node_op<
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
    let ports = rec.ports(&plan.nodes);
    let NodePorts { ain, aout, .. } = ports;

    // The mix's lane, before anything reads the block's mix (it moves while
    // the node is parked too, as the mix's snapshot does).
    let source = plan.param_ports[rec.params.range()]
        .last()
        .filter(|p| p.is_mix())
        .map(|p| (p.from, &st.events.pev(EventSlotId::new(p.slot))[..]));
    u.mode.drive_step(frames, source);

    let want = u.mode.seen();
    if want != u.mode.heading() && u.mode.toggle(want) {
        // Un-bypass: its state is stale. Reset before the first call.
        u.node.reset();
        u.quiet = Samples::ZERO;
        u.last_quiet = false;
        u.last_idle = false;
        u.reported = None;
    }

    if u.mode.is_parked() {
        park(u, head, st, ports);
        return false;
    }

    // The aligned input: fed through the line every block (the line is the
    // node's own history), and kept beside the node's output when this
    // block's blend needs it. Before the call: a node that works in place
    // overwrites its input.
    let main = u.mode.inputs();
    let need = u.mode.needs_input();
    let mut input_silent = 0u64;
    for (c, &s) in ain.iter().enumerate().take(main) {
        let silent = st.flags.get(s).is_silent();
        let src = st.arena.slot(s, frames);
        if u.mode.align(c, src, silent, need, frames) {
            input_silent |= 1 << c;
        }
    }

    let ended = run_node_op(u, head, st);

    let seg = u.mode.step(frames);
    if seg.is_steady() && seg.steady == NodeMode::Active && seg.mix.is_wet() {
        return ended;
    }
    if need {
        u.mode.fold_aligned(frames);
    }
    let lane = u.mode.lane(frames);
    let main = u.mode.outputs();
    for (c, &s) in aout.iter().enumerate() {
        let x = (c < main && need).then(|| u.mode.passed(c, frames));
        seg.blend(st.arena.slot_mut(s, frames), x, lane);
        if seg.is_steady() {
            match seg.steady {
                NodeMode::Active => {
                    // A blend off the ends: silent only where both of its
                    // terms are (a channel with no input has one term, and
                    // at DRY it is silence).
                    let node_silent = st.flags.get(s).is_silent();
                    let no_input = c >= main;
                    let x_silent = no_input || u.mode.passes_silence(c, input_silent);
                    match seg.mix.state() {
                        MixState::Wet => {}
                        MixState::Dry => st.flags.set(s, Flags::new(x_silent, x_silent)),
                        MixState::Between => {
                            let silent = node_silent && x_silent;
                            st.flags.set(s, Flags::new(silent, silent));
                        }
                        MixState::Moving => st.flags.set(s, Flags::NONE),
                    }
                }
                NodeMode::Muted => st.flags.set(s, Flags::SILENCE),
                NodeMode::Bypassed => {
                    let silent = c >= main || u.mode.passes_silence(c, input_silent);
                    st.flags.set(s, Flags::new(silent, silent));
                }
            }
        } else {
            st.flags.set(s, Flags::NONE);
        }
    }
    ended
}

/// A parked node: not processed, only polled for its handoffs. Its audio outputs are the aligned input (the
/// bypass line, or the input itself when the node has no latency), its event
/// outputs only what its held keys still owe, its events discarded.
fn park<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit<F>,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    NodePorts {
        ain,
        aout,
        ein,
        eout,
    }: NodePorts<'_>,
) {
    let rec = head.rec;
    // A parked node is not processed, so a wake raised meanwhile is consumed
    // here, as its discarded events are.
    u.node.take_wake();
    // The node's pending handoffs are answered (a plugin's recalled
    // Processor); it renders nothing and writes no port.
    u.node.poll_parked();
    // Its params go on stepping, as a skipped node's do: their ramps and
    // declick follow the timeline while it is out.
    if rec.params.len != 0 || u.params.busy() {
        run_params(u, head, st);
    }
    if !u.mode.is_stale() {
        // The first parked block: what the node held on its event outputs
        // is closed downstream, ahead of nothing (it writes no events now).
        u.mode.park();
        u.keys.owe_held();
    }

    // The outputs are the inputs delayed by the declared latency: output `c`
    // is input `c` (main buses of one width naming no placement), or the fold
    // of the main buses (`pass.rs`).
    if u.mode.folds() {
        park_folded(u, head, st, ain, aout);
    } else {
        park_by_index(u, head, st, ain, aout);
    }

    for (port, &s) in eout.iter().enumerate() {
        // A parked node writes no param events either.
        st.events.slot_mut(s).clear();
        u.keys.close_port(port, st.events.ev_mut(s));
    }
    u.keys.settle();

    // Events sent to a parked node are discarded: flushed ones, the closes
    // still owed to it and commands. Nothing it was sent goes unclosed: every
    // unit keeps a `held_in` table for each event input (`ModeState::new`),
    // which a bypass closes on its way out.
    if !ein.is_empty() {
        let unit = rec.unit;
        if *st.has_inject {
            for inj in st.inject.iter_mut().filter(|i| i.unit == unit) {
                inj.discard();
            }
            *st.has_inject = false;
        }
        *st.has_due = false;
    }
    u.quiet = Samples::ZERO;
    u.last_quiet = false;
    u.last_idle = false;
}

/// A parked node's outputs by index: output `c` is input `c`.
fn park_by_index<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit<F>,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    ain: &[AudioSlotId],
    aout: &[AudioSlotId],
) {
    let frames = head.frames;
    let main = u.mode.inputs();
    for (c, &s) in aout.iter().enumerate() {
        if c >= main {
            st.arena.slot_mut(s, frames).fill(F::ZERO);
            st.flags.set(s, Flags::SILENCE);
            continue;
        }
        let i = ain[c];
        let silent_in = st.flags.get(i).is_silent();
        match u.mode.ring(c) {
            None => {
                if i != s {
                    st.arena.copy_slot(i, s);
                    st.flags.set(s, st.flags.get(i));
                }
            }
            Some(ring) => {
                let silent = if i == s {
                    ring.run_in_place(st.arena.slot_mut(s, frames), silent_in)
                } else {
                    let (src, dst) = st.arena.pair(i, s, frames);
                    ring.run(src, dst, silent_in)
                };
                st.flags.set(s, Flags::new(silent, silent));
            }
        }
    }
}

/// A parked node's outputs as the fold of its main buses: every input through
/// its ring into the scratch first (an output may share an input's slot), then
/// each output channel folded from the scratch.
fn park_folded<F: Float, A: AudioSlots<F>, Fl: SlotFlags + ?Sized, E: EventSlots + ?Sized>(
    u: &mut Unit<F>,
    head: &Head<'_, '_>,
    st: &mut OpState<'_, A, Fl, E>,
    ain: &[AudioSlotId],
    aout: &[AudioSlotId],
) {
    let frames = head.frames;
    let mut silent_in = 0u64;
    for (c, &i) in ain.iter().enumerate().take(u.mode.inputs()) {
        let silent = st.flags.get(i).is_silent();
        if u.mode
            .align(c, st.arena.slot(i, frames), silent, true, frames)
        {
            silent_in |= 1 << c;
        }
    }
    let outputs = u.mode.outputs();
    for (c, &s) in aout.iter().enumerate() {
        if c >= outputs {
            st.arena.slot_mut(s, frames).fill(F::ZERO);
            st.flags.set(s, Flags::SILENCE);
            continue;
        }
        u.mode
            .fold_aligned_into(c, st.arena.slot_mut(s, frames), frames);
        let silent = u.mode.passes_silence(c, silent_in);
        st.flags.set(s, Flags::new(silent, silent));
    }
}
