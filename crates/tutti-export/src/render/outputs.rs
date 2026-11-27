//! One render pass, N outputs.
//!
//! The graph is rendered once. Every block goes to each output's lane: the
//! lane reads its feed (the graph's planes, or a tap's lanes drained from its
//! ring), folds it onto its target's layout, and hands the gated frames to its
//! sink. A streamed file output goes through its [`Chain`] (resample, dither,
//! encode) as the blocks arrive; a normalizing one keeps them in a [`Spill`]
//! and is measured and written after the render, from the spill, so the graph
//! never renders twice; planes are appended to memory.
//!
//! The stages after the render run over the outputs in order, and the files
//! are put in place last: every output's encoder is finalized into its
//! temporary first, then the token is checked once more, then the renames.

use std::path::PathBuf;

use tutti_buffers::{ChannelLayout, ChannelSet, Placement as ChannelPlacement};
use tutti_buffers::{Float, Fold, Interleaved, InterleavedMut, Planes, PlanesMut};
use tutti_codec::{Chain, Converter, Kept, Landed};
use tutti_dsp::FftFloat;
use tutti_graph::TapReader;
use tutti_measure::Loudness;
use tutti_timeline::RenderClock;
use tutti_units::{Db, SampleRate, Samples};

use crate::control::{Control, Outcome, Stage, Weights};
use crate::normalize::{Measurer, Normalize, Reading, chunk_frames};
use crate::output::{BufferTarget, FileTarget, Gain, TargetError};
use crate::outputs::{Parts, Placed};
use crate::render::{
    FileChannels, REPLAY_BLOCK, RenderPlan, Replay, check_loss, drive, with_source,
};
use crate::spill::{Spill, SpillBudget};
use crate::{
    ConfigError, Done, Feed, Job, Outputs, RenderError, RenderGraph, Rendered, Result, Unmeasured,
    Written,
};

/// A tap an output reads: the reader, and the planes one block of its lanes
/// is drained into.
struct TapFeed<F: Float> {
    reader: TapReader<F>,
    planes: Vec<Vec<F>>,
}

/// A file that waits for its gain: what the graph rendered for it, and what
/// decides the gain.
struct Waiting<F: Float> {
    target: FileTarget,
    /// The channels of the file: what the spill holds.
    channels: ChannelSet,
    spill: Spill<F>,
    gain: Gain,
}

/// Where a file lane's gated frames go.
enum Sink<F: FftFloat> {
    /// Straight into the file's chain as the blocks arrive.
    Stream { chain: Box<Chain<F>> },
    /// Into a spill, to be measured and written once the render is over.
    Spill {
        chain: Box<Chain<F>>,
        waiting: Box<Waiting<F>>,
    },
}

/// What every lane holds, whatever it becomes: the feed, its fold onto the
/// target's channels and the block it folds into.
struct Lane<F: FftFloat> {
    /// The output's position among the render's outputs of both kinds.
    position: usize,
    /// The feed's channels onto the target's.
    fold: Fold<F>,
    /// Set for a tap feed; the graph's planes otherwise.
    tap: Option<TapFeed<F>>,
    layout: ChannelLayout,
    /// Whether the target stated its channels' topology.
    placement: ChannelPlacement,
    /// The interleaved frames of one block's window, at the target's layout.
    scratch: Vec<F>,
}

/// A file output of the pass.
struct FileLane<F: FftFloat> {
    lane: Lane<F>,
    path: PathBuf,
    sink: Sink<F>,
}

/// A buffer output of the pass: planes appended in memory.
struct BufferLane<F: FftFloat> {
    lane: Lane<F>,
    planes: Vec<Vec<F>>,
}

