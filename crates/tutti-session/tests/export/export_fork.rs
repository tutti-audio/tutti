//! What a session bounce renders, and what it forks: no pool, a
//! [`Session`] and its [`BounceJob`](tutti_session::BounceJob) driven by hand.
//!
//! An export forks the live graph (`Editor::fork`). The exports are held to
//! the same units wired fresh with `GraphBuilder` (a fork starts reset, which
//! a fresh graph is), to the graph's own latency and tail, and to a prepare
//! hook's edit. The rest pin what only a fork has: it shares nothing with the live graph, it
//! names a node it cannot copy, it is gathered when the export starts and
//! built wherever the job runs, and everything it held is dropped on the
//! main thread.
//!
//! The instruments, the disk voice and
//! the hosted plugin are `export_fork_synth.rs`, `export_fork_sampler.rs` and
//! `export_fork_plugin.rs`.

use crate::bounced::bounced;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tutti_buffers::{InterleavedMut, PlanesMut};

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::ThreadId;
use std::time::Duration;
use tutti_buffers::ChannelLayout;
use tutti_export::Produced;
use tutti_fixtures::block_on;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, ForkBuild, ForkByClone, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts,
    ParamNode, ParamSet, Params, Prepare, RenderMode, Shape, Status,
};
use tutti_nodes::testing::{Const, Osc, Sink};
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_rt::Mutex;
use tutti_rt::Param;
use tutti_session::BufferMainOutput;
use tutti_session::{
    Bounce, BounceConfigError, BounceError, BounceLatency, DetachedSession, ExportClock, InputPort,
    MainOutput, OutputPort, PreparedGraph, Range, Session, Stem,
};
use tutti_units::{Amplitude, Hz, Q, SampleRate, Samples, Seconds, Tail};

const RATE: f64 = 48_000.0;

/// A detached stereo session whose graph is at the export's rate.
fn session() -> DetachedSession {
    session_with_outputs(2)
}

fn session_with_outputs(outputs: usize) -> DetachedSession {
    let mut session = Session::builder()
        .outputs_floor(ChannelLayout::clamped(outputs))
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session
        .graph()
        .set_sample_rate(SampleRate::of(RATE))
        .unwrap();
    session
}

/// A bounce of the mix into memory for `seconds`: stereo at `rate`, on a
/// frozen clock, trimming nothing and keeping no tail (what the figures it
/// does not name come to is each test's to ask for).
fn bounce_at(seconds: f64, rate: f64) -> Bounce<NodeKey, BufferMainOutput> {
    Bounce::new()
        .main_output_buffers()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(seconds)))
        .sample_rate(SampleRate::of(rate))
        .channels(ChannelLayout::STEREO)
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(tutti_session::BounceTail::None)
}

fn buffers(seconds: f64) -> Bounce<NodeKey, BufferMainOutput> {
    bounce_at(seconds, RATE)
}

/// The planes `bounce` renders, run on this thread.
fn render(
    session: &mut DetachedSession,
    bounce: Bounce<NodeKey, BufferMainOutput>,
) -> Vec<Vec<f32>> {
    bounced(session, bounce)
        .expect("the bounce renders")
        .into_main_output()
        .into_planes()
}

/// The planes `bounce` renders for each of its stems, in memory, run on this
/// thread: the main output is rendered with them and not returned.
fn render_stems(
    session: &mut DetachedSession,
    bounce: Bounce<NodeKey, BufferMainOutput>,
) -> Vec<Vec<Vec<f32>>> {
    let (_, stems) = bounced(session, bounce)
        .expect("the bounce renders")
        .into_parts();
    stems
        .into_iter()
        .map(|p| match p {
            Produced::Buffers(rendered) => rendered.into_planes(),
            other => panic!("expected rendered buffers, got {other:?}"),
        })
        .collect()
}

/// The one stem `node` of a bounce of `seconds`: its planes.
fn stem_of(session: &mut DetachedSession, node: NodeKey, seconds: f64) -> Vec<Vec<f32>> {
    render_stems(session, buffers(seconds).stem_with(Stem::new(node)))
        .pop()
        .expect("one stem")
}

/// What the session refuses `bounce` with, started and run on this thread.
fn refused(
    session: &mut DetachedSession,
    bounce: Bounce<NodeKey, BufferMainOutput>,
) -> BounceError {
    match bounced(session, bounce) {
        Err(e) => e,
        Ok(out) => panic!(
            "expected a refusal, the bounce rendered {} stems",
            out.stems().len()
        ),
    }
}

// ---------------------------------------------------------------------------
// What any export renders
// ---------------------------------------------------------------------------

