//! The VBAP panner declares `Cost::Moderate`; the HRTF renderer stays `Heavy`. Declaring a
//! class changes only the parallel schedule: sibling panners pack into fewer tasks than
//! undeclared ones, and the render on a pool is bit for bit the serial one.

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_fixtures::signal::plane_bits as bits;

use tutti_graph::NodeKey;
use tutti_graph::ParamNode;
use tutti_graph::WorkerPool;
use tutti_graph::{
    Cost, Cx, Editor, Executor, Io, Node, Prepare, Schedule, Shape, Status, TransportPosition,
    Unforkable,
};
use tutti_graph::{Edge, InPort, OutPort, PortSource};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::aim;
use tutti_units::{Azimuth, Beat, Bpm, Elevation, SampleRate, Samples};

const SR: f64 = 48_000.0;
/// Small enough that a stereo `Moderate` panner is under the packing grain.
const MAX_BLOCK: usize = 64;
/// Panners as shipped (`Moderate`).
const PANNERS: u64 = 12;
/// Panners declaring nothing (`Heavy`), so the plan still spreads.
const HEAVY: u64 = 2;

/// `node` as a node that declares nothing: the cost model's default.
struct Undeclared<N>(N);

impl<N: Node> Node for Undeclared<N> {
    fn shape(&self) -> Shape {
        self.0.shape().with_cost(Cost::Heavy)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.0.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// A stereo panner aimed by `k`.
fn panner(k: u64) -> VbapPannerNode<f32> {
    let node = vbap::<f32>(tutti_buffers::Bed::Stereo);
    aim(
        &node.param_set(),
        Azimuth::new(-30.0 + 4.0 * k as f64),
        Elevation::new(0.0),
    );
    node
}

fn wire(ed: &mut Editor, to: NodeKey, port: u16, from: PortSource) {
    ed.spec_mut()
        .topology
        .edges
        .insert(InPort { node: to, port }, vec![Edge::Direct(from)]);
}

/// [`PANNERS`] declared panners and [`HEAVY`] undeclared ones, each off the
/// stereo global input, summed by the graph (a fan-in onto each of two global outputs); on
/// a pool of `workers` when more than one. `declared`: the panners as
/// shipped, or every one wrapped to declare nothing.
fn graph(workers: usize, declared: bool) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(
            WorkerPool::builder()
                .num_threads(workers)
                .build()
                .expect("a worker pool"),
        )));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::STEREO;
    let sources = PANNERS + HEAVY;
    // Two silent global outputs, which the fan-in below fills.
    ed.spec_mut().topology.set_outputs([PortSource::Zero; 2]);
    for k in 0..sources {
        let key = NodeKey(k + 1);
        if declared && k < PANNERS {
            ed.insert(key, "vbap", panner(k));
        } else {
            ed.insert(key, "vbap", Unforkable(Undeclared(panner(k))));
        }
        for c in 0..2u16 {
            wire(&mut ed, key, c, PortSource::Global(c));
            // The graph's fan-in: output channel `c` holds every panner's
            // port `c`, and the graph sums them.
            ed.spec_mut()
                .topology
                .connect_output(c as usize, PortSource::Node(OutPort { node: key, port: c }));
        }
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// Blocks from one frame to the maximum, ragged, a distinct input on each
/// channel.
fn render(exec: &mut Executor) -> Vec<Vec<f32>> {
    let mut all = vec![Vec::new(); 2];
    let mut frame = 0usize;
    for i in 0..60 {
        let n = 1 + (i * 37) % MAX_BLOCK;
        let ins: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..n)
                    .map(|k| (((frame + k) * 7_919 + c) % 211) as f32 / 211.0 - 0.5)
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut chunk = vec![vec![0.0f32; n]; 2];
        {
            let mut outs: Vec<&mut [f32]> = chunk.iter_mut().map(Vec::as_mut_slice).collect();
            let t = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
            exec.process(n, &t, &ins, &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chunk) {
            a.extend(c);
        }
        frame += n;
    }
    all
}

/// A panner of any width is `Moderate`.
#[test]
fn a_panner_is_moderate() {
    for node in [
        vbap::<f32>(tutti_buffers::Bed::Stereo),
        vbap::<f32>(tutti_buffers::Bed::Quad),
        vbap::<f32>(tutti_buffers::Bed::Surround51),
    ] {
        assert_eq!(node.shape().cost, Cost::Moderate);
    }
}

/// Twelve `Moderate` panners beside two undeclared ones: the moderate ones
/// pack into fewer tasks than the same panners declaring nothing, the plan
/// still spreads, and on a pool of two and of four the render is bit for
/// bit the serial one.
#[test]
#[cfg_attr(miri, ignore = "spawns a worker pool")]
fn moderate_panners_pack_and_render_bit_identically() {
    let declared = graph(4, true);
    let undeclared = graph(4, false);
    let (plan, bare) = (
        declared.plan().expect("a plan"),
        undeclared.plan().expect("a plan"),
    );
    assert!(
        plan.tasks().len() < bare.tasks().len(),
        "declared {} tasks, undeclared {}",
        plan.tasks().len(),
        bare.tasks().len()
    );
    // From a one-frame block: every ragged block below is spread, so the
    // comparison is never a serial walk against itself.
    assert!(
        matches!(plan.schedule(4), Schedule::Parallel { from, .. } if from == Samples::new(1)),
        "{:?}",
        plan.schedule(4)
    );

    let serial = render(&mut graph(1, true));
    assert!(serial.iter().all(|c| c.iter().any(|&x| x != 0.0)));
    for workers in [2, 4] {
        let mut exec = graph(workers, true);
        assert!(exec.is_parallel());
        assert_eq!(bits(&serial), bits(&render(&mut exec)), "{workers} workers");
    }
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    tutti_nodes::VbapPannerNode::try_from(tutti_nodes::VbapPannerConfig::new(bed))
        .expect("a bed with speakers to pan between")
}
