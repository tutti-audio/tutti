//! The convolver node's tail, parking and resume.
//!
//! The node's tail is the longest path's: its output is exact zeros past the latency and the
//! tail, and it answers `Status::Silent` once its input has been silent that long, which the
//! graph needs to park it (the executor skips only a node whose last output it was told is
//! silent). A gap of silence that long also restarts the engine at the next input, so a
//! render does not depend on the blocks it is cut into or on whether the node was parked.
//!
//! - `assert_parks_at_the_tail` (three layouts): past the tail every frame is exactly zero,
//!   the graph's render equals the node's own, and the node is called for the blocks that
//!   ring and then skipped.
//! - The status tests: the node returns `Silent` from the first block wholly past the tail
//!   and `Modified` before; a node that never said `Silent` is called every block.
//! - The resume tests: a burst after a parked gap is a fresh node's, bit for bit, and the
//!   whole render is the same bits as a node called on every block, at any block size, for
//!   per-channel, folded and true stereo nodes and a driven gain.
//! - The boundary test: a gap one frame short of the tail does not restart the engine; one
//!   of exactly the tail does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tutti_buffers::{
    AmbisonicFormat, AmbisonicOrder, Bed, BusTopology, ChannelLayout, ChannelTopology, InputOutput,
    MaxBlock, Mono, Planes, PlanesMut,
};

