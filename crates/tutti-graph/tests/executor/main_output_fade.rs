//! A main output crossfade, driven by hand the way a host drives it: the plan renders
//! the new main output's channels and, after them, the old main output's; the host mixes the
//! two by the fade law over the frames [`Executor::main_output_fade`] counts.
//!
//! The sources are constants, so a mixed frame is `old * g_old + new * g_new`
//! with the gains of the law written out in `f64` below, and every figure is
//! analytic. A commit applies at a block's first frame, so a commit "at a block
//! boundary" is any commit.

use tutti_graph::testing as common;

use common::{Kind, TestNode, declick, prepare};
use tutti_graph::{
    Edge, EdgeGain, EdgeId, Editor, Executor, Fade, FadeLaw, MainOutputFade, NodeKey, OutputPort,
    PortSink, PortSource, Renderer, TransportPosition, Unforkable,
};
use tutti_units::{Amplitude, Latency, Samples};

const OLD: f32 = 0.5;
const NEW: f32 = 0.25;
const THIRD: f32 = 0.125;
const BLOCK: usize = 80;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn source(n: u64) -> PortSource {
    PortSource::Node(OutputPort {
        node: key(n),
        port: 0,
    })
}

/// The gains `(out, in)` at frame `k` of an `n`-frame fade: the law written
/// out, with the smootherstep of its progress `x = (k + 1) / (n + 1)`.
fn gains(law: FadeLaw, k: usize, n: usize) -> (f64, f64) {
    let x = (k + 1) as f64 / (n + 1) as f64;
    let s = x * x * x * (6.0 * x * x - 15.0 * x + 10.0);
    match law {
        FadeLaw::EqualAmplitude => (1.0 - s, s),
        FadeLaw::EqualPower => {
            let a = s * std::f64::consts::FRAC_PI_2;
            (a.cos(), a.sin())
        }
    }
}

/// Three constants at keys 1, 2 and 3 (0.5, 0.25, 0.125), the main output taking the
/// first.
fn rig() -> Renderer {
    rig_with(
        &[
            (
                1,
                Kind::Const {
                    value: OLD,
                    width: 1,
                },
            ),
            (
                2,
                Kind::Const {
                    value: NEW,
                    width: 1,
                },
            ),
            (
                3,
                Kind::Const {
                    value: THIRD,
                    width: 1,
                },
            ),
        ],
        &[1],
    )
}

/// The nodes at their keys, the main output taking `outputs`.
fn rig_with(nodes: &[(u64, Kind)], outputs: &[u64]) -> Renderer {
    let (mut ed, exec) = Editor::new(prepare(256));
    for (k, kind) in nodes {
        ed.insert(key(*k), "node", Unforkable(TestNode::new(kind.clone())));
    }
    ed.spec_mut()
        .topology
        .set_outputs(outputs.iter().map(|&n| source(n)).collect::<Vec<_>>());
    ed.commit().expect("commits");
    let mut r = Renderer::new(ed, exec);
    r.set_block(Samples::new(BLOCK));
    r
}

/// Declares a new main output heard from the constant at `node`, to fade in over
/// `fade`, and commits.
fn declare(r: &mut Renderer, node: u64, fade: Fade) {
    let ed = r.editor_mut();
    ed.set_main_output_fade(Some(fade));
    ed.spec_mut().main_output_id = ed.spec().main_output_id.next();
    ed.spec_mut().topology.set_outputs(vec![source(node)]);
    ed.commit()
        .expect("commits with the old main output's channels");
}

/// One block as a host sees it: the fade before it, the new main output's channel and
/// the old main output's.
struct Block {
    fade: Option<MainOutputFade>,
    new: Vec<f32>,
    old: Option<Vec<f32>>,
    /// Every channel the plan rendered: the main output's, then the old main output's.
    all: Vec<Vec<f32>>,
}

/// Applies what is queued and renders `frames`.
fn step(r: &mut Renderer, frames: usize) -> Block {
    let exec: &mut Executor = r.executor_mut();
    exec.apply_pending();
    let fade = exec.main_output_fade();
    let plan = exec.plan().expect("a plan");
    let (rendered, base) = (plan.rendered_outputs(), plan.main_outputs());
    let mut bufs = vec![vec![0.0f32; frames]; rendered];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
    exec.process(frames, &TransportPosition::default(), &[], &mut refs);
    r.editor_mut().collect();
    let old = (rendered > base).then(|| bufs[base].clone());
    Block {
        fade,
        new: bufs[0].clone(),
        old,
        all: bufs,
    }
}

