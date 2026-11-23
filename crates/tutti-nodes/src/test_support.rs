//! Rendering helpers shared by the width-generic nodes' unit tests.

use tutti_buffers::{InputOutput, Planes, Sample};
use tutti_graph::contract::{BlockRig, drive};
use tutti_graph::{IntoNode, Node};
use tutti_units::{Db, Real, SampleRate, Samples};

pub(crate) use tutti_fixtures::noise;

use crate::kernel::{ControlSignals, Kernel};
use crate::params::ControlSet;

/// The rate the node helpers below prepare and drive nodes at.
pub(crate) const RATE: SampleRate = SampleRate::SR_48K;

/// The amplitude of `db` as the `f32` a node's samples are: narrowed once.
pub(crate) fn sample_gain(db: Db) -> f32 {
    db.to_amplitude().get() as f32
}

/// The amplitude of a fed `f32` control signal value in dB, as a per-frame read computes it: `10^(db / 20)`
/// in `f32` through `libm`.
pub(crate) fn fed_sample_gain(db: f32) -> f32 {
    libm::powf(10.0, db / 20.0)
}

/// A node prepared at [`RATE`] for blocks of up to 1024 frames.
pub(crate) fn prepared<N: Node>(node: N) -> N {
    tutti_graph::contract::prepared(node, RATE, 1024)
}

/// `node` alone in an `f64` graph at `rate`: one block of `inputs` (one slice per input
/// port, all the block's length `frames`) through the graph's executor, and what each output
/// port holds.
pub(crate) fn render_f64<N: IntoNode<f64>>(
    node: N,
    rate: SampleRate,
    inputs: &[&[f64]],
    frames: usize,
) -> Vec<Vec<f64>> {
    let (mut rig, _controls) = BlockRig::<f64>::with_format(node, rate, frames);
    {
        let mut ins = rig.inputs_mut();
        for (c, x) in inputs.iter().enumerate() {
            ins.plane_mut(c).samples_mut().copy_from_slice(x);
        }
    }
    rig.block();
    let out = rig.output();
    (0..out.channels())
        .map(|c| out.plane(c).samples().to_vec())
        .collect()
}

/// A fresh node of `make` is linear in its input: the render of `a x + b y` is `a` times that
/// of `x` plus `b` times that of `y`, over `frames` frames of `channels` input channels, to
/// `2^-40` of the peak, which an `f32` in the stream or the state breaks at about `2^-24`. The
/// peak is far above that, so the node sounds.
#[track_caller]
pub(crate) fn assert_linear<N: IntoNode<f64>>(
    make: impl Fn() -> N,
    rate: SampleRate,
    channels: usize,
    frames: usize,
) {
    let x = tutti_fixtures::signal::wide_signal(frames);
    let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
    let (a, b) = (1.0 / 3.0, -core::f64::consts::PI / 7.0);
    let mix: Vec<f64> = x.iter().zip(&y).map(|(x, y)| a * x + b * y).collect();
    let run = |signal: &[f64]| render_f64(make(), rate, &vec![signal; channels], frames);
    let (rx, ry, rm) = (run(&x), run(&y), run(&mix));
    for c in 0..rm.len() {
        let peak = rm[c].iter().fold(0.0f64, |p, v| p.max(v.abs()));
        assert!(peak > 1e-4, "channel {c} sounds");
        for n in 0..frames {
            let want = a * rx[c][n] + b * ry[c][n];
            assert!(
                (rm[c][n] - want).abs() <= peak * 2.0f64.powi(-40),
                "channel {c}, frame {n}: {} against {want}",
                rm[c][n]
            );
        }
    }
}

/// The tolerance of an `f64` result that holds its figure to `2^-45`: far above `f64`'s
/// rounding in a short render and far below the `2^-25` a narrowing to `f32` leaves.
pub(crate) const F64_TOLERANCE: f64 = 1.0 / (1u64 << 45) as f64;

/// One `process` call of a node over `inputs` (one slice per input
/// port), its params at their bases.
pub(crate) fn drive_block(node: &mut dyn Node, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
    drive(node, RATE, Planes::new(inputs).unwrap(), &[])
}

/// Returns a [`prepared`] node of `make`, driven over `before` in one block, then
/// `change`d, then driven over `after` in blocks of `block` frames, with its render of
/// `after`.
pub(crate) fn render_change<N: Node>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    before: &[&[f32]],
    after: &[&[f32]],
    block: usize,
) -> (N, Vec<Vec<f32>>) {
    let mut node = prepared(make());
    drive_block(&mut node, before);
    change(&node);
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut at = 0;
    while at < after[0].len() {
        let end = (at + block).min(after[0].len());
        let chunk: Vec<&[f32]> = after.iter().map(|s| &s[at..end]).collect();
        let o = drive_block(&mut node, &chunk);
        out.resize(o.len(), Vec::new());
        for (c, v) in o.into_iter().enumerate() {
            out[c].extend(v);
        }
        at = end;
    }
    (node, out)
}

/// Asserts that a cell moved between blocks ramps on the stream clock, so a write
/// renders the same whatever the host's blocks: the render of `after` is the same, bit
/// for bit, at blocks of 1, 17, 64 and 256 frames, and it is not the render with the cell
/// left alone. Returns the node rendered at blocks of 64.
#[track_caller]
pub(crate) fn assert_cell_ramps<N: Node>(
    make: impl Fn() -> N,
    change: impl Fn(&N),
    before: &[&[f32]],
    after: &[&[f32]],
    what: &str,
) -> N {
    let (node, at64) = render_change(&make, &change, before, after, 64);
    for block in [1, 17, 256] {
        let (_, out) = render_change(&make, &change, before, after, block);
        assert!(
            out.iter()
                .flatten()
                .zip(at64.iter().flatten())
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{what}: the ramp at blocks of {block} is not the ramp at blocks of 64"
        );
    }
    let (_, held) = render_change(&make, |_: &N| {}, before, after, 64);
    assert!(
        held != at64,
        "{what}: the change is inaudible, so this run cannot see a ramp"
    );
    node
}