/// A saw through a low-pass on channel 0, the raw saw on channel 1: a main output
/// that differs from a node export of the filter on every channel, so the
/// two cannot pass for each other. Returns the filter's key.
fn chain(session: &mut DetachedSession) -> NodeKey {
    let graph = session.graph();
    let osc = graph.add(ForkByClone(Osc::saw(Hz::new(110.0))));
    let filter = graph.add(SvfFilterNode::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(800.0))
            .q(Q::new(1.0)),
    ));
    graph
        .set_sources(InputPort::new(filter, 0), [OutputPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    graph
        .set_sources(MainOutput::channel(0), [OutputPort::new(filter, 0)])
        .unwrap()
        .unfaded();
    graph
        .set_sources(MainOutput::channel(1), [OutputPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    filter.erase()
}

/// [`chain`]'s units wired fresh, with `GraphBuilder`, and rendered in the
/// export's own blocks for `seconds`: the main output (the filter on channel 0,
/// the saw on channel 1) and the filter alone (the graph's own two channels
/// both the filter, of which a mono stem is the first). A fork starts reset,
/// which is what a fresh graph is.
fn chain_fresh(seconds: f64) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    use tutti_export::{
        BufferTarget, Control, Job, Output, Produced, RenderGraph, Setup, render_one,
    };
    use tutti_graph::GraphBuilder;
    use tutti_timeline::FrozenClock;
    let render = |main_output: bool| {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let osc = g.add(Osc::saw(Hz::new(110.0)));
        let (filter, _params) = g.add_with_controls(SvfFilterNode::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(800.0))
                .q(Q::new(1.0)),
        ));
        g.connect(osc, 0, filter, 0).connect_output(filter, 0, 0);
        g.connect_output(if main_output { osc } else { filter }, 0, 1);
        let (editor, executor) = g
            .build(RenderGraph::prepare(SampleRate::of(RATE)))
            .expect("builds");
        let mut graph =
            RenderGraph::assemble(editor, executor, Setup::new()).expect("built together");
        let rate = SampleRate::of(RATE);
        render_one(
            &mut graph,
            &Job::new(
                rate,
                Seconds::new(seconds).to_samples(rate),
                Samples::new(0),
                tutti_export::TailPolicy::Frames(Samples::ZERO),
            )
            .expect("a render"),
            &FrozenClock,
            Output::main_output(BufferTarget::new(ChannelLayout::STEREO)),
            &mut Control::none(),
        )
        .expect("renders")
        .into_finished()
        .and_then(Produced::into_buffers)
        .expect("nothing cancels a control with no token")
        .into_planes()
    };
    (render(true), render(false))
}

/// **An export renders its graph as wired fresh**, for the main output and for
/// one node's stem, on a graph whose live side has not run: bit for bit the same
/// units in a `GraphBuilder` graph ([`chain_fresh`]).
///
/// A fork starts reset rather than copying the running state (an
/// oscillator's phase, a filter's memory); with the live side never rendered
/// that makes no difference, so the comparison is like for like.
///
#[test]
fn exports_render_their_graph_as_wired_fresh() {
    let mut session = session();
    let filter = chain(&mut session);
    let forked_main_output = render(&mut session, buffers(0.25));
    let forked_node = stem_of(&mut session, filter, 0.25);
    let (fresh_main_output, fresh_node) = chain_fresh(0.25);
    // The stem is the node's own width: one channel, not the main output's two.
    assert_eq!(forked_node.len(), 1, "the stem is the mono node's width");
    for (what, fresh, forked) in [
        (
            "main_output",
            &fresh_main_output[..],
            &forked_main_output[..],
        ),
        ("node", &fresh_node[..1], &forked_node[..]),
    ] {
        assert_eq!(fresh.len(), forked.len(), "{what}: width");
        let peak = tutti_fixtures::signal::peak(&forked[0]);
        assert!(peak > 0.1, "{what}: silent");
        for (c, (a, b)) in fresh.iter().zip(forked).enumerate() {
            assert_eq!(a.len(), b.len(), "{what}: length of channel {c}");
            if let Some(i) = a
                .iter()
                .zip(b)
                .position(|(x, y)| x.to_bits() != y.to_bits())
            {
                panic!(
                    "{what}: channel {c} parts at frame {i}: fresh {} export {}",
                    a[i], b[i]
                );
            }
        }
    }
    // And the stem is the node: the filter, which is the main output's channel 0
    // and not its channel 1, the raw saw.
    assert_eq!(forked_node[0], forked_main_output[0]);
    assert_ne!(forked_main_output[1], forked_main_output[0]);
}

/// A source that steps from 0 to 1 at frame `LATE` and declares `LATE`
/// frames of latency, with a finite tail of `TAIL` frames: a look-ahead
/// processor reduced to its bookkeeping.
#[derive(Clone)]
struct Late {
    pos: usize,
}

const LATE: usize = 100;
const TAIL: usize = 300;

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .latency(tutti_units::Latency::new(Samples::new(LATE)))
            .tail(Tail::Finite(Samples::new(TAIL)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = if self.pos >= LATE { 1.0 } else { 0.0 };
            self.pos += 1;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.pos = 0;
    }
}

/// **`trim_reported_latency` and `with_reported_tail` take the figures from
/// the graph that is rendered**: the step a `LATE`-frame source makes at its
/// declared latency lands on frame 0, and the render runs `TAIL` frames
/// past its duration. Asked of the forked plan.
///
/// The tail is resolved by `GraphTail::resolve`: a finite tail under the cap
/// is rendered whole, and a graph whose only node never said (`Tail::Unknown`)
/// renders none — not the cap, which would
/// append silence.
#[test]
fn latency_and_tail_come_from_the_graph() {
    let mut session = session();
    let late = session.graph().add(ForkByClone(Late { pos: 0 }));
    session.graph().connect(late, MainOutput).unwrap().unfaded();
    let seconds = 0.01;
    let frames = (seconds * RATE) as usize;
    let planes = render(
        &mut session,
        buffers(seconds)
            .latency(BounceLatency::Graph)
            .tail(tutti_session::BounceTail::Graph {
                cap: Samples::new(48_000).to_seconds(SampleRate::of(RATE)),
            }),
    );
    assert_eq!(planes[0].len(), frames + TAIL, "the tail");
    assert_eq!(planes[0][0], 1.0, "the latency was not trimmed");
    assert!(planes[0].iter().all(|&s| s == 1.0), "a step from frame 0");

    let mut session = self::session();
    let unknown = session.graph().add(Counter {
        n: Arc::new(AtomicU64::new(0)),
    });
    session
        .graph()
        .connect(unknown, MainOutput)
        .unwrap()
        .unfaded();
    let planes = render(
        &mut session,
        buffers(seconds).tail(tutti_session::BounceTail::Graph {
            cap: Samples::new(4_800).to_seconds(SampleRate::of(RATE)),
        }),
    );
    assert_eq!(
        planes[0].len(),
        frames,
        "an unknown tail renders none, not the cap"
    );
}

