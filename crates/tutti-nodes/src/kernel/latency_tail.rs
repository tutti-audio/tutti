//! A `KernelNode` over a kernel with a processing latency and a finite tail the test can
//! make lie: what a kernel with no latency and a tail longer than a block cannot show. The
//! graph's mix aligned to the kernel's latency, the cut at `tail + latency` inside a block
//! and a block-aligned park. Every figure is the definition of a delay.

use crate::{ControlSignals, Kernel, KernelNode, NoControls};
use tutti_buffers::{BusTopology, MaxBlock};
use tutti_buffers::{Planes, PlanesMut};
use tutti_graph::contract::{Driven, assert_declared_tail, drive_input, prepared};
use tutti_graph::{Buses, Node, ParamNode, Shape};
use tutti_graph::{
    Env, PlayClock, Prepare, Standalone, Status, TransportChanges, TransportPosition,
};
use tutti_units::{Frame, Mix, SampleRate, Samples, Tail, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

/// A mono echo `claimed` frames after the input that declares a tail of `claimed`, behind a
/// processing latency of `latency` frames. When `actual` differs it plays a second echo
/// `actual` frames after the input, past the claim: an honest echo has `actual == claimed`,
/// a lying one rings past its tail, which the node must cut.
#[derive(Clone, Debug)]
struct Echo {
    claimed: usize,
    actual: usize,
    latency: usize,
    line: Vec<f64>,
    at: usize,
}

impl Echo {
    fn new(claimed: usize, actual: usize, latency: usize) -> Self {
        Self {
            claimed,
            actual,
            latency,
            line: Vec::new(),
            at: 0,
        }
    }
}

impl Kernel for Echo {
    type Float = f32;
    type Set = NoControls;

    const GRID: usize = 16;

    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(BusTopology::MONO),
            Buses::main(BusTopology::MONO),
        )
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {
        self.line = vec![0.0; self.claimed.max(self.actual) + self.latency + 1];
        self.at = 0;
    }

    fn reset(&mut self) {
        self.line.fill(0.0);
    }

    fn retarget(&mut self, _: &()) {}

    fn render(
        &mut self,
        input: Planes<'_, f32>,
        _: ControlSignals<'_, f32>,
        mut output: PlanesMut<'_, '_, f32>,
    ) {
        let n = self.line.len();
        let (first, second) = (self.claimed + self.latency, self.actual + self.latency);
        let x = input.plane(0);
        let mut y = output.plane_mut(0);
        for (x, y) in x.samples().iter().zip(y.samples_mut()) {
            self.line[self.at] = f64::from(*x);
            let back = |d: usize| self.line[(self.at + n - d) % n];
            let wet = back(first) + if second != first { back(second) } else { 0.0 };
            *y = wet as f32;
            self.at = (self.at + 1) % n;
        }
    }

    fn latency(&self) -> Samples {
        Samples::new(self.latency)
    }

    fn tail(&self) -> Tail {
        Tail::Finite(Samples::new(self.claimed))
    }
}

fn kernel_node(echo: Echo) -> KernelNode<Echo> {
    KernelNode::with_kernel(echo, NoControls, ())
}

fn node(echo: Echo) -> KernelNode<Echo> {
    prepared(kernel_node(echo), SR, BLOCK)
}

struct Rig {
    node: KernelNode<Echo>,
    frame: u64,
}

impl Rig {
    fn new(echo: Echo) -> Self {
        Self {
            node: node(echo),
            frame: 0,
        }
    }