/// What a host hears of `block` and the figures it asserts, per frame: the
/// gains the host takes from the fade and the mix.
fn host_mix(block: &Block) -> Vec<(f32, f32, f32)> {
    (0..block.new.len())
        .map(|i| {
            let (go, gn) = match block.fade {
                Some(f) if f.done.get() + i < f.fade.duration.get() => {
                    f.fade.law.gains(f.done + Samples::new(i), f.fade.duration)
                }
                _ => (Amplitude::SILENT, Amplitude::UNITY),
            };
            let (go, gn) = (go.get() as f32, gn.get() as f32);
            let old = block.old.as_ref().map_or(0.0, |o| o[i]);
            (go, gn, old * go + block.new[i] * gn)
        })
        .collect()
}

const TOLERANCE: f64 = 2e-6;

fn near(got: f32, want: f64, what: &str) {
    assert!(
        (f64::from(got) - want).abs() < TOLERANCE,
        "{what}: {got} against {want}"
    );
}

/// Frames `first..first + mixed.len()` of a fade `n` long that trades `from`
/// for `to`, checked against the law written out: both gains, the law's
/// invariant (the gains sum to one, or their squares do) and the mix.
fn check_fade(law: FadeLaw, n: usize, first: usize, mixed: &[(f32, f32, f32)], from: f32, to: f32) {
    for (i, &(go, gn, mix)) in mixed.iter().enumerate() {
        let k = first + i;
        let (want_out, want_in) = if k < n { gains(law, k, n) } else { (0.0, 1.0) };
        near(go, want_out, &format!("old gain at frame {k}"));
        near(gn, want_in, &format!("new gain at frame {k}"));
        let invariant = match law {
            FadeLaw::EqualAmplitude => f64::from(go) + f64::from(gn),
            _ => f64::from(go).powi(2) + f64::from(gn).powi(2),
        };
        near(
            invariant as f32,
            1.0,
            &format!("the law's invariant at frame {k}"),
        );
        near(
            mix,
            want_out * f64::from(from) + want_in * f64::from(to),
            &format!("the mix at frame {k}"),
        );
    }
}

/// The fade's length: [`DECLICK`] frames, three blocks.
fn fade(law: FadeLaw) -> Fade {
    Fade::new(Samples::new(declick())).law(law)
}

const LAWS: [FadeLaw; 2] = [FadeLaw::EqualAmplitude, FadeLaw::EqualPower];

#[test]
fn the_declick_length_is_three_blocks_of_the_rig() {
    assert_eq!(declick(), 3 * BLOCK, "the figures below count in blocks");
}

/// The old main output's gain falls and the new main output's rises by the fade law over
/// [`DECLICK`] frames, their invariant holds at every frame, and from the
/// fade's end only the new main output sounds.
#[test]
fn a_main_output_crossfade_mixes_old_and_new_by_the_law_over_declick_frames_then_only_the_new_sounds()
 {
    for law in LAWS {
        let mut r = rig();
        let before = step(&mut r, BLOCK);
        assert_eq!(before.fade, None);
        assert!(
            before.new.iter().all(|&x| x == OLD),
            "the first main output alone"
        );
        declare(&mut r, 2, fade(law));
        let n = declick();
        for b in 0..3 {
            let block = step(&mut r, BLOCK);
            let f = block.fade.expect("the fade runs");
            assert_eq!(
                (f.done.get(), f.width, f.fade.duration.get()),
                (b * BLOCK, 1, n)
            );
            check_fade(law, n, b * BLOCK, &host_mix(&block), OLD, NEW);
        }
        let block = step(&mut r, BLOCK);
        assert_eq!(block.fade, None, "the fade ended with its last frame");
        let mixed = host_mix(&block);
        check_fade(law, n, n, &mixed, OLD, NEW);
        assert!(
            mixed
                .iter()
                .all(|&(go, gn, mix)| go == 0.0 && gn == 1.0 && mix == NEW),
            "after the fade only the new main output sounds"
        );
    }
}

