//! The render source: a graph presented as a block-at-a-time source of planes.
//!
//! [`GraphSource`] block-renders a `tutti_graph` executor into one plane per
//! main output and advances the caller's [`RenderClock`] in lockstep.
//! [`drive`] pulls it block by block, applying the latency and length gate,
//! and hands each block to the outputs (the fold onto each one's layout is
//! theirs, [`FileChannels::fold_from`]), so the graph is stepped exactly once
//! per block however many outputs there are. [`PlaneSource`] replays planes a
//! caller already holds, through the same [`Replay`] the spilled audio of a
//! normalized output is read back by.
//!
//! The frame width is a runtime value carried by
//! `Interleaved`, never a `const CH`. A width in the type can only carry one that is a property of the
//! *code*; the destination width here is a property of the caller's target, so
//! any positive count renders — 3- and 5-wide main outputs included.
//!
//! A fold from the graph's real output width onto a target's
//! ([`tutti_buffers::Fold`], built once, before the first block) downmixes a
//! wider graph rather than truncating it, and zero-fills a narrower one
//! rather than duplicating.

use crate::control::{Control, Outcome, Stage};
use crate::render::BlockCursor;
use crate::{ConfigError, RenderGraph, RtLogSink, TailPolicy};
use std::ops::Range;
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, ChannelSet, ChannelTopology};
use tutti_buffers::{
    Float, Fold, FoldError, InterleavedMut, Planes, PlanesMut, Sample, interleave,
};
use tutti_graph::{RtLogReaders, fold_to_channels};
use tutti_timeline::RenderClock;
use tutti_units::Samples;

/// What a target's channels are, for the fold from the graph's outputs or a
/// tap's lanes: its width, and the speakers they feed when it has them.
pub(crate) struct FileChannels {
    pub(crate) channels: ChannelSet,
}

impl FileChannels {
    /// The fold from a source of `outputs` channels. The source is what it
    /// declared (`main_output`: the graph's main output, or a tap's placement), or else the engine's order for its width (a bare 4 is
    /// quad); the file the channels it states or else the default layout for
    /// its width; where either has no order, or the file states unassigned
    /// channels, the channels go by index ([`fold_to_channels`],
    /// [`Fold::by_index`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Placement`] when the file's channels cannot take the
    /// graph's channels, or the declared main output is
    /// not as wide as the graph, or the graph has more channels than a layout
    /// holds ([`BufferError::Layout`](tutti_buffers::BufferError::Layout)).
    pub(crate) fn fold_from<F: Float>(
        &self,
        outputs: usize,
        main_output: &ChannelSet,
    ) -> crate::Result<Fold<F>> {
        let graph = ChannelLayout::try_from(outputs)
            .map_err(|e| ConfigError::Placement(tutti_buffers::BufferError::from(e).into()))?;
        if main_output.width() != graph {
            return Err(crate::RenderError::TopologyWidth {
                topology: main_output.width(),
                rendered: graph,
            });
        }
        // A main output past the widths a topology holds has none: its
        // channels go by index.
        let fold = match (main_output.topology(), self.channels.topology()) {
            (Some(from), _) => {
                fold_to_channels(from, &self.channels, tutti_buffers::FoldLevels::default())
            }
            // A file that names no speaker takes the graph's channels by
            // index, as it says; `between` never pads by a guess.
            (None, Some(topology)) if !topology.is_unassigned() => Err(FoldError::Unassigned {
                src: graph,
                dst: topology.width(),
            }),
            (None, _) => Fold::by_index(graph, self.channels.width()),
        };
        fold.map_err(|e| ConfigError::Placement(e).into())
    }
}

