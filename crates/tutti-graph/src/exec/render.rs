//! Rendering a block: the frame around the plan's ops, which `par.rs` runs.

use tutti_buffers::Float;

use tutti_platform::with_no_denormals;
use tutti_rt::RingWriter;
use tutti_units::Samples;

use super::commit::{Crossfade, retire_fade};
use super::node_op::Flags;
use super::state::{Inject, State, Unit};
use super::{Clock, Executor, Inbox, Suspension};
use crate::compile::ids::{AudioSlotId, Id, PlanUnitId, TableSlice};
use crate::compile::plan::Plan;
use crate::exec::history::TransportHistory;
use crate::exec::loss::Dropped;
use crate::exec::par::Block;
use crate::exec::slots::SlotFlags;
use crate::node::play::PlayClock;
use crate::node::time::{Env, TransportChanges, TransportPosition};
use tutti_music::MeterMap;
use tutti_tempo::TempoMap;

/// After a block in which a crossfade reached its end: retire each ended
/// fade's outgoing unit into the commit that started it, and start the fade
/// waiting behind it, if any — from the next block on.
#[cold]
#[inline(never)]
pub(super) fn end_fades<F: Float>(
    plan: &Plan,
    store: &mut [Option<Unit<F>>],
    fade_back: &mut RingWriter<Box<Crossfade<F>>>,
    inject: &mut [Inject],
    has_inject: &mut TableSlice<PlanUnitId, bool>,
    dropped: &mut Dropped,
) {
    for rec in &plan.nodes.recs {
        let Some(u) = store[rec.store.0 as usize].as_mut() else {
            continue;
        };
        if !u.fade.as_ref().is_some_and(|x| x.done >= x.len) {
            continue;
        }
        let ended = u.fade.take().expect("checked");
        retire_fade(fade_back, ended);
        if let Some(mut next) = u.queued.take() {
            let incoming = next.waiting.take().expect("a queued fade holds its unit");
            // What the unit that just finished fading in wrote for this one
            // (`Node::chase`), delivered at its first block: the injection
            // `rebuild` sized for it, ahead of anything already waiting
            // there. Rotated into place, so nothing allocates.
            if !next.chase.is_empty()
                && let Some(inj) = inject
                    .iter_mut()
                    .find(|i| i.unit == rec.unit && i.port == 0)
            {
                // Only as much as the buffer's room takes; the rest is lost
                // and charged to the unit.
                let room = inj.events.remaining().min(next.chase.len());
                inj.events.extend(next.chase[..room].iter().copied());
                inj.events.rotate_right(room);
                let lost = next.chase.len() - room;
                if lost > 0 {
                    dropped.at_node(rec.unit, u32::try_from(lost).unwrap_or(u32::MAX));
                }
                inj.live = true;
                has_inject[rec.unit] = true;
            }
            next.chase.clear();
            next.out = Some(std::mem::replace(&mut u.node, incoming));
            u.keys.promote();
            u.fade = Some(next);
            u.quiet = Samples::ZERO;
            u.last_quiet = false;
            u.last_idle = false;
            u.reported = None;
        }
    }
}

impl<F: Float> Executor<F> {
    /// Renders one block of `frames` into `outputs`, reading `inputs`.
    ///
    /// `inputs` must have a slice per main input channel the plan reads, and
    /// `outputs` one per main output channel; every slice at least `frames`
    /// long. Applies queued commits first ([`apply_pending`](Self::apply_pending));
    /// with none queued it never allocates. A caller that sizes the block from
    /// the executor's plan or prepare applies once itself and renders with
    /// [`render_applied`](Self::render_applied). A render prepared
    /// [`Realtime`](crate::RenderMode::Realtime) marks the thread with
    /// [`RtScope::enter`](tutti_rt::RtScope::enter) for the duration.
    ///
    /// # Panics
    ///
    /// If `frames` is zero or exceeds the prepared maximum block.
    pub fn process(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        inputs: &[&[F]],
        outputs: &mut [&mut [F]],
    ) {
        self.process_with_changes(frames, transport, &TransportChanges::NONE, inputs, outputs);
    }

    /// As [`process`](Self::process), with the transport changing inside the
    /// block: `transport` holds at the first frame, and each of `changes`
    /// from its offset on (a transport command lands on its frame). The block
    /// is **not** split at them. Every node gets the whole block and an
    /// [`Env`] carrying the changes, and scheduled `At::Beat`
    /// commands resolve against the transport in force where playback
    /// reaches their beat.
    ///
    /// # Panics
    ///
    /// As [`process`](Self::process), and if a change is not inside the
    /// block (outside a re-prepare, when any block length is accepted).
    pub fn process_with_changes(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        changes: &TransportChanges,
        inputs: &[&[F]],
        outputs: &mut [&mut [F]],
    ) {
        with_no_denormals(|| {
            self.apply_pending();
            self.render_applied(frames, transport, changes, inputs, outputs, None, None);
        });
    }