impl<F: FftFloat> Lane<F> {
    /// Drains the feed, folds the window of this block onto the target's
    /// layout and returns it; `None` when the gate keeps nothing of the block.
    fn block(
        &mut self,
        graph_planes: &[Vec<F>],
        n: usize,
        window: &std::ops::Range<usize>,
    ) -> Result<Option<Interleaved<'_, F>>> {
        let planes: &[Vec<F>] = match &mut self.tap {
            Some(tap) => {
                drain_tap(self.position, tap, n)?;
                &tap.planes
            }
            None => graph_planes,
        };
        if window.is_empty() {
            return Ok(None);
        }
        let frames = window.len();
        let samples = self.layout.interleaved_len(Samples::new(frames));
        let mut block = InterleavedMut::new(&mut self.scratch[..samples], self.layout)?;
        let span = Samples::new(window.start)..Samples::new(window.end);
        self.fold
            .fold_planar_to_interleaved(Planes::from_vecs(planes)?.slice(span), block.reborrow())?;
        Ok(Some(Interleaved::new(
            &self.scratch[..samples],
            self.layout,
        )?))
    }
}

/// Renders `graph` once into every output of `outputs`.
///
/// The body of [`render`](crate::render()); `spill_budget` is the memory the
/// normalizing outputs may keep before they move to files
/// ([`SPILL_MEMORY_BYTES`](crate::spill::SPILL_MEMORY_BYTES) in production; the
/// tests of the spill pass a small one).
pub(crate) fn render_outputs<F: FftFloat>(
    graph: &mut RenderGraph<F>,
    job: &Job,
    clock: &dyn RenderClock,
    outputs: Outputs<F>,
    control: &mut Control<'_>,
    spill_budget: usize,
) -> Result<Outcome<Done<F>>> {
    let parts = outputs.into_parts();
    let stamp = parts.stamp();
    control.weigh(Weights::of(parts.files.iter().map(|f| f.output.target())));
    let plan = RenderPlan::of(job);
    let (done, _lost) = with_source(graph, job.rate(), clock, |src| {
        check_outputs(&parts, src.main_output())?;
        let (mut files, mut buffers) = open_lanes(
            parts.files,
            parts.buffers,
            job,
            &plan,
            src.outputs(),
            src.main_output(),
            src.max_block(),
        )?;
        let mut budget = SpillBudget::new(spill_budget);

        let driven = drive(
            src,
            &plan,
            Stage::Rendering,
            control,
            job.on_event_loss(),
            |src, n, window| {
                let graph_planes = src.planes();
                for file in &mut files {
                    let Some(block) = file.lane.block(graph_planes, n, &window)? else {
                        continue;
                    };
                    match &mut file.sink {
                        Sink::Stream { chain } => chain.block(block)?,
                        Sink::Spill { waiting, .. } => {
                            waiting.spill.push(block.samples(), &mut budget)?;
                        }
                    }
                }
                for buffer in &mut buffers {
                    if let Some(block) = buffer.lane.block(graph_planes, n, &window)? {
                        crate::planar::append(block, &mut buffer.planes)?;
                    }
                }
                Ok(())
            },
        )?;
        if driven == Outcome::Cancelled {
            return Ok(Outcome::Cancelled);
        }
        // Before anything is measured or put in place: a unit that failed
        // rendered silence into what the outputs hold, and a render that lost
        // events is wrong.
        src.check_health()?;
        let lost = src.lost_events();
        check_loss(job.on_event_loss(), lost)?;

        finish(files, buffers, job, lost, control)
    })?;
    Ok(done.map(|(files, buffers)| stamp.done(files, buffers)))
}