use tutti_dsp::MatrixConvolver;
use tutti_graph::contract::{drive, prepared};
use tutti_graph::{Cx, GraphBuilder, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_nodes::{ConvolverNode, FirConvolverConfig};
use tutti_rt::Param;
use tutti_units::{Amplitude, SampleRate, Samples};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;
const FRAMES: usize = 64 * 40;

/// A node that records the `Status` of each `process` call and is otherwise the node inside;
/// with `hide_silence` it says `Modified` where the node said `Silent`.
struct Recording<N> {
    inner: N,
    log: Arc<Mutex<Vec<Status>>>,
    hide_silence: Arc<AtomicBool>,
}

impl<N: Node> Recording<N> {
    fn new(inner: N) -> (Self, Arc<Mutex<Vec<Status>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let rec = Self {
            inner,
            log: Arc::clone(&log),
            hide_silence: Arc::new(AtomicBool::new(false)),
        };
        (rec, log)
    }
}

impl<N: Node> Node for Recording<N> {
    fn shape(&self) -> Shape {
        self.inner.shape()
    }
    fn param_base(&self, k: usize) -> Option<f64> {
        self.inner.param_base(k)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.inner.prepare(p);
    }
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let status = self.inner.process(cx, io);
        self.log.lock().expect("the log").push(status);
        if status == Status::Silent && self.hide_silence.load(Ordering::Relaxed) {
            Status::Modified
        } else {
            status
        }
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

fn calls(log: &Mutex<Vec<Status>>) -> usize {
    log.lock().expect("the log").len()
}

/// A source of `width` channels that is a unit impulse at its first frame and then silent
/// for good, saying so (`Status::Silent`), which the graph parks the node behind it on.
struct Impulse {
    width: ChannelLayout,
    at: usize,
}

impl Node for Impulse {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.width)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut loud = false;
        for c in 0..usize::from(self.width.count()) {
            for (i, o) in io.output(c).iter_mut().enumerate() {
                *o = if self.at + i == 0 { 1.0 } else { 0.0 };
                loud |= *o != 0.0;
            }
        }
        self.at += io.frames();
        if loud {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

/// A response of one tap: `gain` at `delay`.
fn tap(delay: usize, gain: f32) -> Vec<f32> {
    let mut ir = vec![0.0; delay + 1];
    ir[delay] = gain;
    ir
}

/// `node`, rendered alone through `process` (prepared, called on every block) and in a graph
/// behind an impulse source (where the graph skips it once it says it is done), over an
/// impulse at frame 0 on every input, for the declared `latency` and `tail`:
///
/// - past `latency + tail` every frame is exactly zero, in both renders;
/// - the graph's render is the node's bit for bit, so the graph never skipped it while a path
///   still rings;
/// - the node was called for the blocks up to the end of the ring (and so skipped after it).
fn assert_parks_at_the_tail(node: ConvolverNode<f32>, latency: usize, tail: usize) {
    let shape = node.shape();
    let (ins, outs) = (shape.audio_in(), shape.audio_out());
    let n_in = usize::from(ins.count());
    let mut impulse = vec![0.0f32; FRAMES];
    impulse[0] = 1.0;
    let inputs: Vec<&[f32]> = (0..n_in).map(|_| &impulse[..]).collect();

    let mut direct = prepared(node.clone(), SR, BLOCK);
    let mut want: Vec<Vec<f32>> = vec![Vec::new(); usize::from(outs.count())];
    for at in (0..FRAMES).step_by(BLOCK) {
        let block: Vec<&[f32]> = inputs.iter().map(|c| &c[at..at + BLOCK]).collect();
        for (w, b) in want
            .iter_mut()
            .zip(drive(&mut direct, SR, Planes::new(&block).unwrap(), &[]))
        {
            w.extend(b);
        }
    }
    let rings_until = latency + tail;
    for (c, ch) in want.iter().enumerate() {
        assert!(
            ch[rings_until + 1..].iter().all(|&v| v == 0.0),
            "output {c} is not silent past the declared tail (frame {rings_until})"
        );
    }

    let (recording, log) = Recording::new(node);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, outs);
    let source = g.add(Unforkable(Impulse { width: ins, at: 0 }));
    let key = g.add(Unforkable(recording));
    for c in 0..n_in {
        g.connect(source, c, key, c);
    }
    for c in 0..usize::from(outs.count()) {
        g.connect_output(key, c, c);
    }
    let mut r = g
        .renderer(Prepare::new(SR, MaxBlock::of(Samples::new(BLOCK))))
        .expect("a two-node graph builds");
    let got = r.render(FRAMES);
    for (c, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!(
            g.iter().zip(w).all(|(a, b)| a.to_bits() == b.to_bits()),
            "output {c}: the graph's render is not the node's, so the node was skipped while it rang"
        );
    }
    // The block holding the last ringing frame is the last the node is called for.
    let needed = rings_until / BLOCK + 1;
    let called = calls(&log);
    assert!(
        called >= needed,
        "called {called} times, but the node rings through block {needed}"
    );
    assert!(
        called < FRAMES / BLOCK,
        "called {called} times of {}: the graph never skipped it",
        FRAMES / BLOCK
    );
}

/// True stereo with taps at 40, 90, 10 and 200 frames, 64 latency: rings to frame 264.
#[test]
fn a_true_stereo_node_parks_after_its_longest_path() {
    let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
    let node = true_stereo_node::<f32, _>(
        Mono::new(&irs[0]),
        Mono::new(&irs[1]),
        Mono::new(&irs[2]),
        Mono::new(&irs[3]),
        Samples::new(64),
    );
    assert_parks_at_the_tail(node, 64, 200);
}

/// A linear-phase filter of N taps with 64 latency declares `64 + (N - 1) / 2` latency and a
/// tail of `(N - 1) / 2`, so it rings to frame `64 + N - 1`, the last of its response.
#[test]
fn a_linear_phase_node_parks_after_its_response() {
    use tutti_dsp::{LinearPhaseDesign, Tilt};
    use tutti_units::{Db, Hz};

    let tilt = Tilt::new(Db::new(6.0))
        .design(Hz::new(2_000.0), SR)
        .unwrap();
    let fir = LinearPhaseDesign::new(Hz::new(2_000.0))
        .design(SR, &[tilt])
        .unwrap();
    let delay = fir.delay().get();
    let node = ConvolverNode::<f32>::try_from(
        FirConvolverConfig::new(BusTopology::MONO, &fir).block_size(Samples::new(64)),
    )
    .unwrap();
    assert_parks_at_the_tail(node, 64 + delay, delay);
}

/// First order in and out with sixteen taps at distinct delays, the longest (300 frames)
/// only on the last path: the other paths ring out long before it.
#[test]
fn an_ambisonic_node_parks_after_its_longest_path() {
    let foa = AmbisonicFormat::ambix(AmbisonicOrder::of(1));
    let irs: Vec<Vec<f32>> = (0..16)
        .map(|p| {
            tap(
                if p == 15 { 300 } else { 2 + 3 * p },
                0.0625 * (p as f32 + 1.0),
            )
        })
        .collect();
    let views: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
    let node = ambisonic_node::<f32, _>(foa, ChannelTopology::from(foa), &views, Samples::new(64))
        .unwrap();
    assert_parks_at_the_tail(node, 64, 300);
}

/// First order to stereo: four inputs, two outputs, a long tap only on one path.
#[test]
fn an_ambisonic_to_stereo_node_parks_after_its_longest_path() {
    let foa = AmbisonicFormat::ambix(AmbisonicOrder::of(1));
    let irs: Vec<Vec<f32>> = (0..8)
        .map(|p| {
            tap(
                if p == 5 { 250 } else { 5 + 7 * p },
                0.125 * (p as f32 + 1.0),
            )
        })
        .collect();
    let views: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
    let node = ambisonic_node::<f32, _>(
        foa,
        ChannelTopology::from(Bed::Stereo),
        &views,
        Samples::new(64),
    )
    .unwrap();
    assert_parks_at_the_tail(node, 64, 250);
}

/// The node's own `Status::Silent`: true stereo with latency 64 and tail 200 (taps at 40, 90,
/// 10 and 200), driven by hand on an impulse at frame 0 and silence after, blocks of 64. The
/// frames since the last non-zero input before block `k` are `64 k - 1`, and a block is wholly
/// past the tail once they reach latency + tail = 264: block 5 (319 >= 264; block 4 has 255).
/// The node says `Modified` for blocks 0 to 4 and `Silent` from block 5.
#[test]
fn the_node_says_silent_from_the_first_block_past_its_tail() {
    let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
    let node = true_stereo_node::<f32, _>(
        Mono::new(&irs[0]),
        Mono::new(&irs[1]),
        Mono::new(&irs[2]),
        Mono::new(&irs[3]),
        Samples::new(64),
    );
    let (recording, log) = Recording::new(node);
    let mut node = prepared(recording, SR, BLOCK);
    for k in 0..12 {
        let mut x = vec![0.0f32; BLOCK];
        if k == 0 {
            x[0] = 1.0;
        }
        drive(&mut node, SR, Planes::new(&[&x, &x]).unwrap(), &[]);
    }
    let log = log.lock().expect("the log");
    for (k, status) in log.iter().enumerate() {
        let want = if k < 5 {
            Status::Modified
        } else {
            Status::Silent
        };
        assert_eq!(*status, want, "block {k}");
    }
}

/// The node's `Silent` is what lets the graph park it, not the graph's latency-and-tail
/// bookkeeping alone: the executor skips a node only when its last output was reported silent,
/// so the same node with its `Silent` said as `Modified` is called on every block.
#[test]
fn a_node_that_never_says_silent_is_never_skipped() {
    let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
    let node = true_stereo_node::<f32, _>(
        Mono::new(&irs[0]),
        Mono::new(&irs[1]),
        Mono::new(&irs[2]),
        Mono::new(&irs[3]),
        Samples::new(64),
    );
    let (recording, log) = Recording::new(node);
    recording.hide_silence.store(true, Ordering::Relaxed);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let source = g.add(Unforkable(Impulse {
        width: ChannelLayout::STEREO,
        at: 0,
    }));
    let key = g.add(Unforkable(recording));
    for c in 0..2 {
        g.connect(source, c, key, c);
        g.connect_output(key, c, c);
    }
    let mut r = g
        .renderer(Prepare::new(SR, MaxBlock::of(Samples::new(BLOCK))))
        .expect("a two-node graph builds");
    r.render(FRAMES);
    assert_eq!(calls(&log), FRAMES / BLOCK);
}

/// Noise at frame `n` of channel `c`, never exactly zero.
fn noise_at(c: usize, n: usize) -> f32 {
    let h = (n as u32 ^ (c as u32) << 20).wrapping_mul(2_654_435_761);
    (h >> 8) as f32 / (1u32 << 24) as f32 + 0.25
}

/// Whether the stream plays a burst at frame `n`: frames 0..100 and 3000..3100.
fn loud(n: usize) -> bool {
    n < 100 || (3000..3100).contains(&n)
}

/// A source of `width` channels playing the two bursts, `Status::Silent` on a block with no
/// sound. Its output is a function of the absolute frame, so whatever the blocks the same
/// samples are played.
struct Bursts {
    width: ChannelLayout,
    at: usize,
}

impl Node for Bursts {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.width)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut any = false;
        for c in 0..usize::from(self.width.count()) {
            for (i, o) in io.output(c).iter_mut().enumerate() {
                let n = self.at + i;
                *o = if loud(n) { noise_at(c, n) } else { 0.0 };
                any |= *o != 0.0;
            }
        }
        self.at += io.frames();
        if any {
            Status::Modified
        } else {
            Status::Silent
        }
    }
    fn reset(&mut self) {
        self.at = 0;
    }
}

