//! A render that dropped events says so: an error by default (and no file
//! left behind), a count when the caller allows it.
//!
//! The graph is a node declaring a port of 4 events and writing 10 note-ons
//! every block into a node that discards them, beside a tone. A render of
//! 2,048 frames is two 1,024-frame blocks: 6 note-ons refused in each, 12 in
//! all.

use crate::support;
use crate::support::{ExportConfig, RenderConfig};

use support::{render_buffers_under, render_file_under, render_to_file};
use tutti_buffers::ChannelLayout;
use tutti_export::{
    CancelToken, Control, EventLossPolicy, Output, Outputs, Progress, RenderError, RenderGraph,
    Setup, render,
};
use tutti_graph::{Cx, Event, GraphBuilder, Io, Node, OutEvents, Prepare, Shape, Status};
use tutti_nodes::testing::Osc;
use tutti_timeline::FrozenClock;
use tutti_ump::Ump;
use tutti_units::Seconds;
use tutti_units::{Hz, SampleRate};

#[derive(Clone)]
struct Burst;

impl Burst {
    const SHAPE: Shape = Shape::EMPTY
        .events(0, 1)
        .event_capacity(std::num::NonZeroU32::new(4).expect("a capacity"));
    const OUT: OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl Node for Burst {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..10 {
            let at = cx.env.offset(i).expect("inside the block");
            let _ = io
                .event_out(Self::OUT)
                .push(Event::midi(at, Ump([0x2090_3c64, 0, 0, 0])));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

#[derive(Clone)]
struct Discard;

impl Node for Discard {
    fn shape(&self) -> Shape {
        Shape::EMPTY.events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

const RATE: SampleRate = SampleRate::SR_48K;

fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let tone = g.add(Osc::sine(Hz::new(440.0)));
    let burst = g.add(tutti_graph::ForkByClone(Burst));
    let sink = g.add(tutti_graph::ForkByClone(Discard));
    g.connect_events(burst, 0, sink, 0);
    g.pipe_output(tone);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("paired")
}

fn config(policy: EventLossPolicy) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds::new(2_048.0 / 48_000.0),
            on_event_loss: policy,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// By default the render is an error naming how many were dropped, and it
/// stops at the first block that dropped any: the 6 refused in block one, not
/// the 12 of the whole render, so the progress callback never hears of the
/// second block (its reading at 2,048 frames, the plan's total, is delivered
/// whenever the block is rendered).
#[test]
fn a_render_that_dropped_events_stops_at_the_first_block_that_did() {
    let mut rendered_frames = Vec::new();
    let token = CancelToken::new();
    let mut record = |p: &Progress| rendered_frames.push(p.frames().map(|(done, _)| done.get()));
    let err = render_buffers_under(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        &mut Control::new(&token).on_progress(&mut record),
    )
    .expect_err("events were dropped");
    assert!(
        matches!(err, RenderError::EventsDropped { count: 6 }),
        "{err:?}"
    );
    assert_eq!(
        rendered_frames,
        [Some(0)],
        "only the render's start was reported: the second block was never rendered"
    );
}

/// The events one allowed render of `g` into memory lost, as its `Done` reports them.
fn dropped_into_buffers(g: &mut RenderGraph) -> u64 {
    let config = config(EventLossPolicy::Allow);
    let mut outputs = Outputs::new();
    outputs.buffers(Output::main_output(config.buffer_target()));
    let done = render(
        g,
        &config.buffers_job().expect("a job"),
        &FrozenClock,
        outputs,
        &mut Control::none(),
    )
    .expect("allowed")
    .into_finished()
    .expect("nothing cancels it");
    done.dropped_events()
}

/// Allowed, it succeeds and carries the figure; a graph rendered a second
/// time reports that render's drops, not the total.
#[test]
fn an_allowed_loss_is_reported() {
    let mut g = graph();
    assert_eq!(dropped_into_buffers(&mut g), 12);
    assert_eq!(dropped_into_buffers(&mut g), 12);
}

/// A render that lost events puts nothing in place: the file already at the
/// path is byte for byte what it was, and no temporary is left beside it, on
/// the streaming path and the normalizing one. An allowed render replaces it
/// and carries the count. A render the caller cancels is not judged.
#[test]
fn a_render_that_lost_events_replaces_nothing() {
    let dir = std::env::temp_dir().join(format!("tutti-export-loss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.wav");
    let before = b"the previous export".to_vec();
    std::fs::write(&path, &before).unwrap();
    let leftovers = || {
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };

    let err = render_to_file(graph(), &config(EventLossPolicy::Fail), &FrozenClock, &path)
        .expect_err("events were dropped");
    assert!(
        matches!(err, RenderError::EventsDropped { count: 6 }),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()], "no temporary");

    let normalize = tutti_export::Normalize::Peak {
        target: tutti_units::Db::new(-1.0),
    };
    let err = support::render_normalized_to_file(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        normalize,
        &path,
    )
    .expect_err("events were dropped");
    assert!(
        matches!(err, RenderError::EventsDropped { count: 6 }),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()], "no temporary");

    // Cancelled before the first block: not an error, and still untouched.
    let token = CancelToken::new();
    token.cancel();
    let cancelled = render_file_under(
        graph(),
        &config(EventLossPolicy::Fail),
        &FrozenClock,
        None,
        &path,
        &mut Control::new(&token),
    )
    .expect("a cancelled render is not an error");
    assert_eq!(cancelled, tutti_export::Outcome::Cancelled);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let written = render_to_file(
        graph(),
        &config(EventLossPolicy::Allow),
        &FrozenClock,
        &path,
    )
    .expect("allowed");
    assert_eq!(written.dropped_events, 12);
    assert_ne!(std::fs::read(&path).unwrap(), before);
    assert_eq!(leftovers(), vec!["out.wav".to_string()]);
    std::fs::remove_dir_all(&dir).unwrap();
}