/// The rate the per-frame unit tests run their nodes at.
pub(crate) const RATE_44K: SampleRate = SampleRate::SR_44K1;

/// A node prepared at `rate` for blocks of up to 1024 frames.
pub(crate) fn prepared_at<N: Node>(node: N, rate: SampleRate) -> N {
    tutti_graph::contract::prepared(node, rate, 1024)
}

/// One frame of a node — a block of one: `input` holds one sample per audio
/// input, and `out` receives one per output.
pub(crate) fn tick(node: &mut dyn Node, input: &[f32], out: &mut [f32]) {
    tick_fed(node, input, &[], out);
}

/// [`tick`] with declared param `k` fed `params[k]` for the frame (`None`
/// reads its base).
pub(crate) fn tick_fed(
    node: &mut dyn Node,
    input: &[f32],
    params: &[Option<f32>],
    out: &mut [f32],
) {
    let ins: Vec<&[f32]> = input.iter().map(std::slice::from_ref).collect();
    let ps: Vec<Option<&[f32]>> = params
        .iter()
        .map(|p| p.as_ref().map(std::slice::from_ref))
        .collect();
    for (o, c) in out
        .iter_mut()
        .zip(drive(node, RATE, Planes::new(&ins).unwrap(), &ps))
    {
        *o = c[0];
    }
}

/// `len` samples of a unit sine at `hz` in `f64`, starting at phase 0.
pub(crate) fn wide_sine(hz: f64, rate: f64, len: usize) -> Vec<f64> {
    (0..len)
        .map(|i| libm::sin(2.0 * core::f64::consts::PI * hz * i as f64 / rate))
        .collect()
}

/// `len` samples of a unit sine at `hz`, starting at phase 0.
pub(crate) fn sine(hz: f64, rate: f64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| libm::sin(2.0 * core::f64::consts::PI * hz * i as f64 / rate) as f32)
        .collect()
}

/// A graph node over whole-signal `inputs` (one `Vec` per input), rendered in
/// `chunk`-frame blocks through [`drive_block`]; one `Vec` per output. The
/// node must be [`prepared`] for blocks of `chunk`.
pub(crate) fn render_chunked(
    node: &mut dyn Node,
    inputs: &[Vec<f32>],
    chunk: usize,
) -> Vec<Vec<f32>> {
    let len = inputs[0].len();
    let mut out: Vec<Vec<f32>> = Vec::new();
    for start in (0..len).step_by(chunk) {
        let end = (start + chunk).min(len);
        let block: Vec<&[f32]> = inputs.iter().map(|c| &c[start..end]).collect();
        let result = drive_block(node, &block);
        out.resize(result.len(), Vec::with_capacity(len));
        for (o, r) in out.iter_mut().zip(result) {
            o.extend(r);
        }
    }
    out
}

/// The mean of `x`.
pub(crate) fn mean<S: Sample>(x: &[S]) -> f64 {
    x.iter().map(|&v| v.to_f64()).sum::<f64>() / x.len() as f64
}

/// Renders `input` (one `Vec` per channel) through `k` in one call.
pub(crate) fn render_kernel<K: Kernel>(k: &mut K, input: &[Vec<K::Float>]) -> Vec<Vec<K::Float>> {
    let ins: Vec<&[K::Float]> = input.iter().map(Vec::as_slice).collect();
    let width = usize::from(k.shape().audio_out().count());
    let mut out = vec![vec![K::Float::ZERO; input[0].len()]; width];
    let mut outs: Vec<&mut [K::Float]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    let frames = Samples::new(input[0].len());
    k.render(
        &mut InputOutput::from_slices(frames, &ins, &mut outs).unwrap(),
        ControlSignals::NONE,
    )
    .unwrap();
    out
}

/// What the driver does to a kernel that cuts at every grid point ([`Kernel::CUT_ALWAYS`]) for
/// the frames of `input`: a retarget at every grid point and at every frame of `moves` (a
/// control that moved), each at the controls in force there (`moves` is `(frame, controls)`
/// in order, the first at frame 0), the frames between rendered together.
pub(crate) fn drive_kernel<K: Kernel>(
    k: &mut K,
    moves: &[(usize, <K::Set as ControlSet>::Controls)],
    input: &[Vec<K::Float>],
) -> Vec<Vec<K::Float>> {
    let frames = input[0].len();
    let mut stops: Vec<usize> = (0..frames).step_by(K::GRID).collect();
    stops.extend(moves.iter().map(|m| m.0).filter(|&f| f < frames));
    stops.sort_unstable();
    stops.dedup();
    stops.push(frames);
    let mut out = vec![Vec::new(); input.len()];
    for w in stops.windows(2) {
        let at = moves.iter().rev().find(|m| m.0 <= w[0]).unwrap().1;
        k.retarget(&at);
        let piece: Vec<Vec<K::Float>> = input.iter().map(|c| c[w[0]..w[1]].to_vec()).collect();
        for (o, y) in out.iter_mut().zip(render_kernel(k, &piece)) {
            o.extend(y);
        }
    }
    out
}