/// What the outputs are refused for before any of them is opened: no outputs, a
/// zero width, a tap that is not as wide as its placement, a gain taken from an
/// output that does not measure or from another `Outputs`, and two files with
/// one path.
fn check_outputs<F: Float>(parts: &Parts<F>, main_output: &ChannelSet) -> Result<()> {
    if parts.files.is_empty() && parts.buffers.is_empty() {
        return Err(RenderError::NoOutputs);
    }
    for placed in &parts.buffers {
        let target_error = |why| RenderError::Target {
            output: placed.position,
            why,
        };
        check_feed(
            placed,
            &channels_of(
                placed.output.feed(),
                Some(placed.output.target().channel_set()),
                main_output,
            ),
            target_error,
        )?;
    }
    for (i, placed) in parts.files.iter().enumerate() {
        let file = placed.output.target();
        let target_error = |why| RenderError::Target {
            output: placed.position,
            why,
        };
        let channels = channels_of(placed.output.feed(), file.channel_set(), main_output);
        check_feed(placed, &channels, target_error)?;
        if let Some(Gain::Own(Normalize::Lufs { .. })) = file.normalize {
            // Loudness is weighted by placement: refused here, not after the render.
            // A width with no topology has no placement.
            let placed = channels.topology().is_some_and(|topology| {
                tutti_measure::Measure::<f32>::measure(
                    &tutti_measure::LoudnessConfig::new(*topology),
                    tutti_units::SampleRate::SR_48K,
                )
                .is_ok()
            });
            if !placed {
                return Err(target_error(TargetError::UnplacedLoudness {
                    channels: channels.width(),
                }));
            }
        }
        if let Some(Gain::Of(key)) = &file.normalize {
            if !parts.owns(key) {
                return Err(target_error(TargetError::ForeignGain));
            }
            // A key names a file added before this one, of these outputs.
            let from = &parts.files[key.index()];
            if !matches!(from.output.target().normalize, Some(Gain::Own(_))) {
                return Err(target_error(TargetError::GainSource {
                    index: from.position,
                }));
            }
        }
        for other in parts.files.iter().take(i) {
            if other.output.target().path() == file.path() {
                return Err(target_error(TargetError::SamePath {
                    other: other.position,
                }));
            }
        }
    }
    Ok(())
}

/// What any output is refused for: a zero width, and a tap whose lanes are
/// not as wide as its placement.
fn check_feed<T, F: Float>(
    placed: &Placed<T, F>,
    channels: &ChannelSet,
    target_error: impl Fn(TargetError) -> RenderError,
) -> Result<()> {
    if channels.width().count() == 0 {
        return Err(RenderError::UnsupportedChannels);
    }
    if let Feed::Tap { reader, lanes } = placed.output.feed()
        && reader.layout() != lanes.layout()
    {
        return Err(target_error(TargetError::Lanes {
            lanes: reader.layout(),
            placement: lanes.layout(),
        }));
    }
    Ok(())
}

/// The channels of a target: the ones it states, else the feed's.
fn channels_of<F: Float>(
    feed: &Feed<F>,
    stated: Option<ChannelSet>,
    main_output: &ChannelSet,
) -> ChannelSet {
    stated.unwrap_or_else(|| match feed {
        Feed::MainOutput => *main_output,
        Feed::Tap { lanes, .. } => lanes.channel_set(),
    })
}

/// Opens every output: its fold, its file (refused before it is touched when
/// the format cannot hold the render) or its planes.
fn open_lanes<F: FftFloat>(
    file_outputs: Vec<Placed<FileTarget, F>>,
    buffer_outputs: Vec<Placed<BufferTarget, F>>,
    job: &Job,
    plan: &RenderPlan,
    graph_outputs: usize,
    main_output: &ChannelSet,
    max_block: usize,
) -> Result<(Vec<FileLane<F>>, Vec<BufferLane<F>>)> {
    let mut files = Vec::with_capacity(file_outputs.len());
    for Placed { position, output } in file_outputs {
        let (feed, file) = output.into_parts();
        let set = channels_of(&feed, file.channel_set(), main_output);
        let lane = open_lane(
            position,
            feed,
            set,
            file.placement(),
            graph_outputs,
            main_output,
            max_block,
        )?;
        // The blocks the chain is given are folded onto the file's channels by the
        // lane, which also holds them for a normalize: its source is the file's.
        let chain = Box::new(Chain::new(
            file.to_file(),
            &set,
            job.rate(),
            plan.output_length,
        )?);
        let path = file.path().to_path_buf();
        let sink = match file.normalize.clone() {
            None => Sink::Stream { chain },
            Some(gain) => Sink::Spill {
                chain,
                waiting: Box::new(Waiting {
                    spill: Spill::new(lane.layout, &path),
                    channels: set,
                    target: file,
                    gain,
                }),
            },
        };
        files.push(FileLane { lane, path, sink });
    }
    let mut buffers = Vec::with_capacity(buffer_outputs.len());
    for Placed { position, output } in buffer_outputs {
        let (feed, target) = output.into_parts();
        let set = channels_of(&feed, Some(target.channel_set()), main_output);
        let lane = open_lane(
            position,
            feed,
            set,
            target.placement(),
            graph_outputs,
            main_output,
            max_block,
        )?;
        let planes = reserve_planes(plan, lane.layout)?;
        buffers.push(BufferLane { lane, planes });
    }
    Ok((files, buffers))
}