/// **A bounce of a reverb with live cells spends the tail the reverb reports
/// for the room it is set to** (the node declares the largest room's, about
/// nine seconds): a constant fed through a room of 0, the General MIDI room
/// and a room of 0.9 ends at the programme's frames and the bank's own
/// `tail_of` for those controls, long by under a block (the executor counts
/// whole quiet blocks). Whether that report covers the ring-out is the
/// bank's own test. The constant owes nothing itself and is not at rest at
/// the programme's end, so the reverb owes its report in full (a source that
/// still owed would make it owe its ceiling after it).
#[test]
fn a_bounce_ends_at_the_tail_the_reverb_reports_for_its_room() {
    let seconds = 0.2;
    let frames = (seconds * RATE) as usize;
    let block = 1024;
    let bank = tutti_dsp::FreeverbBank::<f32>::new(SampleRate::of(RATE));
    let mut lengths = Vec::new();
    for room in [0.0, 0.5, 0.9] {
        let mut session = session();
        let src = session.graph().add(ForkByClone(Const::mono(0.5)));
        let verb = session.graph().add(tutti_nodes::FreeverbNode::<f32>::from(
            tutti_nodes::FreeverbConfig::new(),
        ));
        session.graph().connect(src, verb).unwrap().unfaded();
        session.graph().connect(verb, MainOutput).unwrap().unfaded();
        session
            .set(verb.room_size().to(tutti_units::Mix::new(room)))
            .unwrap();
        let controls = tutti_nodes::FreeverbControls {
            room: tutti_units::Mix::new(room),
            ..tutti_nodes::FreeverbControls::GENERAL_MIDI
        };
        let Tail::Finite(report) = bank.tail_of(&controls) else {
            panic!("room {room}: a finite report")
        };
        let planes = render(
            &mut session,
            buffers(seconds).tail(tutti_session::BounceTail::default()),
        );
        let got = planes[0].len() - frames;
        assert!(
            (report.get()..report.get() + block).contains(&got),
            "room {room}: rang {got} frames, the reverb reports {report:?}"
        );
        lengths.push(got);
    }
    assert!(
        lengths.is_sorted_by(|a, b| a < b),
        "a shorter room ends before a longer one: {lengths:?}"
    );
}

/// **The latency trimmed is the latency of the graph rendered — after the
/// `prepare` hook.** The live graph is a latency-free constant; the hook
/// puts a `LATE`-frame source on the output. Trimmed by what the hook left,
/// the source's step lands on frame 0; by the live graph's figure (0), it
/// would land on frame `LATE`.
#[test]
fn the_trim_is_read_after_the_prepare_hook() {
    let mut session = session();
    let dc = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(dc, MainOutput).unwrap().unfaded();
    let request = buffers(0.01).latency(BounceLatency::Graph).edit(Box::new(
        |prepared: PreparedGraph<'_>| {
            let key = prepared.fresh_key();
            let editor = prepared.editor;
            editor.insert(key, "test:late", ForkByClone(Late { pos: 0 }));
            let width = editor.spec().topology.outputs.len();
            editor
                .set_outputs(vec![
                    tutti_graph::PortSource::Node(
                        tutti_graph::OutputPort::new(key, 0)
                    );
                    width
                ])
                .expect("the node exists");
        },
    ));
    let planes = render(&mut session, request);
    assert_eq!(
        planes[0][0], 1.0,
        "trimmed by the live graph's latency, not the rendered one's"
    );
}

/// **A prepare hook that swaps the fork's editor for another graph's fails the
/// export; it is not rendered**: the editor would never see what the fork's
/// executor retires. The hook holds `&mut Editor`, so `mem::replace` is
/// writable; the job checks the pair before it seals it.
#[test]
fn a_prepare_hook_that_swaps_the_editor_fails_the_export() {
    let mut session = session();
    let dc = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(dc, MainOutput).unwrap().unfaded();
    let request = buffers(0.01).edit(Box::new(|prepared: PreparedGraph<'_>| {
        let (other, _executor) = tutti_graph::Editor::new(Prepare::new(
            SampleRate::of(RATE),
            tutti_export::GRAPH_MAX_BLOCK,
        ));
        drop(std::mem::replace(prepared.editor, other));
    }));
    match refused(&mut session, request) {
        BounceError::Config(BounceConfigError::PrepareHookUnpaired) => {}
        other => panic!("expected the unpaired refusal, got {other:?}"),
    }
    assert!(
        bounced(&mut session, buffers(0.01)).is_ok(),
        "the token is released"
    );
}

/// **An export of a graph with no outputs says so**, for the main output and for
/// a bounce that also names a stem — not that the stem's node is unheard.
#[test]
fn a_graph_with_no_outputs_says_so() {
    let mut session = session_with_outputs(0);
    let node = session.graph().add(ForkByClone(Osc::sine(Hz::new(220.0))));
    for with_stem in [false, true] {
        let bounce = if with_stem {
            buffers(0.01).stem_with(Stem::new(node.erase()))
        } else {
            buffers(0.01)
        };
        match refused(&mut session, bounce) {
            BounceError::Config(BounceConfigError::GraphHasNoOutputs) => {}
            other => {
                panic!("stem {with_stem}: expected a refusal naming the outputs, got {other:?}")
            }
        }
    }
}