    fn block(&mut self, input: &[f32]) -> Driven {
        let env = Env {
            frame: Frame::new(self.frame),
            sample_rate: SR,
            block_len: Samples::new(input.len()),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(
            &mut self.node,
            &env,
            Planes::new(&[input]).unwrap(),
            &[],
            &[],
        )
    }
}

fn impulse_block() -> Vec<f32> {
    tutti_fixtures::signal::impulse(Samples::new(BLOCK))
}

/// The kernel's latency and tail are declared apart (a tail excludes latency), and the
/// honesty check holds for an honest echo.
#[test]
fn latency_and_tail_are_declared_apart() {
    let mut n = node(Echo::new(100, 100, 7));
    let shape = n.shape();
    assert_eq!(shape.latency.samples(), Samples::new(7));
    assert_eq!(shape.tail.samples().expect("finite").get(), 100);
    assert_declared_tail(&mut n, SR);
}

/// A kernel with no controls declares no port and addresses no cell: its dry/wet is the
/// graph's.
#[test]
fn a_kernel_without_controls_offers_no_ports() {
    let n = node(Echo::new(4, 4, 0));
    assert_eq!(n.shape().params.len(), 0);
    assert_eq!(n.param_set().params().count(), 0);
    assert!(n.param_set().get(UnitParam::Wet).is_none());
}

/// What an echo with its first echo `first` frames after the input and its second `second`
/// frames after it plays for `x` from a silent line: `x` delayed by each, summed. The
/// definition of the delay, not a render.
fn echoes(x: &[f32], first: usize, second: usize) -> Vec<f32> {
    let at = |g: usize, d: usize| g.checked_sub(d).map_or(0.0, |i| f64::from(x[i]));
    (0..x.len())
        .map(|g| (at(g, first) + at(g, second)) as f32)
        .collect()
}

/// A kernel that rings past its tail is cut at `tail + latency` after the last input
/// frame, to the frame, inside the block that holds it: input on the first 20 frames, and
/// the last frame to sound is frame 19 + 10 + 7 = 36.
///
/// A node that did not cut would sound the second echo of the last three input frames at
/// frames 37 to 39, so the last-frame pin fails for it. The kernel keeps running over the
/// rest of the block (its line is flushed by the silence), so what follows the cut is the
/// echoes of the later input alone: the blocks after it carry only an input of 1e-20 (not
/// exactly 0.0, the level at which the node hears input), and the output is exactly those
/// echoes from a silent line. That the park itself resets the kernel is held by
/// `a_block_aligned_cut_parks_and_resets`.
#[test]
fn a_cut_inside_a_block_is_at_tail_plus_latency() {
    let (claimed, latency) = (10, 7);
    let mut rig = Rig::new(Echo::new(claimed, claimed + 3, latency));
    let mut x = vec![0.0f32; BLOCK];
    for (i, s) in x.iter_mut().enumerate().take(20) {
        *s = 1.0 + i as f32;
    }
    let d = rig.block(&x);
    let Some(Tail::Finite(ring)) = d.reported_tail else {
        panic!("finite")
    };
    // A kernel with a finite tail and the default `tail_of` reports its ceiling whole.
    assert_eq!(ring.get(), claimed);
    let last = d.audio[0].iter().rposition(|&s| s != 0.0);
    assert_eq!(last, Some(19 + claimed + latency), "frame 36");
    assert!(19 + claimed + latency < BLOCK);

    let whisper = vec![1e-20f32; 4 * BLOCK];
    let mut heard = Vec::new();
    for block in whisper.chunks(BLOCK) {
        heard.extend(rig.block(block).audio.remove(0));
    }
    assert_eq!(
        heard,
        echoes(&whisper, claimed + latency, claimed + 3 + latency)
    );
}

/// A cut that falls on a block edge parks the node in the next block, and the park resets
/// the kernel. The echo rings far past its claim (a second echo 300 frames after the
/// input), so its line still holds the impulse when the cut lands. The node then hears
/// silence, an impulse and an input of 1e-20 (not exactly 0.0, so it stays awake and runs
/// the kernel), and its output is exactly the echoes of that input from a silent line: the
/// impulse's two echoes at 63 and 343 frames and the whisper's. A node that parked without
/// resetting would replay the first impulse's second echo and differ.
#[test]
fn a_block_aligned_cut_parks_and_resets() {
    // The ring the node reports for an echo of 20 frames, to land the cut at 64.
    let ring = match Rig::new(Echo::new(20, 20, 0))
        .block(&impulse_block())
        .reported_tail
    {
        Some(Tail::Finite(n)) => n.get(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ring, 20);
    // Impulse at frame 0: the cut is at 0 + ring + latency + 1 = 64 frames.
    let latency = BLOCK - 1 - ring;
    let mut rig = Rig::new(Echo::new(20, 300, latency));
    let first = rig.block(&impulse_block());
    assert_eq!(first.status, Some(Status::Modified), "sound to the edge");
    assert_eq!(
        first.audio[0].iter().rposition(|&s| s != 0.0),
        Some(20 + latency),
        "the first echo is the last frame of the block"
    );
    let second = rig.block(&[0.0; BLOCK]);
    assert_eq!(
        second.status,
        Some(Status::Idle),
        "parked in the next block"
    );
    let mut x = vec![0.0f32; 3 * BLOCK];
    x.extend(impulse_block());
    x.extend(vec![1e-20f32; 8 * BLOCK]);
    let mut woken = Vec::new();
    for block in x.chunks(BLOCK) {
        woken.extend(rig.block(block).audio.remove(0));
    }
    assert_eq!(woken, echoes(&x, 20 + latency, 300 + latency));
}

/// The graph's mix aligns the dry input to the kernel's declared latency, never to the tail
/// or the echo: at a mix of 0.5 an impulse comes out half-strength at `latency` (the input,
/// delayed to line up) and half-strength at `latency + echo` (the wet). A block of silence
/// first, so the mix's ramp from the unit's `WET` start is over before the impulse.
#[test]
fn the_graphs_mix_is_aligned_to_the_kernels_latency() {
    let (claimed, latency) = (25, 7);
    let mut solo = Standalone::new(
        kernel_node(Echo::new(claimed, claimed, latency)),
        Prepare::new(SR, MaxBlock::of(Samples::new(BLOCK))),
    );
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mix(key, Mix::new(0.5))
        .expect("a mix");
    let mut x = vec![0.0; BLOCK];
    x.extend(impulse_block());
    let out = solo.render_input(Planes::new(&[&x]).unwrap()).remove(0);
    let spikes: Vec<(usize, f32)> = out
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s != 0.0)
        .map(|(i, &s)| (i - BLOCK, s))
        .collect();
    assert_eq!(
        spikes,
        vec![(latency, 0.5), (latency + claimed, 0.5)],
        "the dry at the latency, the wet an echo later"
    );
}