/// The lane of one output onto `set`: its fold and the block it folds into.
fn open_lane<F: FftFloat>(
    position: usize,
    feed: Feed<F>,
    set: ChannelSet,
    placement: ChannelPlacement,
    graph_outputs: usize,
    main_output: &ChannelSet,
    max_block: usize,
) -> Result<Lane<F>> {
    let layout = set.width();
    let channels = FileChannels { channels: set };
    let (fold, tap) = match feed {
        Feed::MainOutput => (channels.fold_from(graph_outputs, main_output)?, None),
        Feed::Tap { reader, lanes } => {
            let width = lanes.layout();
            let fold = channels
                .fold_from(usize::from(width.count()), &lanes.channel_set())
                .map_err(|e| match e {
                    RenderError::InvalidConfig(ConfigError::Placement(why)) => {
                        RenderError::Target {
                            output: position,
                            why: TargetError::Placement(why),
                        }
                    }
                    other => other,
                })?;
            let planes = vec![vec![F::ZERO; max_block]; reader.width()];
            (fold, Some(TapFeed { reader, planes }))
        }
    };
    Ok(Lane {
        position,
        fold,
        tap,
        layout,
        placement,
        scratch: vec![F::ZERO; layout.interleaved_len(Samples::new(max_block))],
    })
}

/// Empty planes with room for what the plan keeps.
///
/// Reserved fallibly: the duration is the caller's, and an hour of 7.1.4 (or a
/// typo) is a plain `Err` here where `with_capacity` would abort the process.
/// (`vec![Vec::with_capacity(n); ch]` would clone ONE empty Vec `ch` times,
/// and a clone does not carry capacity.)
fn reserve_planes<F: Float>(plan: &RenderPlan, layout: ChannelLayout) -> Result<Vec<Vec<F>>> {
    let too_large = || RenderError::RenderTooLarge {
        frames: plan.output_length,
        channels: layout,
    };
    let ch = usize::from(layout.count());
    let mut planes: Vec<Vec<F>> = Vec::new();
    planes.try_reserve_exact(ch).map_err(|_| too_large())?;
    for _ in 0..ch {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(plan.output_length.get())
            .map_err(|_| too_large())?;
        planes.push(plane);
    }
    Ok(planes)
}

/// Drains the `n` frames the block just rendered from a tap's rings into its
/// planes, every block, whether or not the gate keeps them.
fn drain_tap<F: Float>(output: usize, tap: &mut TapFeed<F>, n: usize) -> Result<()> {
    let mut planes = PlanesMut::from_vecs(&mut tap.planes)?;
    let got = tap
        .reader
        .pop_planes(planes.slice_mut(Samples::ZERO..Samples::new(n)))?;
    let lost = tap.reader.take_lost();
    if lost.get() > 0 {
        return Err(RenderError::TapOverrun { output, lost });
    }
    if got.get() < n {
        return Err(RenderError::TapStarved { output });
    }
    Ok(())
}

/// What a normalizing output was given: the gain, and the reading it came from
/// where this output measured itself.
#[derive(Clone, Copy)]
struct Chosen {
    gain: Db,
    measured: Option<Loudness>,
}

