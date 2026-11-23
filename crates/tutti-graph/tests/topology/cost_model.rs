//! The cost model and coarsening: cheap siblings pack into one task, a
//! pool's participants run every block of a plan with more than one task,
//! and packed plans render bit for bit what one participant renders.
//!
//! The random-topology check is in `tests/differential.rs`
//! (`coarsened_plans_are_bit_identical`).

use tutti_graph::testing as common;

use common::{Kind, TestNode, WORKERS, bits, test_pool};
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_graph::{Cost, Editor, Executor, TransportPosition, Unforkable};
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_units::{Beat, Bpm};

const MAX_BLOCK: usize = 256;

fn gain(cost: Option<Cost>) -> Unforkable<TestNode> {
    let node = TestNode::new(Kind::Gain {
        gain: 0.75,
        width: 1,
    });
    Unforkable(match cost {
        Some(c) => node.cost(c),
        None => node,
    })
}

fn sum(inputs: usize, cost: Option<Cost>) -> Unforkable<TestNode> {
    let node = TestNode::new(Kind::Sum { inputs });
    Unforkable(match cost {
        Some(c) => node.cost(c),
        None => node,
    })
}

fn wire(ed: &mut Editor, node: u64, port: u16, from: PortSource) {
    ed.spec_mut().topology.edges.insert(
        InputPort::new(NodeKey(node), port),
        vec![Edge::Direct(from)],
    );
}

fn node(key: u64) -> PortSource {
    PortSource::Node(OutputPort::new(NodeKey(key), 0))
}

/// A fresh editor and executor, the executor on a pool of `workers` when
/// more than one.
fn pair(workers: usize) -> (Editor, Executor) {
    let (ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    (ed, exec)
}

fn commit(ed: &mut Editor, exec: &mut Executor) {
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
}

/// For each of `fans` main inputs, `lanes` gains off it, summed onto its
/// own output: `tiny/16` of `tutti-nodes`' bench when `fans` is 1.
fn fans(ed: &mut Editor, fans: usize, lanes: usize, gains: Option<Cost>, sums: Option<Cost>) {
    ed.spec_mut().topology.inputs = ChannelLayout::try_from(fans).expect("a test width is small");
    let mut outputs = Vec::new();
    for f in 0..fans {
        let s = 1_000 * (f as u64 + 1);
        ed.insert(NodeKey(s), "sum", sum(lanes, sums));
        for l in 0..lanes {
            let k = s + 1 + l as u64;
            ed.insert(NodeKey(k), "gain", gain(gains));
            wire(ed, k, 0, PortSource::MainInput(f as u16));
            wire(ed, s, l as u16, node(k));
        }
        outputs.push(node(s));
    }
    ed.spec_mut().topology.set_outputs(outputs);
}

/// `lanes` chains of `depth` gains declaring `cost`, each off the main
/// input, summed (lightly) onto one output.
fn wide(ed: &mut Editor, lanes: usize, depth: usize, cost: Option<Cost>) {
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let s = 1_000_000;
    ed.insert(NodeKey(s), "sum", sum(lanes, Some(Cost::Light)));
    for l in 0..lanes {
        let mut last = PortSource::MainInput(0);
        for d in 0..depth {
            let k = 1_000 * (l as u64 + 1) + d as u64;
            ed.insert(NodeKey(k), "gain", gain(cost));
            wire(ed, k, 0, last);
            last = node(k);
        }
        wire(ed, s, l as u16, last);
    }
    ed.spec_mut().topology.set_outputs(vec![node(s)]);
}

/// Render `blocks` (input on every channel), every output's samples.
fn render(exec: &mut Executor, inputs: usize, outputs: usize, blocks: &[usize]) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); outputs];
    let mut frame = 0u64;
    for &n in blocks {
        let chans: Vec<Vec<f32>> = (0..inputs)
            .map(|c| {
                (0..n)
                    .map(|k| ((((frame + k as u64) * 7919 + c as u64) % 211) as f32) / 211.0 - 0.5)
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        let mut chunk = vec![vec![0.0f32; n]; outputs];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
            exec.process(n, &t, &ins, &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c);
        }
        frame += n as u64;
    }
    all
}

/// Block lengths from one frame to the maximum, ragged.
fn ragged() -> Vec<usize> {
    let mut v: Vec<usize> = (0..40).map(|i| 1 + (i * 37) % MAX_BLOCK).collect();
    v.extend([MAX_BLOCK; 4]);
    v
}

/// `tiny/16`: sixteen light gains pack into two tasks of eight (about two
/// grains of work). The same graph with gains that declare nothing keeps a
/// task per gain. Both spread over the pool.
#[test]
fn light_gains_pack_and_undeclared_ones_keep_a_task_each() {
    let (mut ed, mut exec) = pair(4);
    fans(&mut ed, 1, 16, Some(Cost::Light), Some(Cost::Light));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    // The input, two packs of eight gains, the sum and its output.
    assert_eq!(plan.tasks().len(), 4, "{:?}", plan.tasks());
    assert!(exec.is_parallel());

    let (mut ed, mut exec) = pair(4);
    fans(&mut ed, 1, 16, None, Some(Cost::Light));
    commit(&mut ed, &mut exec);
    let plan = exec.plan().expect("a plan");
    // The input, each gain, the sum and its output.
    assert_eq!(plan.tasks().len(), 18);
    assert!(exec.is_parallel());
}

