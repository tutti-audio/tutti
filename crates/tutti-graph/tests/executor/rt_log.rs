//! Logging from the audio thread: a node's `Io::log` reaches the control
//! side's `RtLogReaders`, with the node and the frame the executor fills in,
//! coalesced, gated and merged by frame, on the serial executor and on the
//! parallel one, which renders the same samples with logging on.

use tutti_graph::testing as common;

use common::{TestPool, prepare, test_pool};
use tutti_buffers::ChannelLayout;
use tutti_graph::contract::Direct;
use tutti_graph::{
    Cx, Editor, Executor, Io, LogFilter, Node, NodeKey, Prepare, RtLevel, RtLogReaders, RtRecord,
    RtTarget, RtValue, Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{OutputPort, PortSource};
use tutti_units::{Frame, SampleRate, Tail};

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

/// `loggers` as sources 1.., each an output of the graph, and the registry of
/// the executor's rings. The gate is off until a drain stores a filter.
fn build(
    loggers: Vec<Vec<Act>>,
    max_block: usize,
    pool: Option<std::sync::Arc<dyn tutti_graph::Pool>>,
) -> (Editor, Executor, RtLogReaders) {
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
        outputs.push(PortSource::Node(OutputPort::new(key, 0)));
    }
    ed.spec_mut().topology.set_outputs(outputs);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let readers = RtLogReaders::new(&mut exec);
    (ed, exec, readers)
}