/// The render source for the graph: a `tutti_graph::Executor` (a
/// fork, or a pair built for the render) rendered one block at a time.
///
/// One block per [`render_block`](Self::render_block), the clock advanced by
/// exactly the frames produced. The executor reads the transport from each
/// block's `Env`, so this hands it one — through
/// [`render_graph`](tutti_timeline::render_graph), which reads the clock's
/// snapshot before the block and advances it after (`OfflineTimeline` cuts a
/// block at its tempo steps through `graph_piece`).
pub(crate) struct GraphSource<'a, F: Float> {
    editor: &'a mut tutti_graph::Editor<F>,
    executor: &'a mut tutti_graph::Executor<F>,
    clock: &'a dyn RenderClock,
    /// Silence for every main input the graph declares: an export renders
    /// with nothing plugged in.
    silence: Vec<F>,
    main_inputs: usize,
    /// One plane per main output, `max_block` long.
    planes: Vec<Vec<F>>,
    /// The main output's channels: the first of `planes`.
    outputs: usize,
    /// What the file is placed from: the main output the committed plan was
    /// compiled with, or the setup's own topology.
    main_output: ChannelSet,
    max_block: usize,
    produced: Samples,
    /// The executor's dropped and recovered counts the render counts from
    /// ([`RenderGraph`]'s settled figures: where the last render ended, or
    /// where the pair was wrapped); kept apart, because a recovery in the
    /// render may be of a refusal before it.
    before: (u64, u64),
    /// Where the executor's log records go between blocks.
    rt_log: Option<(RtLogReaders, Arc<dyn RtLogSink>)>,
}

/// Hands what the executor's log rings hold to the sink, stores the sink's
/// filter for the next block, and reports what was lost since the last drain.
fn drain_rt_log(rt_log: Option<&(RtLogReaders, Arc<dyn RtLogSink>)>) {
    if let Some((readers, sink)) = rt_log {
        let lost = readers.drain(sink.filter(), |r| sink.record(r));
        if lost > 0 {
            sink.lost(lost);
        }
    }
}

impl<'a, F: Float> GraphSource<'a, F> {
    /// Refuses a pair it cannot render as configured rather than rendering
    /// something else: an executor prepared at a rate other than the
    /// render's. (The pairing is `RenderGraph::assemble`'s check.) An executor's units are prepared for one rate
    /// on the control side, which is what `RenderGraph::prepare` and
    /// the session's fork are for; re-rating them here would be preparing
    /// them on the render thread.
    pub(crate) fn new(
        editor: &'a mut tutti_graph::Editor<F>,
        executor: &'a mut tutti_graph::Executor<F>,
        sample_rate: tutti_units::SampleRate,
        clock: &'a dyn RenderClock,
        placed_from: Option<ChannelTopology>,
        before: (u64, u64),
        rt_log: Option<(RtLogReaders, Arc<dyn RtLogSink>)>,
    ) -> crate::Result<Self> {
        let prepared = executor.prepare().sample_rate();
        if prepared.get() != sample_rate.get() {
            return Err(ConfigError::RateMismatch {
                prepared: prepared.get(),
                render: sample_rate.get(),
            }
            .into());
        }
        let max_block = executor.prepare().max_block().get();
        // Sized from the plan the first block renders, not from the editor's
        // spec: the spec may hold edits that were never committed, and the
        // executor renders only what was. (Nothing commits during the render,
        // so the plan applied here is the one every block runs.)
        //
        // Installing a commit can itself drop events (a note-off closing into
        // a full delay), so the loss is counted from `before`, which predates
        // this call, and not from here.
        executor.apply_pending();
        // The executor takes a slice for every channel the plan renders, and
        // the main output's are the first `outputs`: a plan that carries an old
        // main output's channels (a main output change crossfading) renders them into planes
        // the export never reads.
        let (main_inputs, outputs, rendered) = executor.plan().map_or_else(
            || {
                let topology = &editor.spec().topology;
                let width = topology.outputs.len();
                (usize::from(topology.inputs.count()), width, width)
            },
            |p| {
                (
                    p.main_inputs() as usize,
                    p.main_outputs(),
                    p.rendered_outputs(),
                )
            },
        );
        let main_output = placed_from.map(ChannelSet::from).unwrap_or_else(|| {
            executor
                .plan()
                .map_or(editor.spec().main_output, |p| p.main_output())
        });
        let planes = vec![vec![F::ZERO; max_block]; rendered];
        // The gate is open from the first block.
        drain_rt_log(rt_log.as_ref());
        Ok(Self {
            editor,
            executor,
            clock,
            silence: vec![F::ZERO; max_block],
            main_inputs,
            planes,
            outputs,
            main_output,
            max_block,
            produced: Samples::new(0),
            before,
            rt_log,
        })
    }
}

