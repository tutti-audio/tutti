//! What an export renders: the graph.
//!
//! An export renders a `tutti_graph` editor/executor pair ([`RenderGraph`]) and
//! nothing else. It becomes a frame source (`render::driver`), and the gate,
//! resample, dither and encoders downstream see only frames.

use std::sync::Arc;
use tutti_buffers::Float;
use tutti_units::SampleRate;

use tutti_buffers::{ChannelTopology, MaxBlock};
use tutti_graph::GraphTail;
use tutti_graph::{Editor, Executor, LogFilter, Pool, Prepare, RenderMode, RtLogReaders, RtRecord};
use tutti_units::Samples;

use crate::{ConfigError, Result};

/// The largest block, in frames, a graph is prepared for in an export
/// ([`RenderGraph::prepare`]).
///
/// 1024 frames: long enough that the executor's per-block walk is paid rarely
/// on a long bounce, short enough that a block's planes stay in cache.
// A multiple of 64. The sampler's clip readers render each block in one piece
// (scratch sized from `Prepare::max_block`), and tutti-sampler's
// `tests/voice/block_length_invariance.rs` pins that a voice's frames do not
// depend on its blocks.
pub const GRAPH_MAX_BLOCK: MaxBlock = MaxBlock::of(Samples::new(1024));

/// How a [`RenderGraph`] is assembled beyond its pair: the pool the render
/// spreads its blocks over, and what the graph's output channels are.
///
/// `Setup::default()` is the render's own thread and the main output the graph
/// declares, which is what a graph built in a test or a simple host wants.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pool: Option<Arc<dyn Pool>>,
    main_output: Option<ChannelTopology>,
    rt_log: Option<Arc<dyn RtLogSink>>,
}

/// Receives the records a render's nodes log on its audio thread, on the
/// thread that renders.
///
/// The render drains the executor's log rings between blocks and hands each
/// record, in frame order, to [`record`](Self::record). A sink is what lets
/// the export stay free of any logging crate: the host's turns a record into
/// whatever its subscriber takes ([`Setup::rt_log`]).
pub trait RtLogSink: Send + Sync + std::fmt::Debug {
    /// The most verbose level per target this sink takes, read when the
    /// render starts and after each block; a record above it is never
    /// written.
    fn filter(&self) -> LogFilter;

    /// A record a node logged, from the block just rendered.
    fn record(&self, record: &RtRecord);

    /// `count` records were lost since the last report: a full ring, or more
    /// than four in one node call.
    fn lost(&self, count: u64);
}