/// The offline renderer returns the new main output's channels whole while the fade
/// runs (mixing the main outputs is the host's, which folds each by its own main output's
/// fold), and the fade's frames advance by what it renders.
#[test]
fn the_renderer_returns_the_new_main_output_whole_and_counts_the_fade_by_frames_rendered() {
    let mut r = rig();
    r.render(BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    for rendered in [BLOCK, 2 * BLOCK] {
        let got = r.render(BLOCK);
        assert!(
            got[0].iter().all(|&x| x == NEW),
            "the new main output, unmixed"
        );
        let f = r.executor().main_output_fade().expect("the fade runs");
        assert_eq!(f.done.get(), rendered, "frames rendered since the commit");
    }
    r.render(BLOCK);
    assert_eq!(
        r.executor().main_output_fade(),
        None,
        "DECLICK frames end it"
    );
}

/// A commit that leaves the main output alone continues the fade from the frames
/// rendered: the next block's first frame is frame `done` of the same fade.
#[test]
fn a_commit_that_leaves_the_main_output_alone_continues_the_fade_from_its_progress() {
    for law in LAWS {
        let mut r = rig();
        step(&mut r, BLOCK);
        declare(&mut r, 2, fade(law));
        let n = declick();
        check_fade(law, n, 0, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        check_fade(law, n, BLOCK, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        // An edit that is not the main output's: the commit lands with `done` at two
        // blocks.
        r.editor_mut().insert(
            key(9),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            })),
        );
        r.editor_mut().commit().expect("commits");
        let block = step(&mut r, BLOCK);
        let f = block.fade.expect("the same fade runs on");
        assert_eq!(f.done.get(), 2 * BLOCK, "progress carried into the plan");
        assert_eq!(r.executor().plan().expect("plan").rendered_outputs(), 2);
        check_fade(law, n, 2 * BLOCK, &host_mix(&block), OLD, NEW);
        let block = step(&mut r, BLOCK);
        assert_eq!(block.fade, None);
        check_fade(law, n, n, &host_mix(&block), OLD, NEW);
    }
}

/// A commit that lands as the fade has just ended does not restart it: the
/// control side that has settled the fade compiles the old main output away, and one
/// that has not carries the old main output on with the fade over. Either way the
/// fade stays over and only the new main output sounds.
#[test]
fn a_commit_at_the_block_the_fade_ends_on_leaves_only_the_new_main_output() {
    for settled in [true, false] {
        let mut r = rig();
        step(&mut r, BLOCK);
        declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
        for _ in 0..3 {
            step(&mut r, BLOCK);
        }
        if settled {
            r.editor_mut().finish_fades();
        }
        r.editor_mut().insert(
            key(9),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            })),
        );
        r.editor_mut().commit().expect("commits");
        let block = step(&mut r, BLOCK);
        assert_eq!(block.fade, None, "settled {settled}");
        let outputs = r.executor().plan().expect("plan").rendered_outputs();
        assert_eq!(outputs, if settled { 1 } else { 2 }, "settled {settled}");
        let mixed = host_mix(&block);
        assert!(
            mixed
                .iter()
                .all(|&(go, gn, mix)| go == 0.0 && gn == 1.0 && mix == NEW),
            "settled {settled}: only the new main output sounds"
        );
    }
}

/// The main output's id a plan renders under.
fn main_output_in_force(r: &Renderer) -> tutti_graph::MainOutputId {
    r.executor().plan().expect("plan").main_output_id()
}