impl<F: Float> GraphSource<'_, F> {
    /// The executor's `(dropped, recovered)` event counts now.
    fn counts(&self) -> (u64, u64) {
        (
            self.executor.dropped_event_count(),
            self.executor.recovered_event_count(),
        )
    }

    /// The graph's main output channels: the planes of a block.
    pub(crate) fn outputs(&self) -> usize {
        self.outputs
    }

    /// What the file is placed from.
    pub(crate) fn main_output(&self) -> &ChannelSet {
        &self.main_output
    }

    /// The planes of the block last rendered: one per main output, the first
    /// frames of each valid.
    pub(crate) fn planes(&self) -> &[Vec<F>] {
        &self.planes[..self.outputs]
    }

    /// Frames handed out so far.
    pub(crate) fn produced(&self) -> Samples {
        self.produced
    }

    /// The most frames one [`render_block`](Self::render_block) produces: the
    /// block the graph was prepared for. Pulling it 64 frames at a time would
    /// run its per-block walk sixteen times as often as it needs to.
    pub(crate) fn max_block(&self) -> usize {
        self.max_block
    }

    /// Renders up to `want` frames (never more than [`max_block`](Self::max_block))
    /// into [`planes`](Self::planes) and returns how many. A render never
    /// starves: the graph produces a full block on demand, so the only `0`
    /// this returns is for a zero-length request.
    ///
    /// That is the offline/live split in one line. A `Source`'s pull answers
    /// starved, ended or failed, because it is polled until dry on a clock of
    /// its own. An offline render is driven to a known frame count, so no
    /// verdict is asked for and this is not a `Source`.
    pub(crate) fn render_block(&mut self, want: usize) -> usize {
        let block_size = want.min(self.max_block);
        if block_size == 0 {
            return 0;
        }

        // Per block, and allocating: the executor takes slice lists, and a
        // list of borrows cannot live beside the buffers it borrows. Two
        // short `Vec`s per `max_block` frames, on an offline worker — the
        // executor's own walk inside `render_graph` stays allocation-free.
        let inputs: Vec<&[F]> = (0..self.main_inputs)
            .map(|_| &self.silence[..block_size])
            .collect();
        let mut outputs: Vec<&mut [F]> = self
            .planes
            .iter_mut()
            .map(|p| &mut p[..block_size])
            .collect();
        // Snapshot, process, advance — in that order, never a priming
        // advance first: frame 0 of a block carries the block's start beat,
        // and only then does the beat move on. Every clock reader agrees on
        // it (every node reads the snapshot from its `Env`), so an advance
        // before the block would start the render one frame past its own
        // start beat.
        tutti_timeline::render_graph(
            self.clock,
            self.executor,
            if inputs.is_empty() {
                Planes::default()
            } else {
                Planes::new(&inputs).expect("every plane is one block long")
            },
            PlanesMut::new(&mut outputs).expect("every plane is one block long"),
        );
        // Retired units and returned commits are freed here, on this thread,
        // as a host's control loop would free them.
        self.editor.collect();
        drain_rt_log(self.rt_log.as_ref());
        self.produced += Samples::new(block_size);
        block_size
    }

    /// The tail the graph's units owe now ([`tutti_graph::Executor::reported_tail`]),
    /// between blocks.
    pub(crate) fn reported_tail(&self) -> tutti_graph::GraphTail {
        self.executor.reported_tail(&self.editor.spec().topology)
    }

    /// Events the graph lost since the render began (`Executor::lost_event_count`):
    /// not a note-off it refused and sent again.
    pub(crate) fn lost_events(&self) -> u64 {
        lost_over(self.before, self.counts())
    }

    /// Whether the units behind this source are still healthy: a forked unit
    /// (a hosted plugin) that failed mid-render rendered silence from then
    /// on, and `process` has no error channel to say so.
    ///
    /// Asked before a file is put in place, so a failed render never replaces
    /// the user's file.
    ///
    /// # Errors
    ///
    /// [`RenderError::ForkFailed`](crate::RenderError::ForkFailed) naming the first unit
    /// that failed.
    pub(crate) fn check_health(&self) -> crate::Result<()> {
        self.editor
            .fork_health()
            .map_err(|fault| crate::RenderError::ForkFailed {
                key: fault.key,
                kind: fault.kind,
                cause: fault.cause,
            })
    }
}

