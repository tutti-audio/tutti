//! A root crossfade, driven by hand the way a host drives it: the plan renders
//! the new root's channels and, after them, the old root's; the host mixes the
//! two by the fade law over the frames [`Executor::root_fade`] counts.
//!
//! The sources are constants, so a mixed frame is `old * g_old + new * g_new`
//! with the gains of the law written out in `f64` below, and every figure is
//! analytic. A commit applies at a block's first frame, so a commit "at a block
//! boundary" is any commit.

use tutti_graph::testing as common;

use common::{Kind, TestNode, declick, prepare};
use tutti_graph::{
    Editor, Executor, Fade, FadeLaw, NodeKey, OutPort, Renderer, RootFade, Source,
    TransportPosition, Unforkable,
};
use tutti_types::Samples;

const OLD: f32 = 0.5;
const NEW: f32 = 0.25;
const THIRD: f32 = 0.125;
const BLOCK: usize = 80;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn source(n: u64) -> Source {
    Source::Node(OutPort {
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

/// Three constants at keys 1, 2 and 3 (0.5, 0.25, 0.125), the root taking the
/// first.
fn rig() -> Renderer {
    let (mut ed, exec) = Editor::new(prepare(256));
    for (k, v) in [(1, OLD), (2, NEW), (3, THIRD)] {
        ed.insert(
            key(k),
            "const",
            Unforkable(TestNode::new(Kind::Const { value: v, width: 1 })),
        );
    }
    ed.spec_mut().topology.set_outputs(vec![source(1)]);
    ed.commit().expect("commits");
    let mut r = Renderer::new(ed, exec);
    r.set_block(Samples(BLOCK));
    r
}

/// Declares a new root heard from the constant at `node`, to fade in over
/// `fade`, and commits.
fn declare(r: &mut Renderer, node: u64, fade: Fade) {
    let ed = r.editor_mut();
    ed.set_root_fade(Some(fade));
    ed.spec_mut().root_id = ed.spec().root_id.next();
    ed.spec_mut().topology.set_outputs(vec![source(node)]);
    ed.commit().expect("commits with the old root's channels");
}

/// One block as a host sees it: the fade before it, the new root's channel and
/// the old root's.
struct Block {
    fade: Option<RootFade>,
    new: Vec<f32>,
    old: Option<Vec<f32>>,
}

/// Applies what is queued and renders `frames`.
fn step(r: &mut Renderer, frames: usize) -> Block {
    let exec: &mut Executor = r.executor_mut();
    exec.apply_pending();
    let fade = exec.root_fade();
    let plan = exec.plan().expect("a plan");
    let (rendered, base) = (plan.rendered_outputs(), plan.global_outputs());
    let mut bufs = vec![vec![0.0f32; frames]; rendered];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
    exec.process(frames, &TransportPosition::default(), &[], &mut refs);
    r.editor_mut().collect();
    let old = (rendered > base).then(|| bufs[base].clone());
    Block {
        fade,
        new: bufs.swap_remove(0),
        old,
    }
}

/// What a host hears of `block` and the figures it asserts, per frame: the
/// gains the host takes from the fade and the mix.
fn host_mix(block: &Block) -> Vec<(f32, f32, f32)> {
    (0..block.new.len())
        .map(|i| {
            let (go, gn) = match block.fade {
                Some(f) if f.done.get() + i < f.fade.duration.get() => {
                    f.fade.law.gains(f.done.get() + i, f.fade.duration.get())
                }
                _ => (0.0, 1.0),
            };
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
    Fade::new(Samples(declick()), law)
}

const LAWS: [FadeLaw; 2] = [FadeLaw::EqualAmplitude, FadeLaw::EqualPower];

#[test]
fn the_declick_length_is_three_blocks_of_the_rig() {
    assert_eq!(declick(), 3 * BLOCK, "the figures below count in blocks");
}

/// The old root's gain falls and the new root's rises by the fade law over
/// [`DECLICK`] frames, their invariant holds at every frame, and from the
/// fade's end only the new root sounds.
#[test]
fn a_root_crossfade_mixes_old_and_new_by_the_law_over_declick_frames_then_only_the_new_sounds() {
    for law in LAWS {
        let mut r = rig();
        let before = step(&mut r, BLOCK);
        assert_eq!(before.fade, None);
        assert!(before.new.iter().all(|&x| x == OLD), "the first root alone");
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
            "after the fade only the new root sounds"
        );
    }
}

/// The offline renderer returns the new root's channels whole while the fade
/// runs (mixing the roots is the host's, which folds each by its own root's
/// fold), and the fade's frames advance by what it renders.
#[test]
fn the_renderer_returns_the_new_root_whole_and_counts_the_fade_by_frames_rendered() {
    let mut r = rig();
    r.render(BLOCK);
    declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
    for rendered in [BLOCK, 2 * BLOCK] {
        let got = r.render(BLOCK);
        assert!(got[0].iter().all(|&x| x == NEW), "the new root, unmixed");
        let f = r.executor().root_fade().expect("the fade runs");
        assert_eq!(f.done.get(), rendered, "frames rendered since the commit");
    }
    r.render(BLOCK);
    assert_eq!(r.executor().root_fade(), None, "DECLICK frames end it");
}

/// A commit that leaves the root alone continues the fade from the frames
/// rendered: the next block's first frame is frame `done` of the same fade.
#[test]
fn a_commit_that_leaves_the_root_alone_continues_the_fade_from_its_progress() {
    for law in LAWS {
        let mut r = rig();
        step(&mut r, BLOCK);
        declare(&mut r, 2, fade(law));
        let n = declick();
        check_fade(law, n, 0, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        check_fade(law, n, BLOCK, &host_mix(&step(&mut r, BLOCK)), OLD, NEW);
        // An edit that is not the root's: the commit lands with `done` at two
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
/// control side that has settled the fade compiles the old root away, and one
/// that has not carries the old root on with the fade over. Either way the
/// fade stays over and only the new root sounds.
#[test]
fn a_commit_at_the_block_the_fade_ends_on_leaves_only_the_new_root() {
    for settled in [true, false] {
        let mut r = rig();
        step(&mut r, BLOCK);
        declare(&mut r, 2, fade(FadeLaw::EqualAmplitude));
        for _ in 0..3 {
            step(&mut r, BLOCK);
        }
        if settled {
            r.editor_mut().settle_fades();
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
            "settled {settled}: only the new root sounds"
        );
    }
}

/// A commit that declares the root again begins a new fade from the root that
/// was fading in, at full level, to the newest, counted from the commit's
/// block; the root that was fading out is dropped. Mid-fade and at the block
/// the first fade ends on.
#[test]
fn a_commit_that_declares_the_root_again_begins_a_new_fade_from_the_root_that_was_fading_in() {
    for (blocks, mid_fade) in [(2, true), (3, false)] {
        for law in LAWS {
            let mut r = rig();
            step(&mut r, BLOCK);
            declare(&mut r, 2, fade(law));
            let first = r.executor().plan().map(|p| p.root_id());
            for _ in 0..blocks {
                step(&mut r, BLOCK);
            }
            declare(&mut r, 3, fade(law));
            let n = declick();
            for b in 0..3 {
                let block = step(&mut r, BLOCK);
                let f = block.fade.expect("the second fade runs");
                assert_eq!(f.done.get(), b * BLOCK, "counted from its own commit");
                assert_ne!(Some(f.old), first, "the root that faded in now fades out");
                assert_eq!(f.width, 1);
                check_fade(law, n, b * BLOCK, &host_mix(&block), NEW, THIRD);
            }
            let block = step(&mut r, BLOCK);
            assert_eq!(block.fade, None);
            assert!(
                host_mix(&block).iter().all(|&(_, _, mix)| mix == THIRD),
                "mid_fade {mid_fade}: the newest root alone"
            );
        }
    }
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
            assert_no_alloc::assert_no_alloc(|| exec.apply_pending());
            let rendered = exec.plan().expect("plan").rendered_outputs();
            let mut bufs = vec![vec![0.0f32; BLOCK]; rendered];
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
            assert_no_alloc::assert_no_alloc(|| {
                exec.process(BLOCK, &TransportPosition::default(), &[], &mut refs);
            });
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
            r.executor().root_fade().map(|f| f.done.get()),
            Some(2 * BLOCK),
            "the second fade, counted from its own commit"
        );
    }

    #[test]
    fn continuing_and_beginning_a_root_fade_are_allocation_free() {
        commit_and_render(1);
    }

    #[test]
    fn parallel_continuing_and_beginning_a_root_fade_are_allocation_free() {
        commit_and_render(4);
    }
}