/// **A stem that cannot be rendered is refused by name, before anything is
/// forked**: a node that is not in the graph, one no output hears (a sink with
/// no outputs of its own, and a source that is merely unwired: the fork holds
/// what the outputs reach, so it renders nothing there), and a channel the
/// node has not (or none at all). A node the outputs do hear is rendered.
#[test]
fn an_unrenderable_stem_is_refused_by_name() {
    let mut session = session();
    let wired = session.graph().add(ForkByClone(Const::mono(0.5)));
    session
        .graph()
        .connect(wired, MainOutput)
        .unwrap()
        .unfaded();
    // `Sink::mono()` consumes one channel and produces nothing.
    let orphan = session.graph().add(ForkByClone(Sink::mono()));
    let loose = session.graph().add(ForkByClone(Const::mono(0.25)));
    let gone = NodeKey::fresh();
    let refusal = |session: &mut DetachedSession,
                   stem: Stem<NodeKey, tutti_session::BufferStem>| {
        match refused(session, buffers(0.01).stem_with(stem)) {
            BounceError::Config(why) => why,
            other => panic!("expected a refusal by name, got {other:?}"),
        }
    };
    assert_eq!(
        refusal(&mut session, Stem::new(gone)),
        BounceConfigError::StemNotInGraph { node: gone }
    );
    for node in [orphan.erase(), loose.erase()] {
        assert_eq!(
            refusal(&mut session, Stem::new(node)),
            BounceConfigError::StemNotAudible { node }
        );
    }
    assert_eq!(
        refusal(&mut session, Stem::new(wired.erase()).channels([0, 1])),
        BounceConfigError::StemChannelMissing {
            node: wired.erase(),
            channel: 1
        }
    );
    assert_eq!(
        refusal(&mut session, Stem::new(wired.erase()).channels([])),
        BounceConfigError::StemChannelMissing {
            node: wired.erase(),
            channel: 0
        }
    );
    // Nothing was left in flight by a refusal, and the heard node renders.
    let planes = stem_of(&mut session, wired.erase(), 0.01);
    assert!(planes[0].iter().all(|&s| s == 0.5), "a heard node renders");
}

// ---------------------------------------------------------------------------
// What a prepare hook sees and reaches
// ---------------------------------------------------------------------------

/// A one-second-free session with a constant 0.5 on the outputs: a graph a
/// hook's edit is told apart from by its level.
fn graph_with_one_node_on() -> (DetachedSession, NodeKey) {
    let mut session = session();
    let node = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(node, MainOutput).unwrap().unfaded();
    (session, node.erase())
}

fn short() -> Bounce<NodeKey, BufferMainOutput> {
    bounce_at(0.05, 44_100.0)
}

/// The `prepare` hook runs on the graph that is actually rendered, before
/// the render.
///
/// Pinned by *audio*, not by a call counter: a hook that runs against some
/// other graph, or after the job was built, would leave the rendered signal
/// at the graph's own 0.5 rather than the 0.25 this one writes. The hook
/// edits the fork's editor and does not commit: the session does.
#[test]
fn a_prepare_hook_reaches_the_graph_that_gets_rendered() {
    let (mut session, _node) = graph_with_one_node_on();
    let level = 0.25f32;
    let request = short().edit(Box::new(move |prepared: PreparedGraph<'_>| {
        // Replace the whole graph's output with a constant.
        let key = prepared.fresh_key();
        let editor = prepared.editor;
        editor.insert(key, "test:level", Const::mono(level));
        let width = editor.spec().topology.outputs.len();
        editor
            .set_outputs(vec![
                tutti_graph::PortSource::Node(
                    tutti_graph::OutputPort::new(key, 0)
                );
                width
            ])
            .expect("the node exists");
    }));
    let planes = render(&mut session, request);
    let peak = planes[0].iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    assert_eq!(
        (peak * 1000.0).round() as usize,
        250,
        "the edit must run on the graph that is rendered — an unmodified graph \
         would come back at the 0.5 it was built with"
    );
}

/// **The `prepare` hook is handed the caller's timeline**, for a bounce with a stem.
///
/// `RenderClock` is advance-only, so the session cannot read a timeline back
/// out of the `clock` a caller supplies. Handing the hook a timeline of its
/// own (say 120 BPM at beat 0) while the renderer advanced the caller's would
/// make the two disagree: a node the hook places on a 90 BPM project would
/// land on a 120 BPM playhead that nothing advances.
#[test]
fn the_prepare_hook_sees_the_callers_timeline() {
    use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};

    let (mut session, node) = graph_with_one_node_on();
    // A deliberately un-default transport: neither 120 BPM nor beat 0.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_units::Beat::new(16.0),
        tempo_map: Arc::new(
            tutti_tempo::TempoMap::constant(tutti_units::Bpm::of(90.0)).expect("a tempo"),
        ),
        sample_rate: SampleRate::SR_44K1,
        loop_range: None,
    }));
    let seen: Arc<Mutex<Option<(usize, usize)>>> = Arc::default();
    let into = Arc::clone(&seen);
    let request = bounce_at(0.05, 44_100.0)
        .stem_with(Stem::new(node))
        .clock(ExportClock::timeline(timeline))
        .edit(Box::new(move |prepared: PreparedGraph<'_>| {
            let transport = prepared.ctx;
            *into.lock() = Some((
                transport
                    .tempo_map()
                    .tempo_at(transport.beat())
                    .get()
                    .round() as usize,
                transport.beat().get().round() as usize,
            ));
        }));
    render(&mut session, request);
    assert_eq!(
        *seen.lock(),
        Some((90, 16)),
        "the hook must see the caller's timeline — 90 BPM at beat 16 — not a manufactured 120 BPM one at beat 0"
    );
}

