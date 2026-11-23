//! An editor and its executor, driven by a test that compiles each graph
//! itself: `switch` compiles a graph against the plan running, ships it with
//! fresh units, and `block` renders one block.

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::Float;
use tutti_graph::testing::{self as common, Kind, prepare};
use tutti_graph::{
    Editor, Executor, NodeKey, Placement, Plan, TransportPosition, ValidGraph, compile,
};
use tutti_units::{Beat, Bpm, Latency};

pub struct Rig<F: Float = f32> {
    pub editor: Editor<F>,
    pub exec: Executor<F>,
    plan: Option<Plan>,
    outputs: usize,
    inputs: usize,
    max_block: usize,
}

impl<F: Float> Rig<F> {
    pub fn new(max_block: usize) -> Self {
        let (editor, exec) =
            Editor::with_event_capacity(prepare(max_block), common::EVENT_CAPACITY);
        Self {
            editor,
            exec,
            plan: None,
            outputs: 0,
            inputs: 0,
            max_block,
        }
    }

    /// Switch to `graph`, with a fresh unit for every node the delta places.
    /// A node's declared latency is the spec's, so a kept unit whose latency
    /// changed has its crossfade cut.
    pub fn switch(&mut self, graph: &ValidGraph, kinds: &BTreeMap<NodeKey, Kind>) {
        let mut shapes = common::shapes_of(kinds);
        for (k, s) in shapes.iter_mut() {
            if let Some(n) = graph.topology().nodes.get(k) {
                s.latency = Latency::new(n.latency);
            }
        }
        let (plan, mut delta) =
            compile(graph, &shapes, &prepare(self.max_block), self.plan.as_ref())
                .expect("compiles");
        plan.verify().expect("verifies");
        delta.cuts = plan
            .units()
            .iter()
            .filter(|now| {
                self.plan
                    .as_ref()
                    .and_then(|p| p.unit(now.key))
                    .is_some_and(|was| {
                        was.generation == now.generation
                            && was.idx == now.idx
                            && was.shape.latency != now.shape.latency
                    })
            })
            .map(|u| Placement {
                key: u.key,
                generation: u.generation,
                idx: u.idx,
            })
            .collect();
        let placed: BTreeSet<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, n)| n.key))
            .collect();
        self.outputs = graph.topology().outputs.len();
        self.inputs = graph.topology().inputs.count() as usize;
        self.plan = Some(plan.clone());
        self.editor
            .package(plan, delta, common::units_for::<F>(kinds, placed))
            .expect("the rig collects every box");
        self.exec.apply_pending();
        self.editor.collect();
    }

    /// One block of `frames` through the executor: main input channel `c` is
    /// `input` scaled by `2^-c` (exact, and distinct per channel).
    pub fn block(&mut self, frames: usize, input: &[F]) -> Vec<Vec<F>> {
        let transport = TransportPosition::new(true, Bpm::of(120.0), Beat::new(0.0), None);
        let chans: Vec<Vec<F>> = (0..self.inputs.max(1))
            .map(|c| {
                input[..frames]
                    .iter()
                    .map(|&x| x * F::from_f32(0.5f32.powi(c as i32)))
                    .collect()
            })
            .collect();
        let ins: Vec<&[F]> = chans.iter().map(Vec::as_slice).collect();
        let mut out = vec![vec![F::ZERO; frames]; self.outputs];
        let mut outs: Vec<&mut [F]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        self.exec.process(frames, &transport, &ins, &mut outs);
        out
    }
}