fn render(exec: &mut Executor, frames: usize, outs: usize) -> Vec<Vec<f32>> {
    let mut bufs = vec![vec![0.0f32; frames]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(frames, &TransportPosition::default(), &[], &mut refs);
    bufs
}

/// Drains under `filter` (which the gate takes from the next block), the
/// records ordered by frame and then node, and the losses since the last
/// drain.
fn take(readers: &RtLogReaders, filter: LogFilter) -> (Vec<RtRecord>, u64) {
    let mut out = Vec::new();
    let lost = readers.drain(filter, |r| out.push(*r));
    out.sort_by_key(|r| (r.frame(), r.source()));
    (out, lost)
}

/// A registry whose gate is open from the next block.
fn open(readers: &RtLogReaders) {
    take(readers, ALL);
}

#[test]
fn nothing_is_logged_before_a_drain_opens_the_gate() {
    let (_ed, mut exec, readers) = build(vec![vec![log(A, 0, 1)]], 64, None);
    render(&mut exec, 64, 1);
    assert_eq!(take(&readers, ALL), (vec![], 0));
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
    let (_ed, mut exec, readers) = build(vec![acts], 64, None);
    take(&readers, LogFilter::new(Some(RtLevel::Warn), None));
    render(&mut exec, 64, 1);
    let (records, lost) = take(&readers, ALL);
    assert_eq!(
        (records.len(), lost),
        (1, 0),
        "only the loud one: {records:?}"
    );
    assert_eq!(records[0].message(), LOUD);
}

#[test]
fn a_record_carries_its_node_and_the_block_start_plus_its_offset() {
    let (_ed, mut exec, readers) = build(vec![vec![log(A, 5, 1)], vec![log(B, 9, 2)]], 64, None);
    open(&readers);
    render(&mut exec, 64, 2);
    render(&mut exec, 32, 2);
    let got: Vec<_> = take(&readers, ALL)
        .0
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
    let (_ed, mut exec, readers) = build(vec![acts], 64, None);
    open(&readers);
    render(&mut exec, 64, 1);
    let got: Vec<_> = take(&readers, ALL)
        .0
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
fn a_plugin_line_keeps_its_text_and_its_target_and_never_coalesces_not_even_an_empty_one() {
    let acts = vec![
        Act::Line("same", 0),
        Act::Line("same", 0),
        Act::Line("", 0),
        Act::Line("", 0),
    ];
    let (_ed, mut exec, readers) = build(vec![acts], 64, None);
    open(&readers);
    render(&mut exec, 64, 1);
    let records = take(&readers, ALL).0;
    let texts: Vec<_> = records.iter().map(RtRecord::text).collect();
    assert_eq!(texts, [Some("same"), Some("same"), Some(""), Some("")]);
    assert!(
        records
            .iter()
            .all(|r| r.target() == RtTarget::Plugin && r.repeats() == 0)
    );
}

#[test]
fn a_fifth_distinct_record_in_one_node_call_is_counted_lost() {
    let acts = (0..6).map(|v| log(A, 0, v)).collect();
    let (_ed, mut exec, readers) = build(vec![acts], 64, None);
    open(&readers);
    render(&mut exec, 64, 1);
    let (records, lost) = take(&readers, ALL);
    assert_eq!((records.len(), lost), (4, 2));
}

#[test]
fn a_full_ring_drops_the_newest_and_counts_each_once() {
    // Four distinct records a block, no drain: 64 blocks fill the 256 slots.
    let acts = (0..4).map(|v| log(A, 0, v)).collect();
    let (_ed, mut exec, readers) = build(vec![acts], 8, None);
    open(&readers);
    for _ in 0..70 {
        render(&mut exec, 8, 1);
    }
    let (records, lost) = take(&readers, ALL);
    assert_eq!((records.len(), lost), (256, 6 * 4));
    assert_eq!(
        records.last().map(RtRecord::frame),
        Some(Frame::new(63 * 8)),
        "the newest were the ones dropped"
    );
    assert_eq!(take(&readers, ALL).1, 0, "a loss is reported once");
}

#[test]
fn a_drain_hands_the_records_of_every_node_in_frame_order_whatever_order_they_ran_in() {
    // The first node logs late in the block, the second early: the ring holds
    // them in the order they ran.
    let (_ed, mut exec, readers) = build(vec![vec![log(A, 50, 0)], vec![log(B, 5, 0)]], 64, None);
    open(&readers);
    render(&mut exec, 64, 2);
    let frames: Vec<_> = take(&readers, ALL).0.iter().map(RtRecord::frame).collect();
    assert_eq!(frames, [Frame::new(5), Frame::new(50)]);
}

#[test]
fn a_drain_across_a_pools_rings_is_ordered_by_frame_and_complete() {
    let (_ed, mut exec, readers) = build(busy(), 64, Some(test_pool(3)));
    open(&readers);
    for _ in 0..8 {
        render(&mut exec, 64, 4);
    }
    let (records, lost) = take(&readers, ALL);
    assert_eq!(lost, 0);
    assert_eq!(records.len(), 8 * 4 * 3);
    assert!(records.windows(2).all(|w| w[0].frame() <= w[1].frame()));
}

#[test]
fn resizing_the_executor_delivers_what_the_old_rings_held_and_reads_the_new_ones() {
    let (_ed, mut exec, readers) = build(vec![vec![log(A, 1, 0)]], 64, None);
    open(&readers);
    render(&mut exec, 64, 1);
    exec.set_pool(Some(test_pool(3)));
    readers.adopt(&mut exec);
    render(&mut exec, 64, 1);
    let (records, lost) = take(&readers, ALL);
    assert_eq!(lost, 0);
    let frames: Vec<_> = records.iter().map(RtRecord::frame).collect();
    assert_eq!(
        frames,
        [Frame::new(1), Frame::new(64 + 1)],
        "the block before the resize, and the one after"
    );
}

/// Four loggers (heavy by default), each logging a coalescing pair, one
/// other record and a plugin line at its own offsets.
fn busy() -> Vec<Vec<Act>> {
    (0..4)
        .map(|i| {
            vec![
                log(BUSY, i, i as i64),
                log(BUSY, i, i as i64),
                Act::Line("a plugin line", i + 4),
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
        let (_ed, mut exec, readers) = build(busy(), 64, pool);
        assert_eq!(exec.is_parallel(), parallel);
        open(&readers);
        let mut audio = Vec::new();
        for &n in &blocks {
            audio.extend(render(&mut exec, n, 4));
        }
        let records: Vec<_> = take(&readers, ALL)
            .0
            .iter()
            .map(|r| {
                (
                    r.frame(),
                    r.source(),
                    r.message(),
                    r.text().map(str::to_owned),
                    r.repeats(),
                )
            })
            .collect();
        (audio, records)
    };
    let serial = run(None);
    assert_eq!(serial.1.len(), blocks.len() * 4 * 3);
    for workers in common::WORKERS {
        let parallel = run(Some(test_pool(workers)));
        assert_eq!(parallel.0, serial.0, "{workers} workers: the samples");
        assert_eq!(parallel.1, serial.1, "{workers} workers: the records");
    }
}

#[test]
fn logging_lines_and_flushing_allocate_nothing_on_either_executor() {
    for workers in [1usize, 3] {
        let pool = (workers > 1).then(|| TestPool::new(workers) as _);
        let (_ed, mut exec, readers) = build(busy(), 64, pool);
        open(&readers);
        let mut bufs = vec![vec![0.0f32; 64]; 4];
        for _ in 0..3 {
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
            exec.process(64, &TransportPosition::default(), &[], &mut refs);
            // The control side reads between blocks, off the gate.
            assert_eq!(take(&readers, ALL).0.len(), 12, "{workers} workers");
        }
    }
}

/// A `Direct` driver with `log` set keeps the node's record, with the block's
/// first frame plus the call's offset; a filter below the record's level for
/// its target keeps nothing.
#[test]
fn a_direct_driver_keeps_the_record_its_filter_lets_in_and_drops_one_below_it() {
    let acts = || {
        vec![Act::Log {
            level: RtLevel::Warn,
            site: A,
            offset: 3,
            value: 7,
        }]
    };
    let logger = || Logger {
        level: 0.0,
        acts: acts(),
    };

    let mut kept = Direct::new(logger(), SampleRate::SR_48K, 8).log(ALL);
    kept.block();
    kept.block();
    let [first, second] = kept.logged() else {
        panic!("one record a block, got {:?}", kept.logged());
    };
    for (record, frame) in [(first, 3), (second, 11)] {
        assert_eq!(record.level(), RtLevel::Warn);
        assert_eq!(record.target(), RtTarget::Rt);
        assert_eq!(record.message(), A);
        assert_eq!(record.fields(), &[("v", RtValue::I64(7))]);
        assert_eq!(record.frame(), Frame::new(frame));
    }
    assert_eq!(kept.log_lost(), 0);

    let below = LogFilter::new(Some(RtLevel::Error), Some(RtLevel::Trace));
    let mut dropped = Direct::new(logger(), SampleRate::SR_48K, 8).log(below);
    dropped.block();
    assert!(dropped.logged().is_empty());

    let mut unset = Direct::new(logger(), SampleRate::SR_48K, 8);
    unset.block();
    assert!(unset.logged().is_empty());
}