/// **A main output export's `prepare` hook sees the caller's timeline too.**
#[test]
fn a_main_output_exports_prepare_hook_sees_the_callers_timeline() {
    use tutti_timeline::{OfflineTimeline, OfflineTimelineConfig};

    let (mut session, _node) = graph_with_one_node_on();
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_units::Beat::new(4.0),
        tempo_map: Arc::new(
            tutti_tempo::TempoMap::constant(tutti_units::Bpm::of(90.0)).expect("a tempo"),
        ),
        sample_rate: SampleRate::SR_44K1,
        loop_range: None,
    }));
    let seen: Arc<Mutex<Option<usize>>> = Arc::default();
    let into = Arc::clone(&seen);
    let request = short()
        .clock(ExportClock::timeline(timeline))
        .edit(Box::new(move |prepared: PreparedGraph<'_>| {
            *into.lock() = Some(
                prepared
                    .ctx
                    .tempo_map()
                    .tempo_at(prepared.ctx.beat())
                    .get()
                    .round() as usize,
            );
        }));
    render(&mut session, request);
    assert_eq!(*seen.lock(), Some(90), "the caller's 90 BPM");
}

// ---------------------------------------------------------------------------
// What a fork has and a plain clone would not
// ---------------------------------------------------------------------------

/// A source that counts frames in a cell its clones **share**, and whose
/// fork source gives a fork a fresh copy of: a node whose live state a copy
/// could move. Its output is the count, so a render that touched the live
/// cell shows as a jump in the live signal. Its tail is `Tail::Unknown`: it
/// never says.
#[derive(Clone)]
struct Counter {
    n: Arc<AtomicU64>,
}

/// [`Counter`]'s fork: a cell of its own, reset (a fork is reset).
struct CounterFork;

impl ForkSource for CounterFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked, ForkCause> {
        let n = Arc::new(AtomicU64::new(0));
        Ok(Forked::new(Box::new(Counter { n })))
    }
}

impl IntoNode for Counter {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts::new(Box::new(self), (), Some(Box::new(CounterFork)), None)
    }
}

impl Node for Counter {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = self.n.fetch_add(1, Ordering::Relaxed) as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.n.store(0, Ordering::Relaxed);
    }
}

/// **Live playback is untouched by an export rendering beside it.** The live
/// graph's audio side renders on its own thread, block after block, from
/// before the export starts until after it reports; the export renders a
/// fork of the same graph meanwhile. The live signal counts on without a
/// jump, and the export counts from 0 (a fork is reset) — the two never
/// shared the counter.
///
/// A plain clone would share the counter with the live graph, and fail this
/// test by design.
#[test]
fn live_playback_continues_unaffected_while_an_export_renders() {
    let mut session = session();
    let node = session.graph().add(Counter {
        n: Arc::new(AtomicU64::new(0)),
    });
    session.graph().connect(node, MainOutput).unwrap().unfaded();
    // Commit on this side, then hand the audio side to the "device".
    session.render_frame(InterleavedMut::one_frame(&mut [0.0, 0.0]).unwrap());
    let mut live = session.graph().take_audio_side();

    let done = AtomicBool::new(false);
    let (heard, rendered) = std::thread::scope(|s| {
        let device = s.spawn(|| {
            let mut heard = Vec::new();
            let mut block = vec![vec![0.0; 256]; 2];
            // At least a few blocks after the export reports, so the two
            // overlap from end to end.
            let mut after = 0;
            while after < 8 {
                live.render(256, PlanesMut::from_vecs(&mut block).unwrap());
                heard.extend_from_slice(&block[0]);
                if done.load(Ordering::Acquire) {
                    after += 1;
                }
            }
            heard
        });
        let rendered = render(&mut session, buffers(1.0));
        done.store(true, Ordering::Release);
        (device.join().expect("the live side"), rendered)
    });

    for (i, &s) in rendered[0].iter().enumerate() {
        assert_eq!(s, i as f32, "the export's count at frame {i}");
    }
    let first = heard[0];
    for (i, &s) in heard.iter().enumerate() {
        assert_eq!(
            s,
            first + i as f32,
            "the live count jumped at frame {i} while the export ran"
        );
    }
}

/// A source no fork may take, as a microphone monitor is inserted
/// (`tutti_graph::Unforkable`): its clones would share the live ring.
struct Mic;

impl Node for Mic {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.5);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A main output export forks what the outputs hear**: an unforkable node no
/// output reaches (an unrouted mic) does not refuse it, and the render is
/// the routed node's.
#[test]
fn an_unrouted_unforkable_node_does_not_refuse_a_main_output_export() {
    let mut session = session();
    let _mic = session.graph().add(tutti_graph::Unforkable(Mic));
    let dc = session.graph().add(ForkByClone(Const::mono(0.25)));
    session.graph().connect(dc, MainOutput).unwrap().unfaded();
    let planes = render(&mut session, buffers(0.01));
    assert!(planes[0].iter().all(|&s| s == 0.25));
}