const STREAM: usize = 3500;

/// The two bursts as `width` channels of [`STREAM`] frames.
fn stream(width: usize) -> Vec<Vec<f32>> {
    (0..width)
        .map(|c| {
            (0..STREAM)
                .map(|n| if loud(n) { noise_at(c, n) } else { 0.0 })
                .collect()
        })
        .collect()
}

/// A driven gain: a value per absolute frame, finite and moving.
fn signal() -> Vec<f32> {
    (0..STREAM)
        .map(|n| 0.5 + 0.25 * ((n % 7) as f32) / 7.0)
        .collect()
}

/// `node` called on every block of `block` frames over `input`, in two segments split at frame
/// 1500, where `gain` moves to 0.25 (the node is parked by then), and `signal` driven on the gain
/// port if given. A block the node answers `Silent` to is zeros, as the graph would leave it.
fn direct(
    node: ConvolverNode<f32>,
    gain: &Param<Amplitude>,
    input: &[Vec<f32>],
    block: usize,
    signal: Option<&[f32]>,
) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, 128);
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); usize::from(node.outputs().count())];
    for (lo, hi) in [(0, 1500), (1500, input[0].len())] {
        if lo == 1500 {
            gain.store(Amplitude::new(0.25));
        }
        let mut at = lo;
        while at < hi {
            let n = block.min(hi - at);
            let ins: Vec<&[f32]> = input.iter().map(|c| &c[at..at + n]).collect();
            let params = [signal.map(|l| &l[at..at + n])];
            let params: &[Option<&[f32]>] = if signal.is_some() { &params } else { &[] };
            for (o, b) in
                out.iter_mut()
                    .zip(drive(&mut node, SR, Planes::new(&ins).unwrap(), params))
            {
                o.extend(b);
            }
            at += n;
        }
    }
    out
}

