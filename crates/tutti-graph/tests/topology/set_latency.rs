//! `Editor::set_latency`: a runtime latency change — a plugin whose latency
//! atomic moved — recompiles and moves PDC, without touching the unit.

use tutti_graph::testing::WireDeclared;

use tutti_graph::EdgeId;
use tutti_graph::testing as common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Kind, TestNode, prepare};
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::MAX_NODE_LATENCY;
use tutti_graph::NodeKey;
use tutti_graph::{
    CommitError, Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, TransportPosition,
    Unforkable,
};
use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
use tutti_units::{Latency, SampleRate, Samples};

/// The delay the plugin actually runs.
const TRUE_LATENCY: usize = 16;

/// A plugin that delays its input by [`TRUE_LATENCY`] frames all along, but
/// *reports* whatever its latency cell held when it was last prepared — 0 at
/// load, as a plugin that only learns its latency after activation does. Its
/// `shape` answers from that cache, so a running unit's own figure lags a
/// change the host hands the editor. Counts its calls, so a replacement
/// would show.
#[derive(Clone)]
struct Plugin {
    reported: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    probed: usize,
    ring: [f32; TRUE_LATENCY],
    pos: usize,
}

impl Plugin {
    fn new() -> Self {
        Self {
            reported: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            probed: 0,
            ring: [0.0; TRUE_LATENCY],
            pos: 0,
        }
    }
}

impl Node for Plugin {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .latency(Latency::new(Samples::new(self.probed)))
    }
    fn prepare(&mut self, _: &Prepare) {
        self.probed = self.reported.load(Ordering::Relaxed);
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = self.ring[self.pos];
            self.ring[self.pos] = x;
            self.pos = (self.pos + 1) % TRUE_LATENCY;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.ring = [0.0; TRUE_LATENCY];
        self.pos = 0;
    }
}

const PLUGIN: NodeKey = NodeKey(1);
const DRY: NodeKey = NodeKey(2);

/// Main input → `PLUGIN` → output 0, and main input → a unity gain
/// (`DRY`, latency 0) → output 1. The two outputs are aligned by PDC.
fn graph(plugin: Plugin) -> (Editor, Executor) {
    graph_at(plugin, prepare(64))
}

fn graph_at(plugin: Plugin, prepared: Prepare) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(prepared);
    ed.resize_inputs(ChannelLayout::MONO);
    ed.insert(PLUGIN, "plugin", Unforkable(plugin));
    ed.insert(
        DRY,
        "gain",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 1.0,
            width: 1,
        })),
    );
    for k in [PLUGIN, DRY] {
        ed.connect_edge(EdgeId::into_port(
            InputPort::new(k, 0),
            Edge::Direct(PortSource::MainInput(0)),
        ))
        .unwrap();
    }
    ed.wire_declared_outputs(vec![
        PortSource::Node(OutputPort::new(PLUGIN, 0)),
        PortSource::Node(OutputPort::new(DRY, 0)),
    ])
    .unwrap();
    (ed, exec)
}

/// One block of `input` through `exec`; both its outputs.
fn block(exec: &mut Executor, input: &[f32]) -> [Vec<f32>; 2] {
    common::block_in(
        exec,
        input.len(),
        &[input],
        2,
        &TransportPosition::default(),
    )
    .try_into()
    .expect("two outputs")
}

/// A one at `at`, zeros elsewhere.
fn impulse(n: usize, at: usize) -> Vec<f32> {
    tutti_fixtures::signal::impulse_at(Samples::new(n), Samples::new(at))
}

fn peak_at(v: &[f32]) -> Option<usize> {
    v.iter().position(|&x| x == 1.0)
}