impl Setup {
    /// A render on its own thread, the output channels as the graph declares
    /// them.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render on `pool` (the parallel executor). Left uncalled, the
    /// render runs on its own thread. The render is bit for bit the same
    /// either way; a pool only spreads each block's independent nodes across
    /// its threads. Give an export its own pool (a `tutti_graph::WorkerPool`),
    /// not the live engine's: a job finds a pool busy with another caller's
    /// and runs on its caller alone.
    #[must_use]
    pub fn pool(mut self, pool: Arc<dyn Pool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Places the file from `main_output` in place of the graph's declared
    /// main output: [`ChannelTopology::unassigned`] for independent buses, a
    /// named topology for speaker feeds. Left uncalled, the file is placed
    /// from the main output the graph declares.
    ///
    /// The render refuses a topology that is not as wide as the graph's
    /// outputs ([`RenderError::TopologyWidth`](crate::RenderError::TopologyWidth)),
    /// or that the file cannot take ([`ConfigError::Placement`]).
    #[must_use]
    pub fn main_output(mut self, main_output: ChannelTopology) -> Self {
        self.main_output = Some(main_output);
        self
    }

    /// Hands what the graph's nodes log on the audio thread to `sink`, drained
    /// between blocks on the render's thread. Left uncalled, the render's log
    /// gate stays off and nothing is written.
    #[must_use]
    pub fn rt_log(mut self, sink: Arc<dyn RtLogSink>) -> Self {
        self.rt_log = Some(sink);
        self
    }
}

/// The graph an export renders: a `tutti_graph` editor/executor pair.
///
/// The pair is already installed (the executor running its plan) and
/// prepared at the render's sample rate: the render refuses any other
/// rate rather than re-rating it, since a unit's preparation is the control
/// side's job. A `RenderGraph` is made one way, [`RenderGraph::assemble`], from
/// a pair that was built together:
///
/// - build it at [`RenderGraph::prepare`] (a `GraphBuilder` in a test or a
///   simple host) and assemble what `build` returns;
/// - or, for a fork of a live graph, `tutti-session` forks, edits and
///   assembles it (the fork machinery is the session's, not this crate's).
///
/// # The pair
///
/// The editor must feed the executor: it is what the render drains of the
/// units and commits the executor retires, and an editor paired with some
/// other executor would never see them. [`assemble`](Self::assemble) checks
/// it, and nothing afterwards can break it: a `RenderGraph` hands out neither
/// half (an edit is made on the pair before it is assembled), so the
/// check is made once, at the one constructor.
///
/// The executor renders in blocks of its prepared `MaxBlock`, each handed the
/// transport the render's clock reports ([`RenderClock::graph_block`]), and
/// the clock is advanced after each block ([`render_graph`]).
/// A clip reader reads the transport at any frame of its block from its
/// `Env`.
///
/// [`RenderClock::graph_block`]: tutti_timeline::RenderClock::graph_block
/// [`render_graph`]: tutti_timeline::render_graph
///
/// # Latency and tail
///
/// [`reported_latency`](Self::reported_latency) and
/// [`declared_tail`](Self::declared_tail) answer from what the graph already
/// holds — the compiled plan's worst-case output latency and the tail fold
/// over its spec. The figures go into the [`Job`](crate::Job), where the leading
/// trim and the tail extension are plain arithmetic.
///
/// # Example
///
/// ```
/// use tutti_timeline::FrozenClock;
/// use tutti_units::{Hz, SampleRate, Samples, Seconds};
/// use tutti_export::{render, BufferTarget, Control, Job, Output, Outputs, RenderGraph, Setup, TailPolicy};
/// use tutti_graph::GraphBuilder;
/// use tutti_nodes::testing::Osc;
/// use tutti_buffers::ChannelLayout;
///
/// let rate = SampleRate::SR_48K;
/// let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
/// let tone = g.add(Osc::sine(Hz::new(440.0)));
/// g.pipe_output(tone);
/// let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
/// let mut graph = RenderGraph::assemble(editor, executor, Setup::new()).expect("built together, so paired");
///
/// let job = Job::new(rate, Seconds::new(0.1).to_samples(rate), Samples::new(0), TailPolicy::Frames(Samples::ZERO)).expect("a render");
/// let mut outputs = Outputs::new();
/// let planes = outputs.buffers(Output::main_output(BufferTarget::new(ChannelLayout::STEREO)));
/// let done = render(&mut graph, &job, &FrozenClock, outputs, &mut Control::none())
///     .expect("renders")
///     .into_finished()
///     .expect("nothing cancels it");
/// let out = done.buffers(&planes).expect("a key of these outputs");
/// assert_eq!(out.len().get(), 4_800);
/// ```
#[derive(Debug)]
pub struct RenderGraph<F: Float = f32> {
    /// The control side. Drained after every block, so what the executor
    /// retires is freed on the render thread.
    editor: Editor<F>,
    /// The executor the render drives. Never handed out: the render is the
    /// only thing that runs it.
    executor: Executor<F>,
    /// The topology the file is placed from in place of the graph's declared
    /// main output ([`Setup::main_output`]).
    main_output: Option<ChannelTopology>,
    /// The executor's `(dropped, recovered)` event counts where the last
    /// render ended, or where the pair was wrapped: the next render counts its
    /// losses from here, so a drop between the two (installing a commit) is
    /// the next render's, and a drop in an earlier render is not.
    settled: (u64, u64),
    /// The executor's log rings and where their records go, when the setup
    /// named a sink.
    rt_log: Option<(RtLogReaders, Arc<dyn RtLogSink>)>,
}

impl RenderGraph {
    /// What a graph is prepared at to render at `sample_rate`: that
    /// rate, [`GRAPH_MAX_BLOCK`], and [`RenderMode::Offline`], so a node
    /// that waits for its inputs or spends more per block for quality does.
    /// It names no sample format, so it is one function for every graph; it
    /// lives on the `f32` default so that `RenderGraph::prepare` needs no
    /// annotation.
    pub fn prepare(sample_rate: SampleRate) -> Prepare {
        Prepare::new(sample_rate, GRAPH_MAX_BLOCK).with_render_mode(RenderMode::Offline)
    }
}

impl<F: Float> RenderGraph<F> {
    /// Assemble an installed editor/executor pair, with its [`Setup`], for a
    /// render. The one constructor.
    ///
    /// A pool the executor already runs on stays unless `setup` names one.
    ///
    /// The pair is already installed: edit and commit the editor before this
    /// (`Editor::commit`, then `Executor::apply_pending` and `Editor::collect`),
    /// because nothing here hands either half out again.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnpairedGraph`] if `editor` does not feed `executor`
    /// (`Editor::is_paired_with`): pass the pair built or forked together.
    /// That the pair's rate is the render's is checked at render, where the
    /// [`Job`](crate::Job) names it ([`ConfigError::RateMismatch`]).
    pub fn assemble(editor: Editor<F>, mut executor: Executor<F>, setup: Setup) -> Result<Self> {
        if !editor.is_paired_with(&executor) {
            return Err(ConfigError::UnpairedGraph.into());
        }
        if setup.pool.is_some() {
            executor.set_pool(setup.pool);
        }
        let settled = (
            executor.dropped_event_count(),
            executor.recovered_event_count(),
        );
        // After the pool: the readers are those of the executor's final sizing.
        let rt_log = setup
            .rt_log
            .map(|sink| (RtLogReaders::new(&mut executor), sink));
        Ok(Self {
            editor,
            executor,
            main_output: setup.main_output,
            settled,
            rt_log,
        })
    }