/// The stages after the render: measure, write, finalize, commit.
fn finish<F: FftFloat>(
    mut files: Vec<FileLane<F>>,
    buffers: Vec<BufferLane<F>>,
    job: &Job,
    lost: u64,
    control: &mut Control<'_>,
) -> Result<Outcome<(Vec<Written>, Vec<Rendered<F>>)>> {
    let rate = job.rate();
    let mut chosen: Vec<Option<Chosen>> = vec![None; files.len()];
    let waiting: Vec<usize> = (0..files.len())
        .filter(|&i| matches!(files[i].sink, Sink::Spill { .. }))
        .collect();
    for &i in &waiting {
        if let Sink::Spill { waiting, .. } = &mut files[i].sink {
            waiting.spill.finish()?;
        }
    }

    if !waiting.is_empty() {
        let own: Vec<usize> = waiting
            .iter()
            .copied()
            .filter(|&i| matches!(waiting_of(&files[i]).gain, Gain::Own(_)))
            .collect();
        // Measuring: the outputs that choose their own gain, a spill each.
        let total = Samples::new(
            own.iter()
                .map(|&i| waiting_of(&files[i]).spill.frames().get())
                .sum(),
        );
        control.set_output(own.first().map_or(0, |&i| files[i].lane.position));
        control.begin(Stage::Measuring, Some(total));
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let mut done = Samples::new(0);
        for &i in &own {
            let position = files[i].lane.position;
            control.set_output(position);
            let w = waiting_of(&files[i]);
            let Gain::Own(mode) = &w.gain else { continue };
            let normalize = mode.within(&w.target, rate);
            let Outcome::Finished((reading, at)) = measure(
                w,
                position,
                files[i].lane.layout,
                rate,
                normalize,
                done,
                total,
                control,
            )?
            else {
                return Ok(Outcome::Cancelled);
            };
            done = at;
            chosen[i] = Some(Chosen {
                gain: reading.gain(),
                measured: reading.loudness(),
            });
        }
        for &i in &waiting {
            if let Gain::Of(key) = &waiting_of(&files[i]).gain
                && let Some(from) = chosen[key.index()]
            {
                chosen[i] = Some(Chosen {
                    gain: from.gain,
                    measured: None,
                });
            }
        }

        // Writing: every waiting output, read back through its chain with its
        // gain.
        let total = Samples::new(
            waiting
                .iter()
                .map(|&i| waiting_of(&files[i]).spill.frames().get())
                .sum(),
        );
        control.set_output(files[waiting[0]].lane.position);
        control.begin(Stage::Writing, Some(total));
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let mut done = Samples::new(0);
        for &i in &waiting {
            let FileLane {
                lane,
                sink: Sink::Spill { chain, waiting },
                ..
            } = &mut files[i]
            else {
                continue;
            };
            control.set_output(lane.position);
            if let Some(chosen) = chosen[i] {
                chain.set_gain(chosen.gain);
            }
            let block = if waiting.target.resampling().is_some() {
                chunk_frames(rate).get()
            } else {
                REPLAY_BLOCK
            };
            let mut replay = waiting.spill.replay()?;
            let Outcome::Finished(at) = replay_through(
                &mut replay,
                chain,
                block,
                Stage::Writing,
                done,
                total,
                control,
            )?
            else {
                return Ok(Outcome::Cancelled);
            };
            done = at;
        }
    }

    // Finalizing.
    let rendered: Vec<Rendered<F>> = buffers
        .into_iter()
        .map(|b| Rendered::from_render(b.planes, rate, lost))
        .collect();
    let mut pending: Vec<PendingFile<F>> = Vec::with_capacity(files.len());
    let mut spill_left: Vec<Option<PathBuf>> = vec![None; files.len()];
    let placements: Vec<ChannelPlacement> = files.iter().map(|f| f.lane.placement).collect();
    for (i, file) in files.into_iter().enumerate() {
        let chain = match file.sink {
            Sink::Spill { chain, mut waiting } => {
                spill_left[i] = waiting.spill.remove().map(|(path, _)| path);
                chain
            }
            Sink::Stream { chain } => chain,
        };
        pending.push(PendingFile {
            position: file.lane.position,
            path: file.path,
            chain: *chain,
        });
    }
    let Outcome::Finished(landed) = finalize_files(pending, control)? else {
        return Ok(Outcome::Cancelled);
    };
    let written = landed
        .into_iter()
        .enumerate()
        .map(|(i, (path, file))| {
            let mut w = self::written(&path, file, lost, placements[i]);
            w.spill_left = spill_left[i].take();
            if let Some(c) = chosen[i] {
                w.gain = Some(c.gain);
                w.measured = c.measured;
            }
            w
        })
        .collect();
    Ok(Outcome::Finished((written, rendered)))
}