/// Light gains beside heavy ones off one input, all summed: the light ones
/// pack (into fewer tasks than gains), each heavy one keeps a task of its
/// own, and at every participant count and ragged block lengths the
/// render is bit for bit the serial one.
#[test]
fn packed_siblings_render_bit_identically() {
    let build = |workers: usize| {
        let (mut ed, mut exec) = pair(workers);
        ed.spec_mut().topology.inputs = ChannelLayout::MONO;
        let s = 1_000_000;
        ed.insert(NodeKey(s), "sum", sum(15, None));
        for l in 0..15u64 {
            // Every fifth gain declares nothing (heavy).
            let cost = (l % 5 != 4).then_some(Cost::Light);
            ed.insert(NodeKey(l + 1), "gain", gain(cost));
            wire(&mut ed, l + 1, 0, PortSource::MainInput(0));
            wire(&mut ed, s, l as u16, node(l + 1));
        }
        ed.spec_mut().topology.set_outputs(vec![node(s)]);
        commit(&mut ed, &mut exec);
        exec
    };
    let serial = {
        let mut exec = build(1);
        let plan = exec.plan().expect("a plan").clone();
        plan.verify().expect("sound");
        // The input, the three heavy gains, the packed light ones (twelve
        // gains in fewer tasks), the sum and its output.
        let heavy = |op: u32| {
            matches!(plan.ops()[op as usize], tutti_graph::Op::Node { unit, .. }
                if plan.units()[unit as usize].shape.cost == Cost::Heavy
                    && plan.units()[unit as usize].shape.audio_in().count() == 1)
        };
        let mut light_tasks = 0;
        for t in plan.tasks() {
            let ops = &plan.task_ops()[t.start as usize..(t.start + t.len) as usize];
            if ops.iter().any(|&o| heavy(o)) {
                assert_eq!(ops.len(), 1, "a heavy gain shares a task");
            } else if ops.len() > 1 {
                light_tasks += 1;
            }
        }
        assert!(light_tasks >= 1, "some light gains packed");
        assert!(plan.tasks().len() < 1 + 15 + 1, "{:?}", plan.tasks());
        render(&mut exec, 1, 1, &ragged())
    };
    assert!(serial[0].iter().any(|&x| x != 0.0));
    for workers in WORKERS {
        let mut exec = build(workers);
        assert!(exec.is_parallel());
        let parallel = render(&mut exec, 1, 1, &ragged());
        assert_eq!(bits(&serial), bits(&parallel), "{workers} workers");
    }
}

/// A pool that counts the blocks it is asked to run, and records the last
/// participant count it was asked to wake.
struct Counting {
    inner: std::sync::Arc<dyn tutti_graph::Pool>,
    runs: std::sync::atomic::AtomicUsize,
    woke: std::sync::atomic::AtomicUsize,
}

impl tutti_graph::Pool for Counting {
    fn participants(&self) -> usize {
        self.inner.participants()
    }
    fn run(&self, wake: usize, job: &(dyn Fn(usize) + Sync)) {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.woke.store(wake, std::sync::atomic::Ordering::Relaxed);
        self.inner.run(wake, job);
    }
    fn waits(&self) -> tutti_graph::BlockWaits {
        self.inner.waits()
    }
}

fn counting(participants: usize) -> std::sync::Arc<Counting> {
    std::sync::Arc::new(Counting {
        inner: test_pool(participants),
        runs: std::sync::atomic::AtomicUsize::new(0),
        woke: std::sync::atomic::AtomicUsize::new(0),
    })
}

/// A plan of several tasks spreads every block over every participant of
/// the pool, a one-frame block included: the pool runs each block once and
/// is asked to wake all four.
#[test]
fn every_block_of_a_plan_of_several_tasks_wakes_every_participant() {
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    let pool = counting(4);
    exec.set_pool(Some(pool.clone()));
    wide(&mut ed, 4, 4, Some(Cost::Light));
    commit(&mut ed, &mut exec);
    assert!(exec.plan().expect("a plan").tasks().len() > 1);
    assert!(exec.is_parallel());
    let blocks = [1, 2, 63, MAX_BLOCK];
    render(&mut exec, 1, 1, &blocks);
    let ordering = std::sync::atomic::Ordering::Relaxed;
    assert_eq!(pool.runs.load(ordering), blocks.len());
    assert_eq!(pool.woke.load(ordering), 4);
}

/// A plan of one task has nothing to spread: its blocks run on the calling
/// thread and never reach the pool.
#[test]
fn a_plan_of_one_task_never_reaches_the_pool() {
    let (mut ed, mut exec) = Editor::new(common::prepare(MAX_BLOCK));
    let pool = counting(4);
    exec.set_pool(Some(pool.clone()));
    wide(&mut ed, 1, 1, None);
    commit(&mut ed, &mut exec);
    assert_eq!(exec.plan().expect("a plan").tasks().len(), 1);
    assert!(!exec.is_parallel());
    render(&mut exec, 1, 1, &[1, MAX_BLOCK]);
    assert_eq!(pool.runs.load(std::sync::atomic::Ordering::Relaxed), 0);
}
