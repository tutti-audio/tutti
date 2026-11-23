//! Edge fades: a lane an editor marks `EdgeFade::In` or
//! `Out` moves its weight over `DECLICK` along the smootherstep, at the
//! position `(j + 1) / (n + 1)` of frame `j` of an `n`-frame fade, counted from
//! its first block under the fade and across blocks, and the editor learns when
//! it ended (`Editor::finish_fades`). `Editor` itself fades nothing unless
//! asked.
//!
//! Every figure is analytic: the smootherstep `x³(6x² − 15x + 10)` written out
//! in `f64`, to within `f32` rounding.

use tutti_graph::testing as common;

use common::{Kind, TestNode, prepare};
use tutti_graph::{
    Edge, EdgeFade, EdgeId, Editor, Executor, NodeKey, OutputPort, PortSink, PortSource,
    TransportPosition, Unforkable,
};
use tutti_units::Gain;

const BLOCK: usize = 64;
/// `DECLICK` at the rig's 48 kHz.
const SPAN: usize = 240;

/// The smootherstep at frame `j` of the `SPAN`-frame fade.
fn s(j: usize) -> f64 {
    common::smooth_gains(j, SPAN).1
}

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn out(node: u64) -> PortSource {
    PortSource::Node(OutputPort::new(key(node), 0))
}

fn output_edge(from: u64) -> EdgeId {
    EdgeId {
        sink: PortSink::Output(0),
        from: Edge::Direct(out(from)),
    }
}

/// Constants `values` (keys `1..`) summed at output 0.
struct Rig {
    ed: Editor,
    exec: Executor,
}

impl Rig {
    fn new(values: &[f32]) -> Self {
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        for (i, &value) in values.iter().enumerate() {
            let n = i as u64 + 1;
            let k = Kind::Const { value, width: 1 };
            ed.insert(key(n), "const", Unforkable(TestNode::new(k)));
        }
        ed.spec_mut().topology.outputs = vec![(1..=values.len() as u64).map(out).collect()];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        Self { ed, exec }
    }

    fn blocks(&mut self, n: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..n {
            let mut block = vec![f32::NAN; BLOCK];
            self.exec.process(
                BLOCK,
                &TransportPosition::default(),
                &[],
                &mut [block.as_mut_slice()],
            );
            all.extend(block);
        }
        self.ed.collect();
        all
    }

    /// Commit what the spec says now and apply it.
    fn commit(&mut self) {
        self.ed.commit().expect("commits");
        self.exec.apply_pending();
        self.ed.collect();
    }
}

/// **A fade in leaves 0 and is the weight from `DECLICK` on.** Frame `j` of the
/// fade is `base * s((j + 1) / 241)`, across the block boundaries (blocks of
/// 64), strictly inside `(0, base)`, and every frame after the 240th is the
/// base itself.
#[test]
fn a_fade_in_rises_along_the_smootherstep_and_reaches_its_weight_at_declick() {
    assert_eq!(common::declick(), SPAN, "the rig's rate");
    let mut rig = Rig::new(&[1.0]);
    rig.ed
        .set_edge_gain(output_edge(1), Gain::new(0.5))
        .expect("the edge exists");
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();

    let got = rig.blocks(6);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 0.5 * s(j);
        assert!(
            (f64::from(g) - want).abs() < 1e-6,
            "frame {j}: {g}, smootherstep {want}"
        );
        assert!(g > 0.0 && g < 0.5, "frame {j} is neither silent nor full");
    }
    for (j, &g) in got.iter().enumerate().skip(SPAN) {
        assert_eq!(g, 0.5, "frame {j}: at the weight from DECLICK on");
    }
}

/// **A fade is a value, not structure**: setting one on a lane that already has
/// its entry sends no plan, so the executor's plan is the one it had.
#[test]
fn a_commit_that_differs_only_in_an_edge_fade_sends_no_plan() {
    let mut rig = Rig::new(&[1.0]);
    rig.ed
        .set_edge_gain(output_edge(1), Gain::new(0.5))
        .expect("the edge exists");
    rig.commit();
    let before = rig.exec.plan().expect("applied").clone();
    for fade in [EdgeFade::In, EdgeFade::Out, EdgeFade::None] {
        rig.ed
            .set_edge_fade(output_edge(1), fade)
            .expect("the edge exists");
        rig.commit();
        assert!(
            std::sync::Arc::ptr_eq(&before, rig.exec.plan().expect("applied")),
            "{fade:?} sent a plan"
        );
    }
}

