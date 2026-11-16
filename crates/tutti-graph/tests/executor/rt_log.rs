//! Logging from the audio thread: a node's `Io::log` reaches the readers the
//! executor hands the control side, with the node and the frame the executor
//! fills in, coalesced and gated as the spec says, on the serial executor and
//! on the parallel one, which renders the same samples with logging on.

use tutti_graph::testing as common;

use common::{TestPool, prepare, test_pool};
use tutti_graph::{
    Cx, Editor, Executor, Io, LogFilter, LogGate, Node, NodeKey, Prepare, RtLevel, RtLogReader,
    RtLogReaders, RtRecord, RtTarget, RtValue, Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Frame, Tail};

/// Sites are statics: coalescing compares the literal's address, which the
/// compiler need not give two equal literals alike.
static A: &str = "a";
static B: &str = "b";
static BUSY: &str = "busy";
static OTHER: &str = "other";
static LOUD: &str = "loud";
static CHATTY: &str = "chatty";

const ALL: LogFilter = LogFilter::new(Some(RtLevel::Trace), Some(RtLevel::Trace));

/// What a [`Logger`] does in each call.
#[derive(Clone, Copy)]
enum Act {
    /// Log `site` at `offset` with the field `v = value`.
    Log {
        level: RtLevel,
        site: &'static str,
        offset: usize,
        value: i64,
    },
    /// Log a plugin's line.
    Line(&'static str, usize),
}

/// A source of a constant that does `acts` in every call.
struct Logger {
    level: f32,
    acts: Vec<Act>,
}

impl Node for Logger {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for act in &self.acts {
            match *act {
                Act::Log {
                    level,
                    site,
                    offset,
                    value,
                } => io.log(
                    level,
                    site,
                    io.offset(offset).expect("inside the block"),
                    &[("v", RtValue::I64(value))],
                ),
                Act::Line(text, offset) => io.log_line(
                    &RtRecord::line(RtLevel::Info, RtTarget::Plugin, text),
                    io.offset(offset).expect("inside the block"),
                ),
            }
        }
        io.output(0).fill(self.level);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn log(site: &'static str, offset: usize, value: i64) -> Act {
    Act::Log {
        level: RtLevel::Warn,
        site,
        offset,
        value,
    }
}

/// `loggers` as sources 1.., each an output of the graph.
fn build(
    loggers: Vec<Vec<Act>>,
    max_block: usize,
    pool: Option<std::sync::Arc<dyn tutti_graph::Pool>>,
) -> (Editor, Executor, Vec<RtLogReader>, LogGate) {
    let (mut ed, mut exec) = Editor::new(prepare(max_block));
    if pool.is_some() {
        exec.set_pool(pool);
    }
    let mut outputs = Vec::new();
    for (i, acts) in loggers.into_iter().enumerate() {
        let key = NodeKey(i as u64 + 1);
        ed.insert(
            key,
            "logger",
            Unforkable(Logger {
                level: 0.25 * (i + 1) as f32,
                acts,
            }),
        );
        outputs.push(Source::Node(OutPort { node: key, port: 0 }));
    }
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let readers = exec.take_log_readers();
    let gate = exec.log_gate();
    (ed, exec, readers, gate)
}

fn render(exec: &mut Executor, frames: usize, outs: usize) -> Vec<Vec<f32>> {
    let mut bufs = vec![vec![0.0f32; frames]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(frames, &TransportPosition::default(), &[], &mut refs);
    bufs
}

fn drain(readers: &mut [RtLogReader]) -> Vec<RtRecord> {
    let mut out = Vec::new();
    for r in readers {
        while let Some(rec) = r.pop() {
            out.push(rec);
        }
    }
    out.sort_by_key(|r| (r.frame(), r.source()));
    out
}

#[test]
fn nothing_is_logged_while_the_gate_is_off() {
    let (_ed, mut exec, mut readers, gate) = build(vec![vec![log(A, 0, 1)]], 64, None);
    assert_eq!(gate.get(), LogFilter::OFF);
    render(&mut exec, 64, 1);
    assert!(drain(&mut readers).is_empty());
    assert_eq!(readers[0].lost(), 0);
}

#[test]
fn a_record_below_the_gates_level_for_its_target_writes_nothing() {
    let acts = vec![
        Act::Log {
            level: RtLevel::Info,
            site: CHATTY,
            offset: 0,
            value: 0,
        },
        log(LOUD, 0, 0),
        Act::Line("plugin line", 0),
    ];
    let (_ed, mut exec, mut readers, gate) = build(vec![acts], 64, None);
    gate.set(LogFilter::new(Some(RtLevel::Warn), None));
    render(&mut exec, 64, 1);
    let records = drain(&mut readers);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].message(), LOUD);
}

#[test]
fn a_record_carries_its_node_and_the_block_start_plus_its_offset() {
    let (_ed, mut exec, mut readers, gate) =
        build(vec![vec![log(A, 5, 1)], vec![log(B, 9, 2)]], 64, None);
    gate.set(ALL);
    render(&mut exec, 64, 2);
    render(&mut exec, 32, 2);
    let got: Vec<_> = drain(&mut readers)
        .iter()
        .map(|r| (r.message(), r.source(), r.frame()))
        .collect();
    assert_eq!(
        got,
        vec![
            (A, NodeKey(1), Frame::new(5)),
            (B, NodeKey(2), Frame::new(9)),
            (A, NodeKey(1), Frame::new(64 + 5)),
            (B, NodeKey(2), Frame::new(64 + 9)),
        ]
    );
}

#[test]
fn the_same_call_several_times_in_one_node_call_is_one_record_with_repeats() {
    let acts = vec![log(A, 0, 1), log(A, 3, 1), log(A, 4, 1), log(A, 5, 2)];
    let (_ed, mut exec, mut readers, gate) = build(vec![acts], 64, None);
    gate.set(ALL);
    render(&mut exec, 64, 1);
    let records = drain(&mut readers);
    let got: Vec<_> = records
        .iter()
        .map(|r| (r.fields()[0].1, r.repeats(), r.frame()))
        .collect();
    assert_eq!(
        got,
        vec![
            (RtValue::I64(1), 2, Frame::new(0)),
            (RtValue::I64(2), 0, Frame::new(5)),
        ]
    );
}

#[test]
fn a_plugin_line_keeps_its_text_and_its_target_and_never_coalesces() {
    let acts = vec![Act::Line("same", 0), Act::Line("same", 0)];
    let (_ed, mut exec, mut readers, gate) = build(vec![acts], 64, None);
    gate.set(ALL);
    render(&mut exec, 64, 1);
    let records = drain(&mut readers);
    assert_eq!(records.len(), 2);
    assert!(
        records.iter().all(|r| r.text() == Some("same")
            && r.target() == RtTarget::Plugin
            && r.repeats() == 0)
    );
}

#[test]
fn a_fifth_distinct_record_in_one_node_call_is_counted_lost() {
    let acts = (0..6).map(|v| log(A, 0, v)).collect();
    let (_ed, mut exec, mut readers, gate) = build(vec![acts], 64, None);
    gate.set(ALL);
    render(&mut exec, 64, 1);
    assert_eq!(drain(&mut readers).len(), 4);
    assert_eq!(readers[0].lost(), 2);
}

#[test]
fn a_full_ring_drops_the_newest_and_counts_each() {
    // Four distinct records a block, no drain: 64 blocks fill the 256 slots.
    let acts = (0..4).map(|v| log(A, 0, v)).collect();
    let (_ed, mut exec, mut readers, gate) = build(vec![acts], 8, None);
    gate.set(ALL);
    for _ in 0..70 {
        render(&mut exec, 8, 1);
    }
    assert_eq!(readers[0].lost(), 6 * 4);
    let records = drain(&mut readers);
    assert_eq!(records.len(), 256);
    assert_eq!(
        records.last().map(RtRecord::frame),
        Some(Frame::new(63 * 8)),
        "the newest were the ones dropped"
    );
}

#[test]
fn a_set_pool_hands_over_a_ring_per_participant_and_keeps_what_the_old_ones_hold() {
    let (_ed, mut exec, mut old, gate) = build(vec![vec![log(A, 0, 1)]], 64, None);
    gate.set(ALL);
    render(&mut exec, 64, 1);
    exec.set_pool(Some(test_pool(3)));
    let fresh = exec.take_log_readers();
    assert_eq!(fresh.len(), 3, "one ring per participant");
    assert!(old[0].is_disconnected());
    assert_eq!(drain(&mut old).len(), 1, "the retired ring still reads");
}

/// Four loggers (heavy by default) behind each other's outputs: enough for
/// the cost model to spread, each logging at its own offsets.
fn busy() -> Vec<Vec<Act>> {
    (0..4)
        .map(|i| {
            vec![
                log(BUSY, i, i as i64),
                log(BUSY, i, i as i64),
                log(OTHER, i + 8, 7),
            ]
        })
        .collect()
}

#[test]
fn a_parallel_render_with_logging_on_is_bit_identical_to_the_serial_one_and_logs_the_same() {
    let blocks = [64usize, 17, 64, 12, 33];
    let run = |pool: Option<std::sync::Arc<dyn tutti_graph::Pool>>| {
        let parallel = pool.is_some();
        let (_ed, mut exec, mut readers, gate) = build(busy(), 64, pool);
        assert_eq!(exec.is_parallel(), parallel);
        gate.set(ALL);
        let mut audio = Vec::new();
        for &n in &blocks {
            audio.extend(render(&mut exec, n, 4));
        }
        let records: Vec<_> = drain(&mut readers)
            .iter()
            .map(|r| (r.frame(), r.source(), r.message(), r.repeats()))
            .collect();
        (audio, records)
    };
    let serial = run(None);
    assert_eq!(serial.1.len(), blocks.len() * 4 * 2);
    for workers in common::WORKERS {
        let parallel = run(Some(test_pool(workers)));
        assert_eq!(parallel.0, serial.0, "{workers} workers: the samples");
        assert_eq!(parallel.1, serial.1, "{workers} workers: the records");
    }
}

#[test]
fn logging_and_flushing_allocate_nothing_on_either_executor() {
    for workers in [1usize, 3] {
        let pool = (workers > 1).then(|| TestPool::no_alloc(workers) as _);
        let (_ed, mut exec, mut readers, gate) = build(busy(), 64, pool);
        gate.set(ALL);
        let mut bufs = vec![vec![0.0f32; 64]; 4];
        for _ in 0..3 {
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
            assert_no_alloc::assert_no_alloc(|| {
                exec.process(64, &TransportPosition::default(), &[], &mut refs);
            });
            // The control side reads between blocks, off the gate.
            assert_eq!(drain(&mut readers).len(), 8, "{workers} workers");
        }
    }
}

/// A registry over a fresh executor, its gate stored by the first drain.
fn registry(loggers: Vec<Vec<Act>>, max_block: usize) -> (Editor, Executor, RtLogReaders) {
    let (ed, mut exec, _readers, _gate) = build(loggers, max_block, None);
    // `build` took the readers: this registry sees the executor as resized.
    exec.set_pool(None);
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    (ed, exec, readers)
}

fn collect(readers: &RtLogReaders) -> (Vec<RtRecord>, u64) {
    let mut out = Vec::new();
    let lost = readers.drain(ALL, |r| out.push(*r));
    (out, lost)
}

#[test]
fn a_drain_hands_the_records_of_every_node_in_frame_order_whatever_order_they_ran_in() {
    // The first node logs late in the block, the second early: the ring holds
    // them in the order they ran.
    let (_ed, mut exec, readers) = registry(vec![vec![log(A, 50, 0)], vec![log(B, 5, 0)]], 64);
    render(&mut exec, 64, 2);
    let frames: Vec<_> = collect(&readers).0.iter().map(RtRecord::frame).collect();
    assert_eq!(frames, [Frame::new(5), Frame::new(50)]);
}

#[test]
fn a_drain_across_a_pools_rings_is_ordered_by_frame_and_complete() {
    let (_ed, mut exec, _taken, _gate) = build(busy(), 64, None);
    // The sizing the registry adopts: one ring per participant.
    exec.set_pool(Some(test_pool(3)));
    let readers = RtLogReaders::new(&mut exec);
    readers.drain(ALL, |_| {});
    for _ in 0..8 {
        render(&mut exec, 64, 4);
    }
    let (records, lost) = collect(&readers);
    assert_eq!(lost, 0);
    assert_eq!(records.len(), 8 * 4 * 2);
    assert!(records.windows(2).all(|w| w[0].frame() <= w[1].frame()));
}

#[test]
fn records_past_a_rings_room_are_counted_once_in_the_drain_that_sees_them() {
    let acts = (0..4).map(|v| log(A, 0, v)).collect();
    let (_ed, mut exec, readers) = registry(vec![acts], 8);
    for _ in 0..70 {
        render(&mut exec, 8, 1);
    }
    let (records, lost) = collect(&readers);
    assert_eq!((records.len(), lost), (256, 4 * 70 - 256));
    assert_eq!(collect(&readers).1, 0, "a loss is reported once");
}

#[test]
fn resizing_the_executor_delivers_what_the_old_rings_held_and_reads_the_new_ones() {
    let (_ed, mut exec, readers) = registry(vec![vec![log(A, 1, 0)]], 64);
    render(&mut exec, 64, 1);
    exec.set_pool(Some(test_pool(3)));
    readers.adopt(&mut exec);
    render(&mut exec, 64, 1);
    let (records, lost) = collect(&readers);
    assert_eq!(lost, 0);
    let frames: Vec<_> = records.iter().map(RtRecord::frame).collect();
    assert_eq!(
        frames,
        [Frame::new(1), Frame::new(64 + 1)],
        "the block before the resize, and the one after"
    );
}