    /// As [`process_with_changes`](Self::process_with_changes), without
    /// applying queued commits: the block renders the plan, prepare and rate
    /// the executor holds now, and a commit sent later waits for the next
    /// block.
    ///
    /// A render prepared [`Realtime`](crate::RenderMode::Realtime) runs inside
    /// [`RtScope`](tutti_rt::RtScope): it never allocates, frees or waits. One
    /// prepared [`Offline`](crate::RenderMode::Offline) runs outside it and
    /// may block and allocate, on the calling thread and on a pool's
    /// participants alike.
    ///
    /// For a caller that reads [`plan`](Self::plan) or
    /// [`prepare`](Self::prepare) to size the block (its length, `outputs`)
    /// and must have them still true when it renders: it calls
    /// [`apply_pending`](Self::apply_pending) once, sizes from the result and
    /// renders here. [`process`](Self::process) would apply again, and a
    /// commit sent in between (a wider plan, a smaller `MaxBlock` from a
    /// re-prepare's resume) would change what the sizes were read from.
    ///
    /// `inputs` and `outputs` must have one slice per main input and output
    /// channel of the plan. Slices past those are ignored, on a pool or not. Fewer of either is a caller bug: it panics in debug builds, and
    /// in release the block renders silence into what was given, running no
    /// node.
    ///
    /// `meter` is the project's meter for this block, lent by the host once
    /// for the block and handed to every node through [`Cx::meter`](crate::Cx#structfield.meter) (on a pool or
    /// not); `None` where the host has none. `tempo` is the project's
    /// tempo map, lent the same way and handed through [`Cx::tempo`](crate::Cx#structfield.tempo).
    ///
    /// # Panics
    ///
    /// As [`process_with_changes`](Self::process_with_changes).
    pub fn render_applied(
        &mut self,
        frames: usize,
        transport: &TransportPosition,
        changes: &TransportChanges,
        inputs: &[&[F]],
        outputs: &mut [&mut [F]],
        meter: Option<&MeterMap>,
        tempo: Option<&TempoMap>,
    ) {
        let _rt = self.prepare.enter_rt_scope();
        // `move` captures `self` as one pointer; the precise capture of every
        // field the body names is a large struct built per block.
        with_no_denormals(move || {
            if let Suspension::Suspended { next, .. } = self.suspension {
                // Between a re-prepare's two commits the units are on the control
                // thread: render silence, for whatever block arrives — the device
                // may be on either side of the change, so no bound applies here.
                // The graph's own state (rings, FIFOs) stands still, but its
                // clock tracks device time: the frame counter advances, and the
                // playhead sees the transport move. A command whose time passes
                // meanwhile lands late when the units are back.
                for o in outputs.iter_mut() {
                    o[..frames].fill(F::ZERO);
                }
                let rate = next.sample_rate();
                let mut env = Env {
                    frame: self.clock.frame,
                    sample_rate: rate,
                    block_len: Samples::new(frames),
                    transport: *transport,
                    changes: *changes,
                    play: PlayClock::ZERO,
                };
                env.play = self.clock.play.begin(transport.playing, env.play_starts());
                self.inbox.commands.observe(&env);
                self.clock.history.observe(&env);
                self.clock.play.end(&env);
                self.clock.frame += Samples::new(frames);
                return;
            }
            let max = self.prepare.max_block();
            assert!(
                frames > 0 && frames <= max.get(),
                "block of {frames} frames against a max of {}",
                max.get()
            );
            assert!(
                changes.as_slice().iter().all(|c| c.at.index() < frames),
                "a transport change past the end of a {frames}-frame block"
            );
            let Self {
                prepare,
                event_cap,
                inbox:
                    Inbox {
                        queue: _,
                        back: _,
                        fade_back,
                        losses,
                        unreported,
                        edge_fades,
                        commands,
                        applied,
                    },
                plan,
                store,
                state,
                clock:
                    Clock {
                        frame,
                        history,
                        play,
                    },
                dropped,
                suspension: _,
                parked: _,
                chase: _,
                pool,
                pool_participants: _,
                rendered,
                main_output_fade,
                stash: _,
                logs,
            } = self;
            let mut env = Env {
                frame: *frame,
                sample_rate: prepare.sample_rate(),
                block_len: Samples::new(frames),
                transport: *transport,
                changes: *changes,
                play: PlayClock::ZERO,
            };
            env.play = play.begin(transport.playing, env.play_starts());
            // Before any node reads it, and also when there is nothing to render:
            // the transport moved all the same, and so did the play clock.
            history.observe(&env);
            play.end(&env);
            let history: &TransportHistory = history;
            let Some(plan) = plan.as_ref() else {
                for o in outputs.iter_mut() {
                    o[..frames].fill(F::ZERO);
                }
                *frame += Samples::new(frames);
                return;
            };
            if outputs.len() < plan.rendered_outputs()
                || inputs.len() < usize::from(plan.main_inputs())
            {
                debug_assert!(
                    false,
                    "{} input and {} output slices for a plan with {} main inputs and {} outputs",
                    inputs.len(),
                    outputs.len(),
                    plan.main_inputs(),
                    plan.rendered_outputs()
                );
                for o in outputs.iter_mut() {
                    o[..frames].fill(F::ZERO);
                }
                commands.observe(&env);
                *frame += Samples::new(frames);
                return;
            }
            let State {
                arena,
                flags,
                events,
                rings,
                audio_fb,
                event_fb,
                taps,
                event_taps,
                meters,
                lanes,
                inject,
                has_inject,
                has_due,
                overlay: _,
                par,
            } = state;
            // Slices, not `&mut Vec`s: a slice's pointer and length are locals
            // the op loop can keep in registers across the opaque node calls,
            // where a `Vec` behind a reference is reloaded after every one.
            let (flags, events, inject, has_inject, has_due) = (
                &mut flags[..],
                &mut events[..],
                &mut inject[..],
                &mut **has_inject,
                &mut **has_due,
            );
            let plan: &Plan = plan;
            let cap = *event_cap;
            commands.gather(&env, plan, *applied, has_due);
            let mut fade_ended = false;

            // Every weighted lane reads its cell once, here: a store while
            // the block runs reaches no op of it, so no participant count
            // can disagree on when it landed.
            for lane in lanes.iter_mut() {
                lane.snapshot();
            }
            // And every unit its mode (`mode.rs`), for the same reason.
            for rec in &plan.nodes.recs {
                if let Some(u) = store[rec.store.0 as usize].as_mut() {
                    u.mode.snapshot();
                }
            }

            // Feedback reads happen before any op: the delay is a whole
            // `MaxBlock`, so nothing this block's captures queue is due yet.
            for (f, spec) in audio_fb.iter().zip(&plan.audio_feedback) {
                let ring = f.as_ref().expect("built by apply");
                let is_silent =
                    ring.peek_oldest(arena.slot_mut(AudioSlotId::new(spec.slot), frames));
                flags.set(
                    AudioSlotId::new(spec.slot),
                    Flags::new(is_silent, is_silent),
                );
            }
            for (f, spec) in event_fb.iter_mut().zip(&plan.event_feedback) {
                let fifo = f.as_mut().expect("built by apply");
                let out = &mut events[spec.slot as usize];
                out.clear();
                fifo.pop_due(out, frames);
            }

            let due = commands.deliveries();
            // The level gate, once for the block: every lane's slots take it.
            logs.begin_block();
            // The pool's participants take the plan's tasks when the plan
            // spreads; otherwise the calling thread is the one participant.
            fade_ended |= crate::exec::par::run_block(
                par.as_mut().expect("a staged plan carries its par"),
                pool.as_ref(),
                Block {
                    dropped: &mut *dropped,
                    plan,
                    env: &env,
                    history,
                    meter,
                    tempo,
                    frames,
                    max,
                    cap,
                    inputs,
                    // The claim table is sized for the plan's width; slices
                    // past it are ignored.
                    outputs: &mut outputs[..plan.rendered_outputs()],
                    arena,
                    flags,
                    events,
                    store,
                    rings,
                    audio_fb,
                    event_fb,
                    taps,
                    event_taps,
                    meters,
                    lanes: &mut lanes[..],
                    inject,
                    has_inject,
                    has_due,
                    due,
                    logs: &mut logs.lanes[..],
                    log_claims: &mut logs.claims,
                },
            );

            if fade_ended {
                end_fades(plan, store, fade_back, inject, has_inject, dropped);
            }
            dropped.flush(plan, losses, unreported);
            // An edge whose declick fade ended this block tells the editor,
            // which drops it (or its entry) at its next collect. A full
            // ring keeps the report in the lane for the next block.
            for (lane, id) in lanes.iter_mut().zip(&plan.sum_lanes) {
                if let (Some(kind), Some(id)) = (lane.finished(), id)
                    && edge_fades.push((*id, kind)).is_ok()
                {
                    lane.acknowledge();
                }
            }
            for f in event_fb.iter_mut().flatten() {
                f.advance(frames);
            }
            main_output_fade.advance(plan, frames);
            *frame += Samples::new(frames);
            rendered.add_block();
        });
    }
}
