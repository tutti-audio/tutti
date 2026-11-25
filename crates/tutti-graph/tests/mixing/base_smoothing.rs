//! The graph smooths a node param's base: a cell that moves ramps linearly
//! over the declick, in the unit's own `f64`, across blocks, from the last
//! frame delivered. It is delivered as [`ParamInput::Smoothing`], which is not a
//! drive. A port that lands whole, a unit's first block, a re-prepare and a
//! fork have no ramp, and a port leaving its source is carried by the
//! source's declick alone.

use tutti_graph::testing::WireDeclared;

use tutti_buffers::{ChannelLayout, Float};
use tutti_graph::testing as common;
use tutti_graph::{
    Cx, Editor, EventOutputPort, Executor, ForkByClone, ForkTarget, Io, Node, NodeKey, OutputPort,
    ParamEvent, ParamIn, ParamInput, ParamNode, ParamRamp, ParamSet, Params, PortSource, Prepare,
    Shape, Status, declare_params,
};
use tutti_rt::Param;
use tutti_units::{Hz, ParamKey, Samples, UnitParam};

use common::prepare;

/// The smoothed port's output, the whole port's, then each port's kind of
/// delivery: 0 `Base`, 1 `Smoothing`, 2 `Frames`; and 1 on each frame where
/// `narrowed()` is `values` cast.
const OUTPUTS: u16 = 5;
const OUT_N: usize = OUTPUTS as usize;

/// A node with a smoothed `Cutoff` and a `Frequency` that lands whole.
#[derive(Clone)]
struct Knob {
    smooth: Param<Hz>,
    whole: Param<Hz>,
}

declare_params!([] Knob, n {
    Cutoff => Hz = &n.smooth,
    Frequency => Hz { whole } = &n.whole,
});

impl<F: Float> Node<F> for Knob {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(OUTPUTS))
            .param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let smooth = self.declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io);
        let whole = self.declared_param::<{ UnitParam::Frequency as u16 }, _>(&io);
        let kind = |p: ParamInput<'_, F>| match p {
            ParamInput::Base => 0.0,
            ParamInput::Smoothing { .. } => 1.0,
            ParamInput::Frames { .. } => 2.0,
        };
        let (base_smooth, base_whole) = (self.smooth.load().get(), self.whole.load().get());
        for (k, (p, base)) in [(smooth, base_smooth), (whole, base_whole)]
            .into_iter()
            .enumerate()
        {
            let out = io.output(k);
            match p.narrowed() {
                Some(v) => out.copy_from_slice(v),
                None => out.fill(F::from_f64(base)),
            }
        }
        io.output(2).fill(F::from_f64(kind(smooth)));
        io.output(3).fill(F::from_f64(kind(whole)));
        let cast = match (smooth.frames(), smooth.narrowed()) {
            (Some(v), Some(n)) => v.iter().zip(n).all(|(&v, &n)| F::from_f64(v) == n),
            _ => true,
        };
        io.output(4).fill(if cast { F::ONE } else { F::ZERO });
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for Knob {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.smooth.detach();
        fork.whole.detach();
        fork
    }
}

/// Sends a step of `Cutoff` to `value` on absolute frame `at`.
#[derive(Clone)]
struct Speak {
    at: u64,
    value: f64,
}