/// **A node that cannot be forked refuses the export by its key**, which a
/// host maps back to whatever it bound to the node; and so does a bounce
/// whose stem names its sibling, since a stem renders in the main output's fork,
/// which holds every node an output hears.
#[test]
fn an_unforkable_node_refuses_the_export_by_key() {
    let mut session = session();
    let mic = session.graph().add(tutti_graph::Unforkable(Mic));
    let osc = session.graph().add(ForkByClone(Osc::sine(Hz::new(220.0))));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(mic, 0)])
        .unwrap()
        .unfaded();
    session
        .graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(osc, 0)])
        .unwrap()
        .unfaded();

    match refused(&mut session, buffers(0.05)) {
        BounceError::NotForkable { node: key } => {
            assert_eq!(key, mic.erase());
            assert!(
                tutti_export::RenderError::NotForkable { key }
                    .to_string()
                    .contains(&format!("{:?}", mic.erase())),
                "the message names it"
            );
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
    match refused(
        &mut session,
        buffers(0.05).stem_with(Stem::new(osc.erase())),
    ) {
        BounceError::NotForkable { node: key } => assert_eq!(key, mic.erase()),
        other => panic!("expected the same named refusal, got {other:?}"),
    }
}

/// **A live audio input refuses the bounce by its key.** An inlet's frames
/// come from a thread of its own, so a fork of it would render silence where
/// the signal is: the bounce is refused naming the inlet, not written as a
/// file that looks complete.
#[test]
fn a_live_audio_inlet_refuses_the_bounce_by_key() {
    let mut session = session();
    let (ring, _producer) = tutti_graph::InletRing::pair(
        ChannelLayout::STEREO,
        Samples::new(256),
        SampleRate::of(RATE),
    );
    let inlet = session.graph().add_inlet(ring).expect("the graph's rate");
    session
        .graph()
        .connect(inlet, MainOutput)
        .unwrap()
        .unfaded();
    match refused(&mut session, buffers(0.05)) {
        BounceError::NotForkable { node } => assert_eq!(node, inlet.erase()),
        other => panic!("expected the inlet's refusal by key, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The fork is gathered when the export starts and built where the job runs
// ---------------------------------------------------------------------------

/// A constant the fork renders: a value the gather read.
#[derive(Clone)]
struct Held(f32);

impl Node for Held {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A live node whose level is a cell the host moves (`f32` bits). Its
/// fork is slow, as a hosted plugin's `plugin-server` launch is: the
/// gather reads the cell, and the deferred build waits on `wait` (at
/// most three seconds) before it makes a [`Held`] of that level.
struct Slow {
    level: Arc<AtomicU32>,
    wait: Mutex<Option<Receiver<()>>>,
    built: Arc<AtomicBool>,
}

impl Node for Slow {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0)
            .fill(f32::from_bits(self.level.load(Ordering::SeqCst)));
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct SlowFork {
    level: Arc<AtomicU32>,
    wait: Mutex<Option<Receiver<()>>>,
    built: Arc<AtomicBool>,
}

impl ForkSource for SlowFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked, ForkCause> {
        unreachable!("an export gathers")
    }

    fn gather(&self, _mode: RenderMode) -> Result<ForkBuild, ForkCause> {
        // Read now, when the export starts.
        let level = f32::from_bits(self.level.load(Ordering::SeqCst));
        let wait = self.wait.lock().take();
        let built = Arc::clone(&self.built);
        Ok(ForkBuild::deferred(move || {
            if let Some(wait) = wait {
                let _ = wait.recv_timeout(Duration::from_secs(3));
            }
            built.store(true, Ordering::SeqCst);
            Ok(Forked::new(Box::new(Held(level))))
        }))
    }
}

impl IntoNode for Slow {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        let fork = SlowFork {
            level: Arc::clone(&self.level),
            wait: Mutex::new(self.wait.lock().take()),
            built: Arc::clone(&self.built),
        };
        NodeParts::new(Box::new(self), (), Some(Box::new(fork)), None)
    }
}

/// A slow node at `level`, its build released by the returned sender.
fn slow(level: f32) -> (Slow, Arc<AtomicU32>, Sender<()>, Arc<AtomicBool>) {
    let cell = Arc::new(AtomicU32::new(level.to_bits()));
    let built = Arc::new(AtomicBool::new(false));
    let (release, wait) = channel();
    let node = Slow {
        level: Arc::clone(&cell),
        wait: Mutex::new(Some(wait)),
        built: Arc::clone(&built),
    };
    (node, cell, release, built)
}

/// Outputs its volume, a param the host sets by address: a
/// `ParamNode`, whose fork reads the value last set at the gather.
#[derive(Clone)]
struct Level(Param<Amplitude>);

impl Node for Level {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0.load().get() as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Level {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.0.detach();
        fork
    }
}

tutti_graph::declare_params!([] Level, n { Volume => Amplitude = &n.0 });

/// **An export does not wait for a slow fork, and is the graph as it was
/// when it started.** A node whose fork build blocks until released (the
/// shape of a hosted plugin's `plugin-server` launch) is exported beside
/// a `ParamNode`. `Session::bounce` returns with the build not yet run (it
/// gathers; the job builds); there the live graph is edited — the slow
/// node's cell and the param moved, the outputs rewired to another node —
/// and only then is the build released and the job run on a thread of its
/// own. The export renders the graph at the start: the slow node's level and
/// the param's value then, on the outputs as they were.
///
/// The gather's own guarantee is pinned in tutti-graph's
/// `a_gathered_fork_is_the_live_graph_at_the_gather`.
#[test]
fn an_export_does_not_wait_for_a_slow_fork_and_takes_no_later_edit() {
    let mut session = session();
    let (node, cell, release, built) = slow(0.5);
    let slow_node = session.graph().add(node);
    let level = session
        .graph()
        .add(Params(Level(Param::new(Amplitude::new(0.5)))));
    let volume = level.volume();
    session.set(volume.to(Amplitude::new(0.75))).unwrap();
    let other = session.graph().add(ForkByClone(Const::mono(9.0)));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(slow_node, 0)])
        .unwrap()
        .unfaded();
    session
        .graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(level, 0)])
        .unwrap()
        .unfaded();

    let job = session.bounce(buffers(0.05)).expect("the export starts");
    assert!(
        !built.load(Ordering::SeqCst),
        "the fork's build had run by the time the export started: it does not wait for the build"
    );

    // Edits after the start: none reaches the fork.
    cell.store(0.25f32.to_bits(), Ordering::SeqCst);
    session.set(volume.to(Amplitude::new(0.25))).unwrap();
    session
        .graph()
        .connect(other, MainOutput)
        .unwrap()
        .unfaded();
    let _ = release.send(());

    let rendered = block_on(job);
    let planes = session
        .apply(rendered)
        .expect("the export renders")
        .into_main_output()
        .into_planes();
    assert!(built.load(Ordering::SeqCst), "the build ran");
    assert!(
        planes[0].iter().all(|&s| s == 0.5),
        "the slow node's level at the start, on output 0: {:?}",
        &planes[0][..4]
    );
    assert!(
        planes[1].iter().all(|&s| s == 0.75),
        "the param's value at the start, on output 1: {:?}",
        &planes[1][..4]
    );
}