/// A declaration made while a fade runs ends it at that block and begins
/// another, from the main output that was fading in (at full level) to the newest;
/// the main output that was fading out is dropped, a step of `1 - g`.
#[test]
fn a_redeclare_mid_fade_begins_a_new_fade_from_the_main_output_that_was_fading_in() {
    for law in LAWS {
        let mut r = rig();
        step(&mut r, BLOCK);
        declare(&mut r, 2, fade(law));
        let first = r.editor().spec().main_output_id;
        let n = declick();
        check_fade(law, n, 0, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        check_fade(law, n, BLOCK, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        declare(&mut r, 3, fade(law));
        let newest = r.editor().spec().main_output_id;
        for b in 0..3 {
            let block = step(&mut r, BLOCK);
            let f = block.fade.expect("the second fade runs");
            assert_eq!(f.done.get(), b * BLOCK, "counted from its own commit");
            assert_eq!(f.old, first, "the main output that faded in now fades out");
            assert_eq!(main_output_in_force(&r), newest);
            check_fade(law, n, b * BLOCK, &host_mix(&block), NEW, THIRD);
        }
        let block = step(&mut r, BLOCK);
        assert_eq!(block.fade, None);
        assert!(host_mix(&block).iter().all(|&(_, _, mix)| mix == THIRD));
    }
}

/// A declaration with no fade set cuts at its block and ends the running one.
#[test]
fn a_hard_redeclare_mid_fade_cuts_at_its_block() {
    let mut r = rig();
    step(&mut r, BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    step(&mut r, BLOCK);
    r.editor_mut().set_main_output_fade(None);
    let ed = r.editor_mut();
    ed.spec_mut().main_output_id = ed.spec().main_output_id.next();
    ed.spec_mut().topology.set_outputs(vec![source(3)]);
    ed.commit().expect("commits");
    let newest = r.editor().spec().main_output_id;
    let block = step(&mut r, BLOCK);
    assert_eq!(block.fade, None);
    assert_eq!(main_output_in_force(&r), newest);
    assert_eq!(r.executor().plan().expect("plan").rendered_outputs(), 1);
    assert!(
        block.new.iter().all(|&x| x == THIRD),
        "the newest main output, whole, from frame 0"
    );
}

/// A width change that does not declare the main output again ends the fade as a
/// cut: the old main output's channels cannot stand beside a main output of another width.
#[test]
fn a_width_change_without_a_new_main_output_cuts_the_fade() {
    let mut r = rig();
    step(&mut r, BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    step(&mut r, BLOCK);
    r.editor_mut()
        .spec_mut()
        .topology
        .set_outputs(vec![source(2), source(2)]);
    r.editor_mut().commit().expect("commits");
    let block = step(&mut r, BLOCK);
    assert_eq!(block.fade, None);
    assert_eq!(r.executor().plan().expect("plan").rendered_outputs(), 2);
    assert!(
        block.all.iter().all(|c| c.iter().all(|&x| x == NEW)),
        "both channels of the new main output, whole, from frame 0"
    );
}

/// The carried fade renders the serial executor's samples bit for bit on a
/// pool, through a continuing commit, a re-declaration and its fade.
#[test]
fn a_carried_fade_renders_bit_for_bit_the_same_on_the_parallel_executor() {
    fn trace(pooled: bool) -> Vec<u32> {
        let nodes: Vec<(u64, Kind)> = (1..=8)
            .map(|k| {
                (
                    k,
                    Kind::Const {
                        value: k as f32 / 16.0,
                        width: 1,
                    },
                )
            })
            .collect();
        let mut r = rig_with(&nodes, &[1, 2]);
        if pooled {
            r.executor_mut().set_pool(Some(common::TestPool::new(4)));
        }
        let law = FadeLaw::EqualPower;
        let mut bits = Vec::new();
        let mut record = |r: &mut Renderer| {
            let b = step(r, BLOCK);
            for c in b.all {
                bits.extend(c.iter().map(|x| x.to_bits()));
            }
            bits.push(b.fade.map_or(u32::MAX, |f| f.done.get() as u32));
        };
        record(&mut r);
        let ed = r.editor_mut();
        ed.set_main_output_fade(Some(fade(law)));
        ed.spec_mut().main_output_id = ed.spec().main_output_id.next();
        ed.spec_mut()
            .topology
            .set_outputs(vec![source(3), source(4)]);
        ed.commit().expect("commits");
        record(&mut r);
        r.editor_mut().remove(key(7));
        r.editor_mut().commit().expect("commits");
        record(&mut r);
        declare_pair(&mut r, 5, 6, fade(law));
        for _ in 0..6 {
            record(&mut r);
        }
        bits
    }
    let serial = trace(false);
    assert!(
        serial.iter().any(|&b| b != 0),
        "the graph rendered something"
    );
    assert_eq!(serial, trace(true));
}

/// As `declare`, two channels wide.
fn declare_pair(r: &mut Renderer, a: u64, b: u64, fade: Fade) {
    let ed = r.editor_mut();
    ed.set_main_output_fade(Some(fade));
    ed.spec_mut().main_output_id = ed.spec().main_output_id.next();
    ed.spec_mut()
        .topology
        .set_outputs(vec![source(a), source(b)]);
    ed.commit().expect("commits");
}

/// A carried fade reports its end once, under the seq that began it, and
/// settling it compiles the old main output's channels away.
#[test]
fn a_carried_fade_reports_its_end_once_and_settling_drops_the_bank() {
    let mut r = rig();
    step(&mut r, BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    step(&mut r, BLOCK);
    r.editor_mut().insert(
        key(9),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
    );
    r.editor_mut().commit().expect("commits");
    step(&mut r, BLOCK);
    assert!(
        !r.editor_mut().finish_fades(),
        "the fade has a block to run"
    );
    step(&mut r, BLOCK);
    assert_eq!(r.executor().main_output_fade(), None);
    assert!(r.editor_mut().finish_fades(), "reported once");
    assert!(!r.editor_mut().finish_fades(), "and not again");
    r.editor_mut().commit().expect("commits");
    let block = step(&mut r, BLOCK);
    assert_eq!(r.executor().plan().expect("plan").rendered_outputs(), 1);
    assert!(block.new.iter().all(|&x| x == NEW));
}

/// A source removed in a carried commit leaves the bank, and the others keep
/// their weights: the old main output is `[0.5, 0.25 * 0.5]`.
#[test]
fn a_carried_commit_that_removes_an_old_source_drops_it_from_the_bank_and_keeps_the_others() {
    let mut r = rig_with(
        &[
            (
                1,
                Kind::Const {
                    value: OLD,
                    width: 1,
                },
            ),
            (
                2,
                Kind::Const {
                    value: NEW,
                    width: 1,
                },
            ),
            (
                3,
                Kind::Const {
                    value: THIRD,
                    width: 1,
                },
            ),
        ],
        &[1, 2],
    );
    let weighted = EdgeId {
        sink: PortSink::Output(1),
        from: Edge::Direct(source(2)),
    };
    r.editor_mut()
        .spec_mut()
        .gains
        .insert(weighted, EdgeGain::new(Amplitude::new(0.5)));
    r.editor_mut().commit().expect("commits");
    step(&mut r, BLOCK);
    r.editor_mut().spec_mut().gains.remove(&weighted);
    declare_pair(&mut r, 3, 3, fade(FadeLaw::EqualAmplitude));
    let block = step(&mut r, BLOCK);
    assert!(block.all[2].iter().all(|&x| x == OLD));
    assert!(block.all[3].iter().all(|&x| x == NEW * 0.5), "weighted");
    r.editor_mut().remove(key(1));
    r.editor_mut().commit().expect("commits");
    let block = step(&mut r, BLOCK);
    assert!(block.fade.is_some(), "the fade goes on");
    assert!(
        block.all[2].iter().all(|&x| x == 0.0),
        "the removed source left the bank"
    );
    assert!(
        block.all[3].iter().all(|&x| x == NEW * 0.5),
        "the other keeps its weight"
    );
}

/// When every old source is gone the carried commit has no bank: a cut, with
/// the inserted node, wired to the main output, heard whole from the commit's block.
#[test]
fn a_carried_commit_that_removes_every_old_source_cuts() {
    let mut r = rig();
    step(&mut r, BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    step(&mut r, BLOCK);
    let ed = r.editor_mut();
    ed.remove(key(1));
    ed.insert(
        key(9),
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
    );
    ed.spec_mut().topology.set_outputs(vec![source(9)]);
    ed.commit().expect("commits");
    let block = step(&mut r, BLOCK);
    assert_eq!(block.fade, None);
    assert_eq!(r.executor().plan().expect("plan").rendered_outputs(), 1);
    assert!(block.new.iter().all(|&x| x == 1.0));
}

/// A carried commit that changes an old source's latency leaves the old
/// main output's alignment where the fade began it: the old main output is `[emitter, lag]`,
/// the emitter's staircase held 48 frames back to align with the lag. The new
/// main output has no latency, yet the staircase stays 48 frames back through the
/// commit, and only the lag, whose latency moved, is touched.
#[test]
fn a_carried_commit_that_changes_an_old_sources_latency_keeps_the_banks_alignment() {
    let lag = |latency| Kind::Spec {
        behaviour: tutti_graph::testing::SpecBehaviour::Dc(NEW),
        ins: 0,
        outs: 1,
        latency,
        tail: tutti_units::Tail::None,
    };
    let mut r = rig_with(
        &[
            (
                1,
                Kind::Emitter {
                    period: 80,
                    phase: 0,
                },
            ),
            (2, lag(48)),
            (
                3,
                Kind::Const {
                    value: THIRD,
                    width: 1,
                },
            ),
        ],
        &[1, 2],
    );
    // The emitter's count after the block `t` falls in, aligned 48 frames late.
    let aligned = |t: usize| {
        if t < 48 {
            0.0
        } else {
            ((t - 48) / BLOCK + 1) as f32
        }
    };
    for _ in 0..2 {
        step(&mut r, BLOCK);
    }
    // A new main output with no latency: the plan's total drops from 48 to 0, and
    // the old main output's channels stay aligned to the 48 they were heard at.
    declare(&mut r, 3, fade(FadeLaw::EqualAmplitude));
    let mut t = 2 * BLOCK;
    let heard = |r: &mut Renderer, t: &mut usize| {
        let block = step(r, BLOCK);
        for (i, &x) in block.all[1].iter().enumerate() {
            assert_eq!(
                x,
                aligned(*t + i),
                "the emitter's bank channel at frame {}",
                *t + i
            );
        }
        *t += BLOCK;
        block
    };
    heard(&mut r, &mut t);
    r.editor_mut()
        .set_latency(key(2), Latency::new(Samples::new(96)))
        .expect("sets");
    r.editor_mut().commit().expect("commits");
    let block = heard(&mut r, &mut t);
    assert!(block.fade.is_some(), "the fade goes on through the commit");
    assert!(
        block.all[2].iter().all(|&x| x == NEW),
        "the lag's channel is unmoved"
    );
}

/// Applying a commit that continues a fade, one that begins another, and
/// rendering through them allocates nothing, on the serial executor and on a
/// pool.
mod no_alloc {
    use super::*;

    fn commit_and_render(workers: usize) {
        let mut r = rig();
        if workers > 1 {
            r.executor_mut()
                .set_pool(Some(common::TestPool::no_alloc(workers)));
        }
        let law = FadeLaw::EqualPower;
        let run = |r: &mut Renderer| {
            let exec = r.executor_mut();
            exec.apply_pending();
            let rendered = exec.plan().expect("plan").rendered_outputs();
            let mut bufs = vec![vec![0.0f32; BLOCK]; rendered];
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
            exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
            r.editor_mut().collect();
        };
        run(&mut r);
        declare(&mut r, 2, fade(law));
        run(&mut r);
        r.editor_mut().insert(
            key(9),
            "const",
            Unforkable(TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            })),
        );
        r.editor_mut().commit().expect("commits");
        run(&mut r);
        declare(&mut r, 3, fade(law));
        run(&mut r);
        run(&mut r);
        assert_eq!(
            r.executor().main_output_fade().map(|f| f.done.get()),
            Some(2 * BLOCK),
            "the second fade, counted from its own commit"
        );
    }

    #[test]
    fn continuing_and_beginning_a_main_output_fade_are_allocation_free() {
        commit_and_render(1);
    }

    #[test]
    fn parallel_continuing_and_beginning_a_main_output_fade_are_allocation_free() {
        commit_and_render(4);
    }
}

/// `Editor::main_output_fade_running` is true from the commit that sends the old
/// main output until the fade's end is settled, and false after a cut.
#[test]
fn main_output_fade_running_is_true_while_the_bank_runs_and_false_after_settling_or_a_cut() {
    let mut r = rig();
    step(&mut r, BLOCK);
    assert!(
        !r.editor().main_output_fade_running(),
        "no main output change yet"
    );
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    assert!(
        r.editor().main_output_fade_running(),
        "sent, not yet rendered"
    );
    for _ in 0..3 {
        step(&mut r, BLOCK);
    }
    assert!(
        r.editor().main_output_fade_running(),
        "rendered through, but not reported until settled"
    );
    assert!(r.editor_mut().finish_fades());
    assert!(!r.editor().main_output_fade_running(), "settled");

    let mut r = rig();
    step(&mut r, BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    step(&mut r, BLOCK);
    r.editor_mut().set_main_output_fade(None);
    let ed = r.editor_mut();
    ed.spec_mut().main_output_id = ed.spec().main_output_id.next();
    ed.spec_mut().topology.set_outputs(vec![source(3)]);
    ed.commit().expect("commits");
    assert!(
        !r.editor().main_output_fade_running(),
        "a cut carries no old main output"
    );
}