    /// Where the next render counts its event losses from.
    pub(crate) fn settled(&self) -> (u64, u64) {
        self.settled
    }

    /// The executor's counts at the end of a render: the next one starts here.
    pub(crate) fn settle(&mut self, counts: (u64, u64)) {
        self.settled = counts;
    }

    /// The log readers and their sink ([`Setup::rt_log`]).
    pub(crate) fn rt_log(&self) -> Option<(RtLogReaders, Arc<dyn RtLogSink>)> {
        self.rt_log.clone()
    }

    /// The topology the file is placed from in place of the main output the
    /// plan was committed with ([`Setup::main_output`]).
    pub(crate) fn main_output_override(&self) -> Option<ChannelTopology> {
        self.main_output
    }

    /// Both halves, for the render. Crate-private: outside the crate the
    /// executor is never handed out.
    pub(crate) fn parts_mut(&mut self) -> (&mut Editor<F>, &mut Executor<F>) {
        (&mut self.editor, &mut self.executor)
    }

    /// The look-ahead latency the graph reports, as a frame count — the
    /// figure to trim from the head of a render ([`Job::new`](crate::Job::new)).
    ///
    /// Read from the compiled plan: the worst-case latency across the
    /// graph's outputs (`Plan::total_latency`), the figure its PDC aligned
    /// every output to, at the rate the graph was prepared at. An executor
    /// with no plan installed has nothing to delay, and reports zero.
    ///
    /// It is a method rather than a `LatencyTrim::Reported` mode on the job
    /// because asking a graph is an *action*, and folding it into a value
    /// would drag a graph into arithmetic that is otherwise pure:
    ///
    /// ```
    /// # use tutti_units::{Hz, SampleRate};
    /// # use tutti_export::{Job, RenderGraph, Setup, TailPolicy};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_units::Samples;
    /// # use tutti_buffers::ChannelLayout;
    /// # let rate = SampleRate::SR_48K;
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add(Osc::sine(Hz::new(440.0)));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph::assemble(editor, executor, Setup::new()).unwrap();
    /// let latency = graph.reported_latency();
    /// let job = Job::new(rate, Samples::new(4_800), latency, TailPolicy::Frames(Samples::ZERO));
    /// # let _ = job;
    /// ```
    ///
    /// Whole frames: the plan's latency is a frame count already.
    pub fn reported_latency(&self) -> Samples {
        self.executor
            .plan()
            .map_or(Samples::ZERO, |plan| plan.total_latency().samples())
    }