/// Run `f` over the frame source for `graph`, then check the fork's health.
/// Returns what `f` did and the events the graph lost over the render; what
/// to do about them is the caller's ([`check_loss`]), made before a file is
/// committed and not for a cancelled render.
///
/// The one place a [`RenderGraph`] becomes a [`GraphSource`], so every render
/// is made and checked identically.
pub(crate) fn with_source<F: Float, T>(
    graph: &mut RenderGraph<F>,
    sample_rate: tutti_units::SampleRate,
    clock: &dyn RenderClock,
    f: impl FnOnce(&mut GraphSource<'_, F>) -> crate::Result<Outcome<T>>,
) -> crate::Result<(Outcome<T>, u64)> {
    let placed_from = graph.main_output_override();
    let settled = graph.settled();
    let rt_log = graph.rt_log();
    let (editor, executor) = graph.parts_mut();
    let mut src = GraphSource::new(
        editor,
        executor,
        sample_rate,
        clock,
        placed_from,
        settled,
        rt_log,
    )?;
    let rendered = f(&mut src);
    // The next render counts from here, whether this one finished or not.
    let counts = src.counts();
    let lost = src.lost_events();
    // The fallback: the render asks before it puts a file in place or returns
    // planes (`render`); this one covers any caller that did not.
    // A render the caller cancelled is not judged, so a fault that came
    // before the cancel does not turn it into a failure.
    let health = match &rendered {
        Ok(Outcome::Finished(_)) | Err(_) => src.check_health(),
        Ok(Outcome::Cancelled) => Ok(()),
    };
    graph.settle(counts);
    let rendered = rendered?;
    health?;
    Ok((rendered, lost))
}

/// The events lost between two readings of the executor's `(dropped,
/// recovered)` counts. A recovery can be of a refusal from before the first
/// reading, so the refusals still owed then (`dropped - recovered`) are
/// settled first: only recoveries past them took back a refusal made in the
/// span.
pub(crate) fn lost_over(before: (u64, u64), after: (u64, u64)) -> u64 {
    let owed = before.0.saturating_sub(before.1);
    let dropped = after.0.saturating_sub(before.0);
    let recovered = after.1.saturating_sub(before.1).saturating_sub(owed);
    dropped.saturating_sub(recovered)
}

/// Fails a render that lost events under `EventLossPolicy::Fail`
pub(crate) fn check_loss(policy: crate::EventLossPolicy, lost: u64) -> crate::Result<()> {
    if lost > 0 && policy == crate::EventLossPolicy::Fail {
        return Err(crate::RenderError::EventsDropped { count: lost });
    }
    Ok(())
}

/// Audio read back in blocks: planes a caller holds, or what a normalized
/// output spilled while the graph rendered.
pub(crate) trait Replay<F: Sample = f32> {
    /// Fills the leading frames of `out`, returning how many frames were
    /// written; `0` is the end.
    fn fill(&mut self, out: InterleavedMut<'_, F>) -> crate::Result<usize>;
}

/// Replays already-rendered planes.
///
/// Planes of the file's own width replay as they are. Any other width is
/// folded onto the file's through the [`Fold`] the caller built
/// ([`FileChannels::fold_from`]), the way the graph's outputs are: a wider
/// render is placed by position and a narrower one by the engine's order,
/// never truncated or zero-padded.
pub(crate) struct PlaneSource<'a, F: Float = f32> {
    planes: &'a [Vec<F>],
    fold: Option<Fold<F>>,
    /// The zeros a channel the planes lack reads: grown to a block, never
    /// shrunk.
    silence: Vec<F>,
    pos: Samples,
}

impl<'a, F: Float> PlaneSource<'a, F> {
    pub(crate) fn new(planes: &'a [Vec<F>], fold: Option<Fold<F>>) -> Self {
        Self {
            planes,
            fold,
            silence: Vec::new(),
            pos: Samples::new(0),
        }
    }
}

impl<F: Float> Replay<F> for PlaneSource<'_, F> {
    fn fill(&mut self, mut out: InterleavedMut<'_, F>) -> crate::Result<usize> {
        let available = self
            .planes
            .first()
            .map_or(0, |p| p.len().saturating_sub(self.pos.get()));
        let n = out.frames().get().min(available);
        if n == 0 {
            return Ok(0);
        }
        let block = out.slice_mut(Samples::ZERO..Samples::new(n));
        let pos = self.pos.get();
        let span = Samples::new(pos)..Samples::new(pos + n);
        // Both write every sample of every frame (rather than leaving whatever
        // the buffer held), which matters because the caller's block is reused
        // across iterations.
        match &self.fold {
            Some(fold) => {
                let planes = Planes::from_vecs(self.planes)?.slice(span);
                fold.fold_planar_to_interleaved(planes, block)?;
            }
            // Planes of the file's own width interleave strictly. A render with
            // another channel count is silent in the channels it lacks (a
            // plane of zeros stands in for each) and its extra planes are not
            // written: the planes the file has are interleaved strictly.
            None if self.planes.len() == block.channels() => {
                interleave(Planes::from_vecs(self.planes)?.slice(span), block)?;
            }
            None => {
                let width = block.channels();
                if self.silence.len() < n {
                    self.silence.resize(n, F::ZERO);
                }
                self.silence[..n].fill(F::ZERO);
                let silence = &self.silence[..n];
                let table: Vec<&[F]> = (0..width)
                    .map(|c| self.planes.get(c).map_or(silence, |p| &p[pos..pos + n]))
                    .collect();
                interleave(Planes::new(&table)?, block)?;
            }
        }
        self.pos += Samples::new(n);
        Ok(n)
    }
}