/// **A fade out leaves from the weight it has and its last nonzero frame is
/// inside the span**; it then reads 0, and the editor is told once and drops
/// the edge, after which a commit leaves the sink silent.
#[test]
fn a_fade_out_ends_inside_the_span_and_the_edge_leaves_the_spec() {
    let mut rig = Rig::new(&[1.0]);
    // Steady for a block first: the fade leaves from weight 1.
    rig.blocks(1);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::Out)
        .expect("the edge exists");
    rig.commit();

    let got = rig.blocks(6);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 1.0 - s(j);
        assert!(
            (f64::from(g) - want).abs() < 1e-6,
            "frame {j}: {g}, smootherstep {want}"
        );
    }
    let last = got.iter().rposition(|&x| x != 0.0).expect("it was heard");
    assert!(
        last < SPAN,
        "the last nonzero frame {last} is inside the span"
    );
    assert!(got[SPAN..].iter().all(|&x| x == 0.0), "and silence after");

    assert!(
        rig.ed.spec().has_edge(output_edge(1)),
        "held until reported"
    );
    assert!(rig.ed.finish_fades(), "the ended fade changed the spec");
    assert!(!rig.ed.spec().has_edge(output_edge(1)), "and took the edge");
    assert!(!rig.ed.finish_fades(), "reported once");
    rig.commit();
    assert!(rig.blocks(1).iter().all(|&x| x == 0.0));
}

/// **A fade in that ended leaves a plain edge**: the report drops the fade and
/// the entry it came with, and the sum is the plain sum again.
#[test]
fn an_ended_fade_in_leaves_a_plain_edge() {
    let mut rig = Rig::new(&[1.0, 2.0]);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();
    rig.blocks(5);
    assert!(rig.ed.finish_fades());
    assert!(
        !rig.ed.spec().gains.contains_key(&output_edge(1)),
        "no entry: the edge is plain again"
    );
    rig.commit();
    assert!(rig.blocks(1).iter().all(|&x| x == 3.0));
}

/// **An edge brought back while it fades out turns round from where it is**
/// (no step): `In` after a half-done `Out` rises from the weight the lane had.
#[test]
fn a_fade_reversed_midway_continues_from_the_weight_it_had() {
    let mut rig = Rig::new(&[1.0]);
    rig.blocks(1);
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::Out)
        .expect("the edge exists");
    rig.commit();
    let down = rig.blocks(2); // 128 frames of the fade out
    let at = *down.last().expect("frames");
    rig.ed
        .set_edge_fade(output_edge(1), EdgeFade::In)
        .expect("the edge exists");
    rig.commit();
    let up = rig.blocks(1);
    assert!(
        (up[0] - at).abs() < 1e-5,
        "rises from the weight the fade out had reached: {} from {at}",
        up[0]
    );
    assert!(up[BLOCK - 1] > up[0], "and rises");
}

/// **The parallel executor renders a graph in the middle of fades exactly as
/// the one-participant one**, bit for bit, across blocks of different lengths, with
/// fades in, out and a weight moved under it.
#[test]
fn parallel_equals_one_participant_for_a_graph_mid_fade() {
    fn run(workers: usize) -> Vec<u32> {
        let values: Vec<f32> = (1..=8).map(|i| i as f32 * 0.37 - 1.1).collect();
        let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
        if workers > 1 {
            exec.set_pool(Some(common::TestPool::new(workers)));
        }
        for (i, &value) in values.iter().enumerate() {
            let k = Kind::Const { value, width: 1 };
            ed.insert(key(i as u64 + 1), "const", Unforkable(TestNode::new(k)));
        }
        ed.spec_mut().topology.outputs = vec![(1..=8).map(out).collect()];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let mut got = Vec::new();
        let mut run_blocks = |exec: &mut Executor, ed: &mut Editor, sizes: &[usize]| {
            for &n in sizes {
                let mut block = vec![f32::NAN; n];
                exec.process(
                    n,
                    &TransportPosition::default(),
                    &[],
                    &mut [block.as_mut_slice()],
                );
                ed.collect();
                got.extend(block.iter().map(|x| x.to_bits()));
            }
        };
        run_blocks(&mut exec, &mut ed, &[64, 64]);
        for k in 1..=8u64 {
            let fade = if k % 2 == 0 {
                EdgeFade::In
            } else {
                EdgeFade::Out
            };
            ed.set_edge_fade(output_edge(k), fade).expect("edge");
            ed.set_edge_gain(output_edge(k), Gain::new(0.25 * k as f64))
                .expect("edge");
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        assert_eq!(exec.is_parallel(), workers > 1);
        run_blocks(&mut exec, &mut ed, &[37, 64, 5, 64, 64, 11, 64, 64, 64]);
        got
    }
    let serial = run(1);
    assert_eq!(run(4), serial, "four participants against one");
}
