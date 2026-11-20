//! The render source: a graph presented as a block-at-a-time source of planes.
//!
//! [`GraphSource`] block-renders a `tutti_graph` executor into one plane per
//! global output and advances the caller's [`RenderClock`] in lockstep.
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
//! any positive count renders — 3- and 5-wide masters included.
//!
//! A fold from the graph's real output width onto a target's
//! ([`tutti_buffers::Fold`], built once, before the first block) downmixes a
//! wider graph rather than truncating it, and zero-fills a narrower one
//! rather than duplicating.

use crate::control::{Control, Outcome, Stage};
use crate::render::BlockCursor;
use crate::{ConfigError, RenderGraph, RtLogSink};
use std::ops::Range;
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Float, Fold, InterleavedMut, Planes, PlanesMut, Sample, interleave};
use tutti_graph::{RtLogReaders, fold_to_width};
use tutti_timeline::RenderClock;
use tutti_units::Samples;

/// What a target's channels are, for the fold from the graph's outputs or a
/// tap's lanes: its width, and the speakers they feed when it states them.
pub(crate) struct FileChannels<'a> {
    pub(crate) layout: ChannelLayout,
    pub(crate) topology: Option<&'a ChannelTopology>,
}

impl FileChannels<'_> {
    /// The fold from a source of `outputs` channels. The source is what it
    /// declared (`root`: the graph's root, or a tap's placement), or else the engine's order for its width (a bare 4 is
    /// quad); the file the topology it states or else the default layout for
    /// its width; where either has no order, or the file states unassigned
    /// channels, the channels go by index ([`fold_to_width`],
    /// [`Fold::by_index`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Placement`] when the file's topology cannot take the
    /// graph's channels, or the declared root is
    /// not as wide as the graph, or the graph has more channels than a layout
    /// holds ([`BufferError::Layout`](tutti_buffers::BufferError::Layout)).
    pub(crate) fn fold_from<F: Float>(
        &self,
        outputs: usize,
        root: Option<&ChannelTopology>,
    ) -> crate::Result<Fold<F>> {
        let graph = ChannelLayout::try_from(outputs)
            .map_err(|e| ConfigError::Placement(tutti_buffers::BufferError::from(e).into()))?;
        if let Some(root) = root
            && root.width() != graph
        {
            return Err(ConfigError::Root(tutti_graph::RootError::Width {
                declared: root.width(),
                requested: graph,
            })
            .into());
        }
        let Some(topology) = self.topology else {
            let fold = match root {
                Some(root) => fold_to_width(root, self.layout),
                None => fold_to_width(&ChannelTopology::default_for(graph), self.layout),
            };
            return fold.map_err(|e| ConfigError::Placement(e).into());
        };
        let from = match root {
            Some(root) => root.clone(),
            None => ChannelTopology::default_for(graph),
        };
        // A file that names no speaker takes the graph's channels by index,
        // as it says; `between` never pads by a guess.
        if topology.is_unassigned() {
            return Fold::by_index(from.width(), topology.width())
                .map_err(|e| ConfigError::Placement(e).into());
        }
        Fold::between(&from, topology).map_err(|e| ConfigError::Placement(e).into())
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
    /// Silence for every global input the graph declares: an export renders
    /// with nothing plugged in.
    silence: Vec<F>,
    global_inputs: usize,
    /// One plane per global output, `max_block` long.
    planes: Vec<Vec<F>>,
    /// The root's channels: the first of `planes`.
    outputs: usize,
    /// What the graph's output channels are, when it declared it.
    root: Option<ChannelTopology>,
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
        root: Option<ChannelTopology>,
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
        // the root's are the first `outputs`: a plan that carries an old
        // root's channels (a root change crossfading) renders them into planes
        // the export never reads.
        let (global_inputs, outputs, rendered) = executor.plan().map_or_else(
            || {
                let topology = &editor.spec().topology;
                let width = topology.outputs.len();
                (usize::from(topology.inputs.count()), width, width)
            },
            |p| {
                (
                    p.global_inputs() as usize,
                    p.global_outputs(),
                    p.rendered_outputs(),
                )
            },
        );
        let planes = vec![vec![F::ZERO; max_block]; rendered];
        // The gate is open from the first block.
        drain_rt_log(rt_log.as_ref());
        Ok(Self {
            editor,
            executor,
            clock,
            silence: vec![F::ZERO; max_block],
            global_inputs,
            planes,
            outputs,
            root,
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

    /// The graph's global output channels: the planes of a block.
    pub(crate) fn outputs(&self) -> usize {
        self.outputs
    }

    /// What the graph's outputs are, when it declared it.
    pub(crate) fn root(&self) -> Option<&ChannelTopology> {
        self.root.as_ref()
    }

    /// The planes of the block last rendered: one per global output, the first
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
        let inputs: Vec<&[F]> = (0..self.global_inputs)
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
    let root = graph.root_topology().cloned();
    let settled = graph.settled();
    let rt_log = graph.rt_log();
    let (editor, executor) = graph.parts_mut();
    let mut src = GraphSource::new(editor, executor, sample_rate, clock, root, settled, rt_log)?;
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

    control.begin(stage, Some(plan.total));
    while src.produced() < plan.total {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        // `Samples` has no `Sub` on purpose — an unsigned count that wraps is a
        // buffer size that reads off the end of the world. `remaining_after` is
        // the named, saturating form.
        let want = plan
            .total
            .remaining_after(src.produced())
            .min(Samples::new(max_block));
        let block_start = src.produced();
        let n = src.render_block(want.get());
        if n == 0 {
            break;
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
        control.advance(stage, src.produced(), plan.total);
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        // After the check, so a render the caller cancelled is not judged.
        check_loss(on_loss, src.lost_events())?;
    }

    Ok(Outcome::Finished(()))
}

#[cfg(test)]
mod tests;