fn bits_equal(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The kinds of two-channel node the resume tests run: each at a tail of 200 and latency 64.
type Make = fn() -> ConvolverNode<f32>;

fn kinds() -> Vec<(&'static str, Make)> {
    fn true_stereo() -> ConvolverNode<f32> {
        let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
        true_stereo_node::<f32, _>(
            Mono::new(&irs[0]),
            Mono::new(&irs[1]),
            Mono::new(&irs[2]),
            Mono::new(&irs[3]),
            Samples::new(64),
        )
    }
    fn dual_mono() -> ConvolverNode<f32> {
        per_channel_node::<f32, _>(
            &[Mono::new(&tap(200, 0.5)), Mono::new(&tap(120, -0.75))],
            Samples::new(64),
        )
        .unwrap()
    }
    fn folded() -> ConvolverNode<f32> {
        folded_node::<f32, _>(
            &[Mono::new(&tap(200, 0.5)), Mono::new(&tap(120, -0.75))],
            Samples::new(64),
        )
        .unwrap()
    }
    vec![
        ("true stereo", true_stereo as Make),
        ("dual mono", dual_mono as Make),
        ("folded", folded as Make),
    ]
}

/// Silence long enough to park the node (the gap is 2900 frames; the node's tail is 264), a gain
/// change made while it is parked, then a burst, for true stereo, dual mono and folded nodes:
///
/// - called on every block (driven by hand), the whole render is the same bits at blocks of 64,
///   37, 100 and 128, and equal to the render through a graph, which skips the parked node;
/// - the second burst and its tail are a fresh node's at the new gain, bit for bit, and the
///   frames past the tail are exact zeros.
///
/// A gain that ramped over the first loud block (its ramp frozen while parked), or an engine
/// that carried its partition phase and round-off across the gap, would differ in the bits.
#[test]
fn a_burst_after_a_parked_gap_is_a_fresh_nodes_at_any_block_size() {
    for (name, make) in kinds() {
        let input = stream(2);
        // The second burst alone through a fresh node at the gain it resumes at.
        let fresh = make();
        fresh.gain().store(Amplitude::new(0.25));
        let second: Vec<Vec<f32>> = input.iter().map(|c| c[3000..STREAM].to_vec()).collect();
        let want = direct_once(fresh, &second);

        let mut first_render: Option<Vec<Vec<f32>>> = None;
        for block in [64usize, 37, 100, 128] {
            let node = make();
            let gain = node.gain();
            let continuous = direct(node, &gain, &input, block, None);
            for (c, w) in want.iter().enumerate() {
                assert!(
                    bits_equal(&continuous[c][3000..STREAM], w),
                    "{name}, block {block}, output {c}: the resumed burst is not a fresh node's"
                );
            }
            match &first_render {
                None => first_render = Some(continuous.clone()),
                Some(f) => {
                    for c in 0..2 {
                        assert!(
                            bits_equal(&continuous[c], &f[c]),
                            "{name}, block {block}, output {c}: the render depends on the blocks"
                        );
                    }
                }
            }

            // The same stream through a graph, which skips the node once it is parked.
            let node = make();
            let gain = node.gain();
            let (recording, log) = Recording::new(node);
            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
            let source = g.add(Unforkable(Bursts {
                width: ChannelLayout::STEREO,
                at: 0,
            }));
            let key = g.add(Unforkable(recording));
            for c in 0..2 {
                g.connect(source, c, key, c);
                g.connect_output(key, c, c);
            }
            let mut r = g
                .renderer(Prepare::new(SR, MaxBlock::of(Samples::new(128))))
                .expect("a two-node graph builds");
            r.set_block(Samples::new(block));
            let mut got = r.render(1500);
            gain.store(Amplitude::new(0.25));
            for (a, b) in got.iter_mut().zip(r.render(STREAM - 1500)) {
                a.extend(b);
            }
            for c in 0..2 {
                assert!(
                    bits_equal(&got[c], &continuous[c]),
                    "{name}, block {block}, output {c}: the parked graph's render is not the \
                     render of a node called on every block"
                );
            }
            let blocks = STREAM.div_ceil(block);
            assert!(
                calls(&log) < blocks / 2,
                "{name}, block {block}: called {} times of {blocks}: the graph never skipped it",
                calls(&log)
            );
        }
    }
}

/// `node` over `input` in one block, as a fresh node renders it.
fn direct_once(node: ConvolverNode<f32>, input: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, 512);
    let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
    drive(&mut node, SR, Planes::new(&ins).unwrap(), &[])
}