    /// The tail the graph declares — the most it can keep ringing after its
    /// input stops — with its caveats: the fold over the graph's topology
    /// (`tutti_graph::graph_tail`, at the graph's prepared rate) of each node's
    /// declared ceiling ([`Shape::tail`](tutti_graph::Shape)).
    ///
    /// For a caller that wants a [`Job`](crate::Job)'s tail to be what the
    /// graph's nodes can ring for at most: a reverb, a convolver, a hosted
    /// plugin that declared a decay. A render that spends what the nodes
    /// *report* as the tail renders takes this as its ceiling
    /// ([`TailPolicy::Reported`](crate::TailPolicy::Reported)). It returns the figure and its caveats
    /// rather than a frame count, because for two graphs there is no count:
    /// one that never decays, and one whose nodes were never taught to
    /// answer. Resolving either into a number is a decision, so it happens at
    /// the call site:
    ///
    /// ```
    /// # use tutti_units::{Hz, SampleRate, Seconds};
    /// # use tutti_export::{Job, RenderGraph, Setup, TailPolicy};
    /// # use tutti_graph::GraphBuilder;
    /// # use tutti_nodes::testing::Osc;
    /// # use tutti_units::Samples;
    /// # use tutti_buffers::ChannelLayout;
    /// # let rate = SampleRate::SR_48K;
    /// # let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    /// # let tone = g.add(Osc::sine(Hz::new(440.0)));
    /// # g.pipe_output(tone);
    /// # let (editor, executor) = g.build(RenderGraph::prepare(rate)).unwrap();
    /// # let graph = RenderGraph::assemble(editor, executor, Setup::new()).unwrap();
    /// let declared = graph.declared_tail();
    /// let tail = declared.samples().unwrap_or_else(|| {
    ///     // This bounce stops four seconds into an unbounded tail.
    ///     Seconds::new(4.0).to_samples(rate)
    /// });
    /// let job = Job::new(rate, Samples::new(4_800), Samples::new(0), TailPolicy::Frames(tail));
    /// # let _ = job;
    /// ```
    pub fn declared_tail(&self) -> GraphTail {
        tutti_graph::graph_tail(
            &self.editor.spec().topology,
            self.editor.prepare().sample_rate(),
        )
    }

    /// The tail the graph's units owe now: [`declared_tail`](Self::declared_tail)'s
    /// fold where a node owes what is left of its last
    /// [`report_tail`](tutti_graph::Io::report_tail) (else its declared
    /// ceiling) when nothing upstream still owes, nothing when the executor
    /// skips it as at rest, and its declared ceiling after an upstream that
    /// does ([`Executor::reported_tail`](tutti_graph::Executor::reported_tail)).
    ///
    /// Before a render it is the declared figure. A render reads it itself at
    /// the programme's end and after every block of the tail
    /// ([`TailPolicy::Reported`](crate::TailPolicy::Reported)).
    pub fn reported_tail(&self) -> GraphTail {
        self.executor.reported_tail(&self.editor.spec().topology)
    }
}