/// Where a [`Pinned`] unit was built, prepared and dropped.
#[derive(Default)]
struct Threads {
    built: tutti_fixtures::Record<ThreadId>,
    prepared: tutti_fixtures::Record<ThreadId>,
    dropped: tutti_fixtures::Record<ThreadId>,
}

fn here(at: &tutti_fixtures::Record<ThreadId>) {
    at.lock().unwrap().push(std::thread::current().id());
}

/// A unit confined to the main thread, as an in-process VST2 fork is:
/// its source is pinned (`ForkBuild::pinned`), and it records where it
/// is built, prepared and dropped.
struct Pinned {
    threads: Arc<Threads>,
}

impl Node for Pinned {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {
        here(&self.threads.prepared);
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(1.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl Drop for Pinned {
    fn drop(&mut self) {
        here(&self.threads.dropped);
    }
}

struct PinnedFork(Arc<Threads>);

impl ForkSource for PinnedFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked, ForkCause> {
        here(&self.0.built);
        Ok(Forked::new(Box::new(Pinned {
            threads: Arc::clone(&self.0),
        })))
    }

    fn gather(&self, mode: RenderMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::pinned)
    }
}

/// A live [`Pinned`] (recording nothing) whose forks record into
/// `threads`.
struct PinnedNode(Arc<Threads>);

impl IntoNode for PinnedNode {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts::new(
            Box::new(Pinned {
                threads: Arc::default(),
            }),
            (),
            Some(Box::new(PinnedFork(self.0))),
            None,
        )
    }
}

fn all_main(what: &str, at: &tutti_fixtures::Record<ThreadId>) {
    let main = std::thread::current().id();
    let at = at.lock().unwrap();
    assert!(!at.is_empty(), "the fork was never {what}");
    assert!(
        at.iter().all(|&t| t == main),
        "the fork was {what} off the main thread: {at:?} (main {main:?})"
    );
}

/// **A main-thread-only fork source builds on the main thread** though the
/// job builds the rest of its fork on the thread that runs it: a pinned
/// source (an in-process VST2 plugin's) is built and prepared in the gather,
/// in `Session::bounce`, and the unit it built is dropped by
/// `apply`, on the main thread, after the render on a thread of its own. A slow deferred source beside it keeps the job busy for a while.
#[test]
fn a_main_thread_only_fork_builds_and_drops_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let pinned = session.graph().add(PinnedNode(Arc::clone(&threads)));
    let (node, _cell, release, _built) = slow(0.5);
    let slow_node = session.graph().add(node);
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(slow_node, 0)])
        .unwrap()
        .unfaded();
    session
        .graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(pinned, 0)])
        .unwrap()
        .unfaded();
    drop(release); // the slow build goes straight through

    let job = session.bounce(buffers(0.05)).expect("the export starts");
    let rendered = block_on(job);
    let planes = session
        .apply(rendered)
        .expect("the export renders")
        .into_main_output()
        .into_planes();
    assert!(
        planes[1].iter().all(|&s| s == 1.0),
        "the pinned unit renders"
    );
    all_main("built", &threads.built);
    all_main("prepared", &threads.prepared);
    all_main("dropped", &threads.dropped);
}

/// **A job dropped unrun drops its fork on the main thread**, wherever it is
/// dropped: an export whose fork holds a pinned unit (built in the gather)
/// is handed to a thread that drops it unrun (as a cancelled task is dropped
/// by a pool); whatever the job held goes back to the session, and
/// the next `update` drops it on the main thread. No render happened.
#[test]
fn a_dropped_export_job_drops_its_fork_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let pinned = session.graph().add(PinnedNode(Arc::clone(&threads)));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(pinned, 0)])
        .unwrap()
        .unfaded();
    session
        .graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(pinned, 0)])
        .unwrap()
        .unfaded();

    let job = session.bounce(buffers(0.05)).expect("the export starts");
    // Dropped on another thread, never run.
    std::thread::spawn(move || drop(job))
        .join()
        .expect("the dropping thread");
    assert!(
        threads.dropped.lock().unwrap().is_empty(),
        "the pinned unit is held for the main thread, not dropped where the job was"
    );
    let _events = session.update();
    all_main("built", &threads.built);
    all_main("dropped", &threads.dropped);
    assert!(
        session.bounce(buffers(0.05)).is_ok(),
        "the token went with the job"
    );
}

/// Why a [`Broken`] fork's build fails: a type to downcast back out.
#[derive(Debug, PartialEq)]
struct NoServer;

impl std::fmt::Display for NoServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server would not start")
    }
}

impl std::error::Error for NoServer {}

/// A node whose fork's deferred build fails, as a hosted plugin's does
/// when its `plugin-server` will not start.
struct Broken;

impl Node for Broken {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

struct BrokenFork;

impl ForkSource for BrokenFork {
    fn fork(&self, _mode: RenderMode) -> Result<Forked, ForkCause> {
        unreachable!("an export gathers")
    }
    fn gather(&self, _mode: RenderMode) -> Result<ForkBuild, ForkCause> {
        Ok(ForkBuild::deferred(|| Err(ForkCause::new(NoServer))))
    }
}

impl IntoNode for Broken {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts::new(Box::new(self), (), Some(Box::new(BrokenFork)), None)
    }
}

