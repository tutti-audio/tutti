//! A `KernelNode` over a kernel with a processing latency and a finite tail the test can
//! make lie: what a kernel with no latency and a tail longer than a block cannot show. The
//! graph's mix aligned to the kernel's latency, the cut at `tail + latency` inside a block
//! and a block-aligned park. Every figure is the definition of a delay.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_graph::contract::{Driven, assert_honest_tail, drive_input, prepared};
use tutti_graph::{Buses, Node, Shape};
use tutti_graph::{Env, PlayClock, Prepare, Solo, Status, TransportChanges, TransportPosition};
use tutti_nodes::{ControlLanes, ControlSet, Kernel, KernelNode};
use tutti_types::{BusTopology, Frame, MaxBlock, Mix, SampleRate, Samples, Tail, UnitAddr};

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

/// An echo has no cells.
#[derive(Clone, Debug)]
struct NoParams;

impl ControlSet for NoParams {
    type Controls = ();

    fn addrs(&self) -> &'static [UnitAddr] {
        &[]
    }

    fn defaults(&self) {}

    fn flatten(&self, _: &(), _: &mut [f32]) {}

    fn assemble(&self, _: &[f32]) {}
}

impl Kernel for Echo {
    type Set = NoParams;

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

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        mut output: PlanesMut<'_, '_, S>,
    ) {
        let n = self.line.len();
        let (first, second) = (self.claimed + self.latency, self.actual + self.latency);
        let x = input.plane(0);
        let mut y = output.plane_mut(0);
        for (x, y) in x.samples().iter().zip(y.samples_mut()) {
            self.line[self.at] = x.to_f64();
            let back = |d: usize| self.line[(self.at + n - d) % n];
            let wet = back(first) + if second != first { back(second) } else { 0.0 };
            *y = S::from_f64(wet);
            self.at = (self.at + 1) % n;
        }
    }

    fn latency(&self) -> Samples {
        Samples(self.latency)
    }

    fn tail(&self) -> Tail {
        Tail::Finite(Samples(self.claimed))
    }
}

fn kernel_node(echo: Echo) -> KernelNode<Echo> {
    KernelNode::with_kernel(echo, NoParams, ())
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
            block_len: Samples(input.len()),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(&mut self.node, &env, &[input], &[], &[])
    }
}

fn impulse_block() -> Vec<f32> {
    tutti_fixtures::signal::impulse(Samples(BLOCK))
}

/// The kernel's latency and tail are declared apart (a tail excludes latency), and the
/// honesty check holds for an honest echo.
#[test]
fn latency_and_tail_are_declared_apart() {
    let mut n = node(Echo::new(100, 100, 7));
    let shape = n.shape();
    assert_eq!(shape.latency.samples(), Samples(7));
    assert_eq!(shape.tail.samples().expect("finite").get(), 100);
    assert_honest_tail(&mut n, SR);
}

/// A kernel that rings past its tail is cut at `tail + latency` after the last input
/// frame, to the frame, inside the block that holds it: input on the first 20 frames, and
/// the last frame to sound is the last input frame plus the reported tail plus the latency.
#[test]
fn a_cut_inside_a_block_is_at_tail_plus_latency() {
    let (claimed, latency) = (10, 7);
    let echo = Echo::new(claimed, claimed + 3, latency);
    let mut rig = Rig::new(echo.clone());
    let mut x = vec![0.0f32; BLOCK];
    for (i, s) in x.iter_mut().enumerate().take(20) {
        *s = 1.0 + i as f32;
    }
    let d = rig.block(&x);
    let Some(Tail::Finite(ring)) = d.reported_tail else {
        panic!("finite")
    };
    let last = d.audio[0].iter().rposition(|&s| s != 0.0);
    // The second echo, 3 frames past the claim, would sound for 3 frames more; the cut is
    // at 19 + ring + 7, inside the block.
    assert_eq!(last, Some(19 + ring.get() + latency));
    assert!(19 + ring.get() + latency < BLOCK);

    // The cut reset the kernel: what it held when cut (the second echo, still in flight)
    // never sounds. Faint input keeps the node running; a node that never heard the noise
    // answers it identically.
    let mut fresh = Rig::new(echo);
    for _ in 0..4 {
        let whisper = vec![1e-20; BLOCK];
        assert_eq!(rig.block(&whisper).audio, fresh.block(&whisper).audio);
    }
}

/// A cut that falls on a block edge parks the node in the next block, and the park resets
/// the kernel: the node that woke answers as a fresh one. The echo rings far past its
/// claim, so its line still holds the impulse when the cut lands, and a node that parked
/// without resetting would play it back.
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
    // Impulse at frame 0: the cut is at 0 + ring + latency + 1 = 64 frames.
    let echo = Echo::new(20, 300, BLOCK - 1 - ring);
    let mut rig = Rig::new(echo.clone());
    let first = rig.block(&impulse_block());
    assert_eq!(first.status, Some(Status::Modified), "sound to the edge");
    let second = rig.block(&[0.0; BLOCK]);
    assert_eq!(
        second.status,
        Some(Status::Idle),
        "parked in the next block"
    );
    // A node that never heard the impulse. The same silence then an impulse to both: the
    // woken node must not play back the one the cut left in its line.
    let mut fresh = Rig::new(echo);
    let mut woken = Vec::new();
    let mut new = Vec::new();
    for b in 0..12 {
        // Silence, then input that is faint but never exactly silent (so the node stays
        // awake and the kernel runs on): an impulse, then a whisper.
        let x = match b {
            0..=2 => vec![0.0; BLOCK],
            3 => impulse_block(),
            _ => vec![1e-20; BLOCK],
        };
        woken.extend(rig.block(&x).audio.remove(0));
        new.extend(fresh.block(&x).audio.remove(0));
    }
    assert_eq!(woken, new);
}

/// The graph's mix aligns the dry input to the kernel's declared latency, never to the tail
/// or the echo: at a mix of 0.5 an impulse comes out half-strength at `latency` (the input,
/// delayed to line up) and half-strength at `latency + echo` (the wet). A block of silence
/// first, so the mix's ramp from the unit's `WET` start is over before the impulse.
#[test]
fn the_graphs_mix_is_aligned_to_the_kernels_latency() {
    let (claimed, latency) = (25, 7);
    let mut solo = Solo::new(
        kernel_node(Echo::new(claimed, claimed, latency)),
        Prepare::new(SR, MaxBlock::of(Samples(BLOCK))),
    );
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mix(key, Mix(0.5))
        .expect("a mix");
    let mut x = vec![0.0; BLOCK];
    x.extend(impulse_block());
    let out = solo.render_input(&[&x]).remove(0);
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
