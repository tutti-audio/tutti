//! What a node logs on the audio thread reaches the control side through the
//! engine's `rt_log()` readers, from the block after the filter is stored.

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::Engine;
use tutti_graph::{
    Cx, Editor, Io, LogFilter, Node, NodeKey, OutPort, Prepare, RtLevel, RtLogReaders, RtRecord,
    RtValue, Shape, Source, Status, Unforkable,
};
use tutti_timeline::Transport;
use tutti_units::{Frame, SampleRate, Samples, Tail};

static SITE: &str = "site";

const ALL: LogFilter = LogFilter::new(Some(RtLevel::Trace), Some(RtLevel::Trace));

/// A silent source that logs one record at frame 3 of every block.
struct Logger;

impl Node for Logger {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let at = io.offset(3).expect("inside the block");
        io.log(
            RtLevel::Warn,
            SITE,
            at,
            &[("offset", RtValue::from(3usize))],
        );
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

#[test]
fn an_engine_renders_a_nodes_records_to_its_readers_from_the_block_after_the_filter_is_stored() {
    let transport = Transport::new(SampleRate::SR_48K);
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(64)),
    ));
    ed.insert(NodeKey(1), "logger", Unforkable(Logger));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        })]);
    ed.commit().expect("commits");
    let mut exec = exec;
    let readers = RtLogReaders::new(&mut exec);
    let engine = Engine::new(&transport, &mut ed, exec, tutti_engine::BlockDates::Os)
        .expect("within the limits");
    let block = |engine: &Engine| {
        let mut buf = vec![0.0f32; 64];
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO).unwrap());
    };
    let collect = || {
        let mut out: Vec<RtRecord> = Vec::new();
        let lost = readers.drain(ALL, |r| out.push(*r));
        (out, lost)
    };
    block(&engine);
    assert!(
        collect().0.is_empty(),
        "the gate was off for the first block"
    );
    block(&engine);
    let (after, lost) = collect();
    assert_eq!(lost, 0);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].source(), NodeKey(1));
    assert_eq!(after[0].fields(), &[("offset", RtValue::U64(3))]);
    assert_eq!(after[0].frame(), Frame::new(64 + 3));
}