/// The same with a gain driven on the `Volume` port, per frame: at every block size the render
/// is the same bits, and the second burst is a fresh node's under the same control signal.
#[test]
fn a_burst_after_a_parked_gap_is_a_fresh_nodes_with_a_driven_gain() {
    let signal = signal();
    for (name, make) in kinds() {
        let input = stream(2);
        let fresh = make();
        let second: Vec<Vec<f32>> = input.iter().map(|c| c[3000..STREAM].to_vec()).collect();
        let mut fresh = prepared(fresh, SR, 512);
        let ins: Vec<&[f32]> = second.iter().map(|c| &c[..]).collect();
        let want = drive(
            &mut fresh,
            SR,
            Planes::new(&ins).unwrap(),
            &[Some(&signal[3000..STREAM])],
        );

        let mut first_render: Option<Vec<Vec<f32>>> = None;
        for block in [64usize, 37, 100, 128] {
            let node = make();
            let gain = node.gain();
            let got = direct(node, &gain, &input, block, Some(&signal));
            for (c, w) in want.iter().enumerate() {
                assert!(
                    bits_equal(&got[c][3000..STREAM], w),
                    "{name}, block {block}, output {c}: the resumed burst is not a fresh node's"
                );
            }
            match &first_render {
                None => first_render = Some(got),
                Some(f) => {
                    for c in 0..2 {
                        assert!(
                            bits_equal(&got[c], &f[c]),
                            "{name}, block {block}, output {c}: the render depends on the blocks"
                        );
                    }
                }
            }
        }
    }
}