/// **A fork whose build fails on the job's thread refuses the export by its
/// node's key**, its cause kept whole, and what the build had made (a pinned
/// unit, built in the gather) is dropped on the main thread, as a rendered
/// graph is.
#[test]
fn a_fork_build_that_fails_names_its_node_and_drops_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let pinned = session.graph().add(PinnedNode(Arc::clone(&threads)));
    let broken = session.graph().add(Broken);
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(pinned, 0)])
        .unwrap()
        .unfaded();
    session
        .graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(broken, 0)])
        .unwrap()
        .unfaded();

    let job = session
        .bounce(buffers(0.05))
        .expect("the export starts: the build fails in the job");
    let rendered = block_on(job);
    match session.apply(rendered) {
        Err(BounceError::Fork(tutti_graph::ForkError::Source { key, cause })) => {
            assert_eq!(key, broken.erase());
            assert_eq!(cause.downcast_ref::<NoServer>(), Some(&NoServer));
        }
        other => panic!("expected the failing build named, got {other:?}"),
    }
    all_main("built", &threads.built);
    all_main("prepared", &threads.prepared);
    all_main("dropped", &threads.dropped);
}

/// Records where it is dropped: captured by a `prepare` hook's edit, it
/// drops when the edit does.
struct EditWitness(tutti_fixtures::Record<ThreadId>);

impl Drop for EditWitness {
    fn drop(&mut self) {
        here(&self.0);
    }
}

/// A request for the mix whose `prepare` edit captures an
/// [`EditWitness`] recording into `drops`.
fn witnessed(drops: &tutti_fixtures::Record<ThreadId>) -> Bounce<NodeKey, BufferMainOutput> {
    let witness = EditWitness(drops.clone());
    buffers(0.05).edit(Box::new(move |_prepared: PreparedGraph<'_>| {
        let _ = &witness;
    }))
}

/// **A `prepare` edit that never runs drops on the main thread**, as the
/// fork does: one whose fork's build fails (the job returns before the
/// edit), and one whose job is dropped unrun on another thread (as a
/// cancelled task is dropped by whichever pool thread reaches it). An edit
/// may capture anything the caller read on the main thread; until it runs it
/// is that state.
#[test]
fn a_prepare_edit_that_never_runs_drops_on_the_main_thread() {
    // A build that fails.
    let mut session = session();
    let broken = session.graph().add(Broken);
    session
        .graph()
        .connect(broken, MainOutput)
        .unwrap()
        .unfaded();
    let drops: tutti_fixtures::Record<ThreadId> = tutti_fixtures::Record::default();
    let job = session
        .bounce(witnessed(&drops))
        .expect("the export starts");
    let rendered = block_on(job);
    match session.apply(rendered) {
        Err(BounceError::Fork(tutti_graph::ForkError::Source { .. })) => {}
        other => panic!("expected the failing build, got {other:?}"),
    }
    all_main("dropped (failed build)", &drops);

    // A job dropped before it ran.
    let mut session = self::session();
    let dc = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(dc, MainOutput).unwrap().unfaded();
    let drops: tutti_fixtures::Record<ThreadId> = tutti_fixtures::Record::default();
    let job = session
        .bounce(witnessed(&drops))
        .expect("the export starts");
    std::thread::spawn(move || drop(job))
        .join()
        .expect("the dropping thread");
    assert!(
        drops.lock().unwrap().is_empty(),
        "the edit is held for the main thread, not dropped where the job was"
    );
    let _events = session.update();
    all_main("dropped (job dropped before it ran)", &drops);
}

/// **An export forks a node's setting at the value its key set**: a delay set synced through
/// its key echoes a constant at the quarter note of the session's 120 bpm (24 000 frames at
/// 48 kHz), where the same delay left free echoes at its 0.25 s (12 000), and the live word
/// is the one the key set after the export.
#[test]
fn an_export_forks_a_nodes_setting_at_the_value_its_key_set() {
    use tutti_music::NoteDuration;
    use tutti_nodes::{
        DelayConfig, DelayControls, DelayLayout, DelayNode, DelaySync, MaxDelay, NoteTimes,
    };
    use tutti_units::Feedback;

    let echo = |sync: Option<DelaySync>| {
        let mut session = self::session();
        let g = session.graph();
        let source = g.add(ForkByClone(Const::mono(0.5)));
        let layout = DelayLayout::builder(
            tutti_buffers::ChannelTopology::MONO,
            MaxDelay::of(Seconds::new(2.0)),
        )
        .notes(NoteTimes::all(NoteDuration::QUARTER))
        .build()
        .expect("a layout");
        let delay = g.add(DelayNode::<f32>::from(DelayConfig::new(layout).controls(
            DelayControls {
                time: Seconds::new(0.25),
                feedback: Feedback::NONE,
                ..DelayControls::DEFAULT
            },
        )));
        g.set_sources(InputPort::new(delay, 0), [OutputPort::new(source, 0)])
            .unwrap()
            .unfaded();
        g.set_sources(MainOutput::channel(0), [OutputPort::new(delay, 0)])
            .unwrap()
            .unfaded();
        if let Some(sync) = sync {
            session.set(delay.sync().to(sync)).unwrap();
        }
        let planes = render(&mut session, buffers(0.75));
        if let Some(sync) = sync {
            assert_eq!(session.get(delay.sync()).unwrap(), sync);
        }
        planes[0].iter().position(|&v| v != 0.0).expect("an echo")
    };
    assert_eq!(echo(None), 12_000);
    assert_eq!(echo(Some(DelaySync::Synced)), 24_000);
}