/// **A latency change moves the other path's compensation on the next
/// commit, and nothing else.** The plugin reports 0 at load though it runs
/// 16 frames late, so its output trails the dry path by 16. `set_latency`
/// and a commit later, the dry path is delayed by 16 and the two line up;
/// the plugin's own output — whose compensation stays 0, since it is the
/// latest path — is bit-identical to a run that never changed anything (no
/// click), and the unit is the same one (never retired, called every block).
#[test]
fn a_latency_change_moves_the_other_paths_compensation() {
    let plugin = Plugin::new();
    let (reported, calls) = (Arc::clone(&plugin.reported), Arc::clone(&plugin.calls));
    let (mut ed, mut exec) = graph(plugin.clone());
    ed.commit().expect("commits");

    // Misaligned while the plugin under-reports.
    let mut fed = impulse(64, 8);
    let [wet, dry] = block(&mut exec, &fed);
    assert_eq!(peak_at(&wet), Some(8 + TRUE_LATENCY));
    assert_eq!(peak_at(&dry), Some(8), "no compensation yet");

    // The plugin's latency atomic moves; the host tells the editor.
    reported.store(TRUE_LATENCY, Ordering::Relaxed);
    ed.set_latency(PLUGIN, Latency::new(Samples::new(TRUE_LATENCY)))
        .expect("the node exists");
    assert_eq!(
        ed.spec().topology.nodes[&PLUGIN].latency,
        Samples::new(TRUE_LATENCY)
    );
    ed.commit().expect("recompiles");

    let mut wet_all = wet;
    for b in 0..4 {
        let input = impulse(64, 20);
        let [wet, dry] = block(&mut exec, &input);
        fed.extend(input);
        if b > 0 {
            assert_eq!(
                peak_at(&dry),
                peak_at(&wet),
                "block {b}: the dry path now lines up with the plugin"
            );
            assert_eq!(peak_at(&dry), Some(20 + TRUE_LATENCY));
        }
        wet_all.extend(wet);
    }
    // An impulse at the end of a block carries into the next: the carry
    // crosses the block boundary at the recompiled graph too.
    let input = impulse(64, 60);
    let [wet, _] = block(&mut exec, &input);
    fed.extend(input);
    wet_all.extend(wet);
    let input = vec![0.0f32; 64];
    let [wet, _] = block(&mut exec, &input);
    fed.extend(input);
    wet_all.extend(wet);
    // The plugin's path is untouched by the recompile (no click): its wet
    // output is the whole input delayed by `TRUE_LATENCY` at gain 1, the
    // carry across blocks included.
    let mut want = vec![0.0f32; TRUE_LATENCY];
    want.extend_from_slice(&fed[..fed.len() - TRUE_LATENCY]);
    assert_eq!(wet_all, want);
    assert!(ed.collect().is_empty(), "no unit was retired");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        7,
        "the same unit, every block"
    );
    let plan = exec.plan().expect("applied");
    assert!(
        plan.audio_delays()
            .iter()
            .map(|d| d.len)
            .chain(plan.event_delays().iter().map(|d| d.len))
            .any(|len| len == Samples::new(TRUE_LATENCY)),
        "a PDC delay of the new latency"
    );
}

/// Refusals: a key with no node, a latency past what PDC compensates, and
/// between a re-prepare's two commits. What PDC compensates is ten seconds at
/// the editor's rate: 480 000 frames at 48 kHz, 960 000 at 96 kHz.
#[test]
fn set_latency_refuses_what_it_cannot_honour() {
    for (rate, frames) in [(SampleRate::SR_48K, 480_000), (SampleRate::SR_96K, 960_000)] {
        let (mut ed, _exec) = graph_at(
            Plugin::new(),
            Prepare::new(rate, MaxBlock::of(Samples::new(64))),
        );
        ed.commit().expect("commits");
        assert_eq!(MAX_NODE_LATENCY.to_samples_ceil(rate), Samples::new(frames));
        assert_eq!(
            ed.set_latency(NodeKey(99), Latency::new(Samples::new(4))),
            Err(CommitError::NoSuchNode { node: NodeKey(99) })
        );
        // At the limit is fine; one past it is refused, not clamped, and
        // nothing is written.
        let limit = Latency::new(Samples::new(frames));
        ed.set_latency(PLUGIN, limit).expect("at the limit");
        let past = Latency::new(Samples::new(frames + 1));
        assert_eq!(
            ed.set_latency(PLUGIN, past),
            Err(CommitError::LatencyTooLong {
                node: PLUGIN,
                latency: past,
                limit
            }),
            "{rate:?}"
        );
        assert_eq!(
            ed.spec().topology.nodes[&PLUGIN].latency,
            Samples::new(frames),
            "{rate:?}"
        );
    }
    let (mut ed, _exec) = graph(Plugin::new());
    ed.commit().expect("commits");
    ed.reprepare(prepare(128)).expect("re-prepares");
    assert_eq!(
        ed.set_latency(PLUGIN, Latency::new(Samples::new(4))),
        Err(CommitError::Repreparing)
    );
}