/// The restart boundary, with the burst's first frame mid-block at blocks of 64 and 37: the
/// bursts are 100 noise frames each, `gap` silent frames apart, through true stereo
/// (latency 64, tail 200, so 264 owed).
///
/// - A gap of 263 frames does not restart the engine: the whole render, through the second
///   burst's tail, is the bits of a bare engine run over the stream without a break.
/// - A gap of 264 frames does: up to the second burst it is the bare engine's bits (the first
///   burst's tail is all before it), and from it the second burst is a fresh engine's, which
///   differs from the continuous engine (so the restart is seen, and the test could fail).
#[test]
fn a_gap_of_exactly_the_tail_restarts_the_engine_and_one_less_does_not() {
    let irs = [tap(40, 0.5), tap(90, -0.25), tap(10, 0.75), tap(200, 0.125)];
    let engine = |input: &[Vec<f32>]| -> Vec<Vec<f32>> {
        let mut m = MatrixConvolver::<f32>::two_by_two(
            [
                Mono::new(&irs[0][..]),
                Mono::new(&irs[1][..]),
                Mono::new(&irs[2][..]),
                Mono::new(&irs[3][..]),
            ],
            Samples::new(64),
        );
        let mut out = vec![vec![0.0f32; input[0].len()]; 2];
        m.process(
            &mut InputOutput::paired(
                Planes::from_vecs(input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        out
    };
    let node = || {
        true_stereo_node::<f32, _>(
            Mono::new(&irs[0]),
            Mono::new(&irs[1]),
            Mono::new(&irs[2]),
            Mono::new(&irs[3]),
            Samples::new(64),
        )
    };
    for gap in [263usize, 264] {
        let start2 = 100 + gap;
        let total = start2 + 100 + 264 + 64;
        let input: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..total)
                    .map(|n| {
                        if n < 100 || (start2..start2 + 100).contains(&n) {
                            noise_at(c, n)
                        } else {
                            0.0
                        }
                    })
                    .collect()
            })
            .collect();
        let continuous = engine(&input);
        let second: Vec<Vec<f32>> = input.iter().map(|c| c[start2..].to_vec()).collect();
        let fresh = engine(&second);
        let sounding = start2 + 100 + 264;
        for block in [64usize, 37] {
            assert_ne!(
                (start2 % block, start2 % block),
                (0, 0),
                "the burst starts mid-block"
            );
            let got = direct_plain(node(), &input, block);
            for c in 0..2 {
                if gap == 263 {
                    assert!(
                        bits_equal(&got[c][..sounding], &continuous[c][..sounding]),
                        "gap {gap}, block {block}, output {c}: not the unbroken engine's bits"
                    );
                } else {
                    assert!(
                        bits_equal(&got[c][..start2], &continuous[c][..start2]),
                        "gap {gap}, block {block}, output {c}: differs before the restart"
                    );
                    assert!(
                        bits_equal(&got[c][start2..sounding], &fresh[c][..sounding - start2]),
                        "gap {gap}, block {block}, output {c}: the second burst is not a \
                         fresh engine's"
                    );
                }
                // Past the tail: exact zeros.
                assert!(got[c][sounding..].iter().all(|&v| v == 0.0));
            }
        }
        // The restart changes the bits: a fresh engine is not the continuous one here, or the
        // two cases above could not tell restart from none.
        assert!(
            (0..2).any(|c| !bits_equal(
                &continuous[c][start2..sounding],
                &fresh[c][..sounding - start2]
            )),
            "gap {gap}: a fresh engine and the continuous one agree, so this cannot see a restart"
        );
    }
}