fn waiting_of<F: FftFloat>(file: &FileLane<F>) -> &Waiting<F> {
    match &file.sink {
        Sink::Spill { waiting, .. } => waiting,
        // Only the lanes listed as waiting are asked.
        Sink::Stream { .. } => unreachable!("listed as waiting"),
    }
}

/// Reads `waiting`'s spill at the file's rate through the meter, a chunk of at
/// most a second at a time, and returns the reading and how many frames of
/// `total` the measuring stage has now read.
///
/// The converter is the one the write pass uses, so what is measured is what
/// is written; the reading is refused when it is not a measurement of the
/// programme.
fn measure<F: FftFloat>(
    waiting: &Waiting<F>,
    output: usize,
    layout: ChannelLayout,
    rate: SampleRate,
    normalize: Normalize,
    mut done: Samples,
    total: Samples,
    control: &mut Control<'_>,
) -> Result<Outcome<(Reading, Samples)>> {
    let out_rate = waiting.target.rate(rate);
    let unmeasurable = |reason: Unmeasured| RenderError::Unmeasurable {
        output: Some(output),
        channels: layout,
        sample_rate: out_rate,
        reason,
    };
    let topology = waiting
        .channels
        .topology()
        .ok_or_else(|| unmeasurable(Unmeasured::Placement))?;
    let mut measurer = Measurer::new(out_rate, topology, normalize).map_err(unmeasurable)?;
    let mut converter: Option<Converter<F>> = match waiting.target.resampling() {
        Some(resample) => resample.converter(layout, rate)?,
        None => None,
    };
    let mut replay = waiting.spill.replay()?;
    let chunk = chunk_frames(rate).get();
    let mut block = vec![F::ZERO; layout.interleaved_len(Samples::new(chunk))];
    let mut block = InterleavedMut::new(&mut block, layout)?;
    loop {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let n = replay.fill(block.reborrow())?;
        if n == 0 {
            break;
        }
        let frames = block.as_interleaved().slice(Samples::ZERO..Samples::new(n));
        match converter.as_mut() {
            Some(converter) => converter.push(frames, |out| {
                measurer.feed_interleaved(out.as_interleaved())
            })?,
            None => measurer.feed_interleaved(frames)?,
        }
        done += Samples::new(n);
        control.advance(Stage::Measuring, done, total);
    }
    // The loop above ends on a read that found nothing left, after its check
    // at the top: a cancel on the last chunk's reading is seen there.
    if let Some(converter) = converter.as_mut() {
        converter.finish(|out| measurer.feed_interleaved(out.as_interleaved()))?;
    }
    let reading = measurer.finish();
    reading.check().map_err(unmeasurable)?;
    Ok(Outcome::Finished((reading, done)))
}

/// What a landed file is, for the caller: its size on disk, `0` if it cannot
/// be stat'd.
fn written(
    path: &std::path::Path,
    landed: Landed,
    dropped_events: u64,
    placement: tutti_buffers::Placement,
) -> Written {
    Written {
        path: path.to_path_buf(),
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        frames: landed.frames(),
        dropped_events,
        clipped_samples: landed.clipped(),
        gain: None,
        measured: None,
        placement,
        spill_left: None,
    }
}