impl Speak {
    const SHAPE: Shape = Shape::EMPTY
        .events(0, 1)
        .param_events()
        .event_capacity(tutti_graph::testing::cap_of(4));
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl<F: Float> Node<F> for Speak {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let start = cx.env.frame.get();
        if (start..start + io.frames() as u64).contains(&self.at) {
            let offset = io.offset((self.at - start) as usize).expect("inside");
            let step = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(self.value), Samples::new(0));
            let _ = io
                .event_out(Self::OUT)
                .push_param(ParamEvent::ramp(offset, step));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

const KNOB: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(2);
const CUTOFF: ParamIn = ParamIn {
    node: KNOB,
    param: tutti_units::UnitAddr::first(UnitParam::Cutoff),
};

/// The declick in frames at the test rate.
fn declick() -> usize {
    tutti_graph::declick_frames(prepare(1).sample_rate()).get()
}

/// One value `f32` cannot hold: the smoothed port's base and a ramp's end.
const B: f64 = 0.1 + 1e-12;

struct Rig<F: Float> {
    ed: Editor<F>,
    exec: Executor<F>,
    smooth: Param<Hz>,
    whole: Param<Hz>,
    block: usize,
}

impl<F: Float> Rig<F> {
    /// A knob at `smooth` and `whole`, a block of `block` frames.
    fn new(block: usize, smooth: f64, whole: f64) -> Self {
        let (mut ed, mut exec) = Editor::<F>::with_format(prepare(block));
        let (s, w) = (Param::new(Hz::new(smooth)), Param::new(Hz::new(whole)));
        let (smooth, whole) = (s.handle(), w.handle());
        ed.insert(
            KNOB,
            "knob",
            Params(Knob {
                smooth: s,
                whole: w,
            }),
        );
        ed.wire_declared_outputs((0..OUTPUTS).map(|c| PortSource::Node(OutputPort::new(KNOB, c))))
            .unwrap();
        ed.commit().expect("commits");
        exec.apply_pending();
        Self {
            ed,
            exec,
            smooth,
            whole,
            block,
        }
    }

    /// Renders `frames` frames in blocks of the rig's size: the outputs, in
    /// the unit's `f64`.
    fn run(&mut self, frames: usize) -> [Vec<f64>; OUT_N] {
        let mut out: [Vec<f64>; OUT_N] = Default::default();
        for _ in 0..frames / self.block {
            let b = common::block(&mut self.exec, self.block, OUT_N);
            self.ed.collect();
            for (o, c) in out.iter_mut().zip(b) {
                o.extend(c.iter().map(|x| x.to_f64()));
            }
        }
        out
    }
}

/// **A base step `a → b` at rest ramps over the declick: frame `j` of `n` is
/// `a + (b - a)·(j + 1)/n`, in `f64`, whatever the blocks (64 frames and 1).**
/// The last frame is `b`'s bits (a value `f32` cannot hold), the frames after
/// it are `b`, and every block the ramp touches is `Smoothing`, never driven.
#[test]
fn a_base_step_ramps_linearly_in_f64_over_the_declick_at_block_sizes_64_and_1() {
    assert_ne!(f64::from(B as f32), B, "f32 cannot hold the target");
    let (a, n) = (1.0, declick());
    for block in [64usize, 1] {
        let mut rig = Rig::<f64>::new(block, a, 3.0);
        let before = rig.run(2 * block);
        assert!(before[0].iter().all(|&x| x == a), "block {block}: at rest");
        assert!(before[2].iter().all(|&x| x == 0.0), "block {block}: Base");
        rig.smooth.store(Hz::new(B));
        let frames = (n + block).div_ceil(block) * block + block;
        let out = rig.run(frames);
        for j in 0..n - 1 {
            let want = a + (B - a) * ((j + 1) as f64 / n as f64);
            assert!(
                (out[0][j] - want).abs() <= 4.0 * f64::EPSILON,
                "block {block}, frame {j}: {} vs {want}",
                out[0][j]
            );
            assert_eq!(out[2][j], 1.0, "block {block}, frame {j}: smoothing");
            assert_eq!(
                out[4][j], 1.0,
                "block {block}, frame {j}: narrowed is the cast"
            );
        }
        assert_eq!(
            out[0][n - 1].to_bits(),
            B.to_bits(),
            "block {block}: the last frame is b"
        );
        assert!(
            out[0][n - 1..].iter().all(|&x| x == B),
            "block {block}: held"
        );
        assert_eq!(*out[2].last().unwrap(), 0.0, "block {block}: Base again");
    }
}

/// **`narrowed()` is `values` cast, elementwise, in an `f32` graph**, and the
/// frames are the `f64` ramp: a figure `f32` cannot hold reaches the node
/// whole.
#[test]
fn narrowed_is_the_f64_values_cast_elementwise_in_an_f32_graph() {
    let n = declick();
    let mut rig = Rig::<f32>::new(64, 1.0, 3.0);
    let _ = rig.run(64);
    rig.smooth.store(Hz::new(B));
    let out = rig.run(64 * 5);
    for j in 0..n - 1 {
        let want = 1.0 + (B - 1.0) * ((j + 1) as f64 / n as f64);
        assert_eq!(out[0][j], f64::from(want as f32), "frame {j}");
        assert_eq!(out[4][j], 1.0, "frame {j}: cast");
    }
}

/// **A retarget mid-ramp continues from the last frame delivered**: a base
/// moved at frame `m` starts a ramp of `n` frames from `values[m - 1]`.
#[test]
#[allow(clippy::needless_range_loop, reason = "several outputs at one frame")]
fn a_retarget_continues_from_the_last_frame_delivered() {
    let (n, m) = (declick(), 128);
    let mut rig = Rig::<f64>::new(64, 1.0, 3.0);
    let _ = rig.run(64);
    rig.smooth.store(Hz::new(2.0));
    let first = rig.run(m);
    let held = first[0][m - 1];
    assert!(held > 1.0 && held < 2.0, "mid-ramp");
    rig.smooth.store(Hz::new(5.0));
    let out = rig.run(n + 64 * 2);
    for j in 0..n - 1 {
        let want = held + (5.0 - held) * ((j + 1) as f64 / n as f64);
        assert!(
            (out[0][j] - want).abs() <= 8.0 * f64::EPSILON,
            "frame {j}: {} vs {want}",
            out[0][j]
        );
    }
    assert_eq!(out[0][n - 1], 5.0, "lands on the new base");
}

/// **A port that lands whole steps exactly**: its move is read on the block it
/// happens in, with no frames, while the smoothed port beside it ramps.
#[test]
fn a_whole_port_is_an_exact_step_with_no_frames() {
    let mut rig = Rig::<f64>::new(64, 1.0, 3.0);
    let _ = rig.run(128);
    rig.whole.store(Hz::new(B));
    rig.smooth.store(Hz::new(2.0));
    let out = rig.run(64);
    assert!(out[1].iter().all(|&x| x == B), "the whole port's new base");
    assert!(out[3].iter().all(|&x| x == 0.0), "Base: no frames");
    assert!(out[2].iter().all(|&x| x == 1.0), "the smoothed port ramps");
}

/// **Smoothing is not a drive**: while the base ramps the port reads
/// `Smoothing` (never `Frames`) and its `EffectiveCell` reads no lane; a
/// source's first event is `Frames` and shows the lane.
#[test]
fn smoothing_is_not_driven_and_the_effective_cell_shows_the_base() {
    let mut rig = Rig::<f64>::new(64, 1.0, 3.0);
    let cell = rig
        .ed
        .param_effective_cell(CUTOFF)
        .expect("a declared param");
    let _ = rig.run(128);
    rig.smooth.store(Hz::new(2.0));
    let out = rig.run(64);
    assert!(out[2].iter().all(|&x| x == 1.0), "Smoothing");
    assert_eq!(cell.current(), None, "the base, while smoothing");

    rig.ed.insert(
        SRC,
        "say",
        ForkByClone(Speak {
            at: 64 * 12 + 10,
            value: 7.0,
        }),
    );
    rig.ed
        .connect_param(CUTOFF, EventOutputPort::new(SRC, 0))
        .unwrap();
    rig.ed.commit().expect("connects");
    rig.exec.apply_pending();
    // The connect's declick, then the source silent: the base again.
    let _ = rig.run(64 * 6);
    assert_eq!(cell.current(), None, "a source that has not spoken");
    let out = rig.run(64 * 8);
    assert!(out[2].contains(&2.0), "the first event drives it");
    assert_eq!(*out[0].last().unwrap(), 7.0, "the source's value");
    assert_eq!(cell.current(), Some(7.0), "the lane's last frame");
}

/// **A unit's first block lands whole**, and so does a node that took the
/// key over in a re-prepare, and a fork: a base written after the unit was
/// built and before its first block reads in full from frame 0.
#[test]
fn the_first_block_after_a_prepare_and_a_fork_land_whole() {
    let mut rig = Rig::<f64>::new(64, 1.0, 3.0);
    rig.smooth.store(Hz::new(9.0));
    let out = rig.run(64);
    assert!(out[0].iter().all(|&x| x == 9.0), "the first block");
    assert!(out[2].iter().all(|&x| x == 0.0), "Base, no ramp");

    // A re-prepare restarts every unit: what the cell holds then is whole.
    let _ = rig.run(128);
    rig.smooth.store(Hz::new(5.0));
    rig.ed.reprepare(prepare(64)).expect("reprepares");
    rig.exec.apply_pending();
    rig.ed.collect();
    rig.exec.apply_pending();
    rig.ed.collect();
    let out = rig.run(64);
    assert!(out[0].iter().all(|&x| x == 5.0), "after the prepare");
    assert!(out[2].iter().all(|&x| x == 0.0), "Base, no ramp");

    // A fork starts steady at the base it is made at, while the live graph
    // goes on ramping to it.
    let _ = rig.run(128);
    rig.smooth.store(Hz::new(2.0));
    let (fork_ed, fork_exec) = rig
        .ed
        .fork(ForkTarget::MainOutput, prepare(64))
        .expect("forks");
    let mut fork = Rig::<f64> {
        ed: fork_ed,
        exec: fork_exec,
        smooth: rig.smooth.handle(),
        whole: rig.whole.handle(),
        block: 64,
    };
    fork.exec.apply_pending();
    let out = fork.run(64);
    assert!(out[0].iter().all(|&x| x == 2.0), "the fork, from frame 0");
    assert!(out[2].iter().all(|&x| x == 0.0), "Base in the fork");
    let live = rig.run(64);
    assert!(live[2].iter().all(|&x| x == 1.0), "the live graph ramps");
}

/// **A port leaving its source is carried by the declick alone**: the
/// disconnect crossfades the last driven value into the base along the
/// declick's law, with no ramp term beside it, and no `Smoothing` follows.
#[test]
#[allow(clippy::needless_range_loop, reason = "several outputs at one frame")]
fn a_source_removed_leaves_the_declick_crossfade_alone() {
    const BLOCK: usize = 64;
    let n = declick();
    let mut rig = Rig::<f64>::new(BLOCK, 1.0, 3.0);
    rig.ed
        .insert(SRC, "say", ForkByClone(Speak { at: 10, value: 7.0 }));
    rig.ed
        .connect_param(CUTOFF, EventOutputPort::new(SRC, 0))
        .unwrap();
    rig.ed.commit().expect("connects");
    rig.exec.apply_pending();
    let driven = rig.run(BLOCK * 8);
    assert_eq!(*driven[0].last().unwrap(), 7.0, "driven");

    // The base moves in the same commit that drops the source: the declick
    // fades to the new base, not to a ramp toward it.
    rig.smooth.store(Hz::new(2.0));
    assert!(
        rig.ed
            .disconnect_param(CUTOFF, EventOutputPort::new(SRC, 0))
    );
    rig.ed.commit().expect("disconnects");
    rig.exec.apply_pending();
    let out = rig.run(BLOCK * 8);
    for j in 0..n {
        let s = smootherstep(j, n);
        let want = 7.0 * (1.0 - s) + 2.0 * s;
        assert!(
            (out[0][j] - want).abs() < 1e-12,
            "frame {j}: {} vs {want}",
            out[0][j]
        );
    }
    assert!(out[0][n..].iter().all(|&x| x == 2.0), "the base, held");
    assert!(
        out[2][BLOCK * 4..].iter().all(|&x| x == 0.0),
        "no smoothing follows the declick"
    );
}

/// The declick law at frame `j` of `n`: the smootherstep `x³(6x² − 15x + 10)`
/// at `x = (j + 1) / (n + 1)`.
fn smootherstep(j: usize, n: usize) -> f64 {
    let x = (j + 1) as f64 / (n + 1) as f64;
    x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
}

/// **A crossfade block steps a key's params once, not twice**: a source
/// connected in the commit that replaces the knob fades in over the declick
/// along the smootherstep, `1 + 6·s(j)` at frame `j` of `n`, one block of
/// progress per block of the fade (both units hear the same frames, and the
/// equal-amplitude gains sum to one).
#[test]
#[allow(clippy::needless_range_loop, reason = "frame index feeds the law")]
fn a_crossfade_block_steps_its_params_once() {
    const BLOCK: usize = 64;
    let n = declick();
    let mut rig = Rig::<f64>::new(BLOCK, 1.0, 3.0);
    let _ = rig.run(BLOCK * 2);
    rig.ed.insert(
        SRC,
        "say",
        ForkByClone(Speak {
            at: 128,
            value: 7.0,
        }),
    );
    rig.ed
        .connect_param(CUTOFF, EventOutputPort::new(SRC, 0))
        .unwrap();
    rig.ed
        .replace(
            KNOB,
            Params(Knob {
                smooth: Param::new(Hz::new(1.0)),
                whole: Param::new(Hz::new(3.0)),
            }),
            tutti_graph::Fade::new(Samples::new(n * 2)),
        )
        .expect("replaces");
    rig.ed.commit().expect("commits");
    rig.exec.apply_pending();
    let out = rig.run(BLOCK * 8);
    for j in 0..n {
        let want = 1.0 + 6.0 * smootherstep(j, n);
        assert!(
            (out[0][j] - want).abs() < 1e-9,
            "frame {j}: {} vs {want}",
            out[0][j]
        );
    }
    assert!(
        out[0][n..].iter().all(|&x| (x - 7.0).abs() < 1e-9),
        "driven"
    );
}