/// `node` called on every block of `block` frames over `input`, gain at unity.
fn direct_plain(node: ConvolverNode<f32>, input: &[Vec<f32>], block: usize) -> Vec<Vec<f32>> {
    let mut node = prepared(node, SR, block.max(128));
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); usize::from(node.outputs().count())];
    let mut at = 0;
    while at < input[0].len() {
        let n = block.min(input[0].len() - at);
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[at..at + n]).collect();
        for (o, b) in out
            .iter_mut()
            .zip(drive(&mut node, SR, Planes::new(&ins).unwrap(), &[]))
        {
            o.extend(b);
        }
        at += n;
    }
    out
}

/// A gap inside one call: the node called once over the whole 3500-frame two-burst stream (a
/// block longer than the 264 frames owed, so the restart and the cut before it both happen
/// within the call) renders the same bits as blocks of 64, for true stereo and dual mono, and
/// the second burst is a fresh node's.
#[test]
fn a_gap_inside_one_call_renders_as_blocks_of_64_do() {
    let input = stream(2);
    let second: Vec<Vec<f32>> = input.iter().map(|c| c[3000..STREAM].to_vec()).collect();
    for (name, make) in kinds().into_iter().take(2) {
        let blocks = direct_plain(make(), &input, 64);
        let whole = direct_plain(make(), &input, STREAM);
        let fresh = direct_once(make(), &second);
        for c in 0..2 {
            assert!(
                bits_equal(&whole[c], &blocks[c]),
                "{name}, output {c}: one call over the gap is not blocks of 64"
            );
            assert!(
                bits_equal(&whole[c][3000..STREAM], &fresh[c]),
                "{name}, output {c}: the burst after the gap is not a fresh node's"
            );
        }
    }
}

fn unassigned_bus(width: impl Into<tutti_buffers::ChannelLayout>) -> tutti_buffers::BusTopology {
    tutti_buffers::BusTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// One response per channel, at as many unassigned channels as responses.
fn per_channel_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(unassigned_bus(width), irs).block_size(block_size),
    )
}

/// Every input folded to mono, then one response per output, at the engine's placement of
/// that many channels.
fn folded_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    let bus = tutti_buffers::BusTopology::default_for(width).expect("a test width fits a bus");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(bus, irs)
            .folded()
            .block_size(block_size),
    )
}

/// True stereo from `[LL, LR, RL, RR]`.
fn true_stereo_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    ll: tutti_buffers::Mono<'_, S>,
    lr: tutti_buffers::Mono<'_, S>,
    rl: tutti_buffers::Mono<'_, S>,
    rr: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> tutti_nodes::ConvolverNode<F> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(tutti_buffers::BusTopology::STEREO, &[ll, lr, rl, rr])
            .true_stereo()
            .block_size(block_size),
    )
    .expect("four responses on a stereo pair")
}

/// An ambisonic set of `input` in, `output` out.
fn ambisonic_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    input: tutti_buffers::AmbisonicFormat,
    output: tutti_buffers::ChannelTopology,
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let output = tutti_buffers::BusTopology::try_from(&output).expect("a test output fits a bus");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(output, irs)
            .ambisonic(input)
            .block_size(block_size),
    )
}