/// Reads `replay` back through `chain`: the write pass of a normalized output
/// (a second writer per format is how a crate ends up with a whole-signal
/// encoder that works and a streaming one that does not).
///
/// `block_frames` is how many frames one read takes, and `done` of `total`
/// frames of `stage` were already spent in the pass (by the outputs before
/// this one). The token is checked before every block. A cancelled pass is
/// [`Outcome::Cancelled`].
fn replay_through<F: FftFloat>(
    replay: &mut dyn Replay<F>,
    chain: &mut Chain<F>,
    block_frames: usize,
    stage: Stage,
    done: Samples,
    total: Samples,
    control: &mut Control<'_>,
) -> Result<Outcome<Samples>> {
    let layout = chain.layout();
    let mut block = vec![F::ZERO; layout.interleaved_len(Samples::new(block_frames.max(1)))];
    let mut block = InterleavedMut::new(&mut block, layout)?;
    let mut at = done.get();
    loop {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let n = replay.fill(block.reborrow())?;
        if n == 0 {
            break;
        }
        chain.block(block.as_interleaved().slice(Samples::ZERO..Samples::new(n)))?;
        at += n;
        control.advance(stage, Samples::new(at), total);
    }
    Ok(Outcome::Finished(Samples::new(at)))
}

/// A file output whose frames are all written: its chain, to be finalized.
pub(crate) struct PendingFile<F: FftFloat> {
    /// The output's position among the render's outputs.
    pub(crate) position: usize,
    pub(crate) path: PathBuf,
    pub(crate) chain: Chain<F>,
}

/// Finalizes every file into its temporary, then puts them in place: the token is checked before each output's finalize
/// (between outputs) and once more after the last, before the first rename.
/// The renames are the one step the token does not reach.
///
/// Returns each file that landed with what its chain counted, in the order
/// given. A rename that fails after others landed is
/// [`RenderError::NotCommitted`], naming what landed, which is kept: nothing is
/// rolled back. One that fails before any landed is the rename's own error.
/// The file whose rename failed is kept beside its path and named by the
/// error ([`tutti_codec::EncodeError::Commit`]); every other file that did not land
/// is removed.
pub(crate) fn finalize_files<F: FftFloat>(
    files: Vec<PendingFile<F>>,
    control: &mut Control<'_>,
) -> Result<Outcome<Vec<(PathBuf, Landed)>>> {
    let count = files.len();
    control.set_output(files.first().map_or(0, |f| f.position));
    control.begin(Stage::Finalizing, None);
    if control.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let mut closed = Vec::with_capacity(count);
    for (k, file) in files.into_iter().enumerate() {
        if k > 0 && control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        control.set_output(file.position);
        // A chain keeps a file it could not close whole, and the render has no use
        // for it: it is removed here, so a failed render leaves nothing beside the paths.
        closed.push((file.path, file.chain.finalize().map_err(Kept::discard)?));
        if k + 1 < count {
            control.at(Stage::Finalizing, 0.5 * (k + 1) as f32 / count as f32);
        }
    }
    if count > 0 {
        control.at(Stage::Finalizing, 0.5);
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
    }
    let mut landed = Vec::with_capacity(count);
    for (path, file) in closed {
        match file.commit() {
            Ok(clipped) => landed.push((path, clipped)),
            Err(cause) if landed.is_empty() => return Err(cause.into()),
            Err(cause) => {
                return Err(RenderError::NotCommitted {
                    landed: landed.into_iter().map(|(path, _)| path).collect(),
                    cause,
                });
            }
        }
    }
    // After the last rename: the only reading that is exactly 1.0.
    control.at(Stage::Finalizing, 1.0);
    Ok(Outcome::Finished(landed))
}

#[cfg(test)]
mod tests;