/// The frames the replay of planes hands out per block: 64, which only paces
/// the encoder.
///
/// Not raised to the graph's block: a 1024-frame replay measured no faster.
pub(crate) const REPLAY_BLOCK: usize = 64;

/// Pull the whole render out of `src`, gating each block, handing every block
/// to `consume`.
///
/// The gate (latency trim + output-length cap) lives here rather than on a sink
/// because it needs counters that span blocks. `consume` is called for every
/// block, with the block's planes (the first `n` frames of each valid) and the
/// frame range of it the gate keeps: empty for a block before the latency or
/// past the length, which a tap still has to drain.
///
/// A fixed tail ([`TailPolicy::Frames`]) is pulled with the programme. A
/// reported one ([`TailPolicy::Reported`]) is asked of the graph at the
/// programme's last frame and again after every block pulled for it: each ask
/// bounds the tail (what is spent plus what is owed), the tail is the
/// smallest bound seen, so it is never longer than the first ask's and ends
/// early when a node comes to rest. The graph's latency is then pulled once, so a
/// ring-out inside a latency after the node that rang still reaches the
/// output; progress counts the plan's most until then and the true total
/// after.
///
/// The token is checked before every block and after the progress a block
/// delivered (a callback may cancel; the last block's too), and a cancelled
/// render ends there as [`Outcome::Cancelled`]. `stage` is the stage the
/// frames count in.
///
/// Under [`EventLossPolicy::Fail`](crate::EventLossPolicy::Fail) the render
/// stops at the first block that has lost an event, after that block's
/// progress and cancel check (a render that was cancelled is not judged), with
/// [`RenderError::EventsDropped`](crate::RenderError::EventsDropped) counting what had
/// been lost by then: a render that is going to fail does not spend the rest
/// of its length first.
pub(crate) fn drive<F: Float, C>(
    src: &mut GraphSource<'_, F>,
    plan: &crate::render::RenderPlan,
    stage: Stage,
    control: &mut Control<'_>,
    on_loss: crate::EventLossPolicy,
    mut consume: C,
) -> crate::Result<Outcome<()>>
where
    C: FnMut(&GraphSource<'_, F>, usize, Range<usize>) -> crate::Result<()>,
{
    let max_block = src.max_block().max(1);
    let mut kept = Samples::new(0);

    // Where the render is, and what progress counts: the plan's most until
    // the flush fixes the true total.
    let mut phase = Phase::Programme;
    let mut total = plan.total;
    control.begin(stage, Some(plan.total));
    loop {
        // The frames the phase pulls next, and the phase after it: none left
        // moves the render on.
        let pulled = src.produced();
        let (next, want) = match phase {
            Phase::Programme => match plan.tail {
                TailPolicy::Frames(_) => (Phase::Done, plan.total.remaining_after(pulled)),
                TailPolicy::Reported {
                    ceiling,
                    kept_latency,
                } => (
                    Phase::Tail {
                        ceiling,
                        kept_latency,
                        spent: Samples::ZERO,
                        bound: ceiling,
                    },
                    plan.programme.remaining_after(pulled),
                ),
            },
            Phase::Tail {
                ceiling,
                kept_latency,
                spent,
                bound,
            } => {
                // Every ask bounds the tail from the programme's end: what
                // is spent plus what is owed now (which also counts what
                // sounded after the end, so it is never too short). The tail
                // is the smallest bound seen, and never passes `ceiling`.
                let owed = src.reported_tail().resolve(ceiling.remaining_after(spent));
                let bound = bound.min(spent + owed);
                let left = bound.remaining_after(spent);
                phase = Phase::Tail {
                    ceiling,
                    kept_latency,
                    spent,
                    bound,
                };
                // The graph's latency is pulled once, after the last ask, for
                // every node; the true total is known from here.
                let end = pulled + plan.latency + kept_latency;
                if left == Samples::ZERO {
                    total = end;
                    control.advance(stage, pulled, total);
                }
                (Phase::Flush { end }, left)
            }
            Phase::Flush { end } => (Phase::Done, end.remaining_after(pulled)),
            Phase::Done => break,
        };
        if want == Samples::ZERO {
            phase = next;
            continue;
        }
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let block_start = pulled;
        let n = src.render_block(want.get().min(max_block));
        if n == 0 {
            return Ok(Outcome::Finished(()));
        }
        if let Phase::Tail { spent, .. } = &mut phase {
            *spent += Samples::new(n);
        }

        let cursor = BlockCursor {
            block_start,
            latency: plan.latency,
            kept_so_far: kept,
            output_length: plan.output_length,
        };
        let window = cursor.window(Samples::new(n));
        let kept_now = window.len();
        consume(src, n, window)?;
        kept += Samples::new(kept_now);
        control.advance(stage, src.produced(), total);
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        // After the check, so a render the caller cancelled is not judged.
        check_loss(on_loss, src.lost_events())?;
    }

    Ok(Outcome::Finished(()))
}

/// Where [`drive`] is in a render. A render is a programme, then (for a
/// reported tail) a tail asked of the graph after every block, then the
/// graph's latency flushed once.
#[derive(Clone, Copy)]
enum Phase {
    /// Pulling the programme, or all of a fixed tail's frames.
    Programme,
    /// Pulling a reported tail: `spent` frames so far, up to `bound`, the
    /// smallest bound any ask has given (never past `ceiling`).
    Tail {
        ceiling: Samples,
        kept_latency: Samples,
        spent: Samples,
        bound: Samples,
    },
    /// Pulling the graph's latency (and the kept part of it), up to the
    /// frame `end`.
    Flush {
        end: Samples,
    },
    Done,
}

#[cfg(test)]
mod tests;
