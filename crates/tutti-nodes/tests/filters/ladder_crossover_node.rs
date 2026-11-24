//! `LadderFilterNode` and `CrossoverNode` as the graph runs them: what a node is across a reset,
//! a clone and a fork, when a move that lands between two grid points starts, and what its
//! tail declares. Every figure is the node's own definition (reset is silence, a grid of 16
//! frames counted from the last reset, an unknown tail is never parked), not another render's.

// These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.

use tutti_buffers::ChannelLayout;
use tutti_buffers::ChannelTopology;
use tutti_buffers::Planes;
use tutti_graph::contract::{drive, drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Status, TransportChanges, TransportPosition};
use tutti_nodes::{
    CrossoverConfig, CrossoverControls, CrossoverNode, LadderFilterConfig, LadderFilterNode,
    LadderType,
};
use tutti_units::{Frame, Hz, Resonance, SampleRate, Samples, Tail, UnitAddr, UnitParam};

const SR: SampleRate = SampleRate::SR_48K;

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    tutti_fixtures::noise::<f32>(seed, frames)
        .into_iter()
        .map(|x| 0.5 * x)
        .collect()
}

/// A node with a first cutoff cell that a test can move: the ladder's cutoff, the crossover's
/// first split.
trait Cutoff: Node + ParamNode + Clone + 'static {
    const NAME: &'static str;
    fn build() -> Self;
    fn set_cutoff(&self, hz: f64) -> bool;
    fn cutoff(&self) -> Option<f64>;
}

impl Cutoff for LadderFilterNode<f32> {
    const NAME: &'static str = "ladder";
    fn build() -> Self {
        LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                .freq(Hz::new(900.0))
                .res(Resonance::new(0.7)),
        )
    }
    fn set_cutoff(&self, hz: f64) -> bool {
        self.param_set().set(UnitParam::Cutoff, hz)
    }
    fn cutoff(&self) -> Option<f64> {
        self.param_base(0)
    }
}

impl Cutoff for CrossoverNode<f32> {
    const NAME: &'static str = "crossover";
    fn build() -> Self {
        CrossoverNode::<f32>::try_from(CrossoverConfig::new(
            ChannelTopology::STEREO,
            CrossoverControls::try_from(&[Hz::new(300.0), Hz::new(3_000.0)][..]).unwrap(),
        ))
        .unwrap()
    }
    fn set_cutoff(&self, hz: f64) -> bool {
        self.param_set()
            .set(UnitAddr::new(UnitParam::Cutoff, 0), hz)
    }
    fn cutoff(&self) -> Option<f64> {
        self.param_base(0)
    }
}

/// Renders `x` (on both channels) through `node` in blocks of `block`, calling `before(at)`
/// first with the stream frame `at` each block starts on, and returns every output channel.
fn render(
    node: &mut dyn Node,
    x: &[f32],
    block: usize,
    mut before: impl FnMut(usize),
) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = Vec::new();
    for (i, chunk) in x.chunks(block).enumerate() {
        before(i * block);
        let y = drive(node, SR, Planes::new(&[chunk, chunk]).unwrap(), &[]);
        out.resize(y.len(), Vec::new());
        for (all, part) in out.iter_mut().zip(y) {
            all.extend(part);
        }
    }
    out
}

fn check_a_reset_node_renders_as_a_fresh_one<N: Cutoff>() {
    let x = noise(3, 1_024);
    let mut used = prepared(N::build(), SR, 64);
    assert!(used.set_cutoff(2_500.0));
    render(&mut used, &noise(9, 3_000), 64, |_| {});
    used.reset();
    let fresh = N::build();
    assert!(fresh.set_cutoff(2_500.0));
    let mut fresh = prepared(fresh, SR, 64);
    let (a, b) = (
        render(&mut used, &x, 64, |_| {}),
        render(&mut fresh, &x, 64, |_| {}),
    );
    assert!(a.iter().flatten().any(|&s| s != 0.0), "{}", N::NAME);
    assert_eq!(a, b, "{}", N::NAME);
}

/// A reset node is a fresh one: after a burst of noise and a `reset`, the same signal renders the
/// same bits as on a node that never ran (the state is silence, the design is on its target, the
/// grid counts from the reset).
#[test]
fn a_reset_node_renders_as_a_fresh_one() {
    check_a_reset_node_renders_as_a_fresh_one::<LadderFilterNode<f32>>();
    check_a_reset_node_renders_as_a_fresh_one::<CrossoverNode<f32>>();
}

/// A reset node is a fresh one whatever moved before it: after audio rendered at one design, a
/// cell written to another and a `reset`, the same signal renders the same bits as on a node
/// built with that cell, so the first design after the reset lands at once and does not ramp
/// from the one before it.
#[test]
fn a_reset_after_a_move_renders_as_a_fresh_node_with_the_moved_controls() {
    let x = noise(4, 1_024);
    let mut used = prepared(CrossoverNode::build(), SR, 64);
    render(&mut used, &noise(8, 2_048), 64, |_| {});
    assert!(used.set_cutoff(2_500.0));
    used.reset();
    let fresh = CrossoverNode::build();
    assert!(fresh.set_cutoff(2_500.0));
    let mut fresh = prepared(fresh, SR, 64);
    let (a, b) = (
        render(&mut used, &x, 64, |_| {}),
        render(&mut fresh, &x, 64, |_| {}),
    );
    assert!(a.iter().flatten().any(|&s| s != 0.0));
    assert_eq!(a, b);
}

fn check_a_move_between_grid_points_starts_at_the_next_one<N: Cutoff>() {
    let x = noise(5, 128);
    let run = |write: bool| {
        let node = N::build();
        let mut node = prepared(node, SR, 64);
        let writer = node.clone();
        render(&mut node, &x, 8, |at| {
            if write && at == 40 {
                assert!(writer.set_cutoff(6_000.0));
            }
        })
    };
    let (moved, held) = (run(true), run(false));
    for (m, h) in moved.iter().zip(&held) {
        assert_eq!(m[..48], h[..48], "{}: before the grid point", N::NAME);
    }
    assert!(
        moved.iter().zip(&held).any(|(m, h)| m[48..64] != h[48..64]),
        "{}: the write is heard from the grid point",
        N::NAME
    );
}

/// A cell written between two blocks takes effect at the next grid point, not before. The
/// grid is 16 frames from the reset; the cell is written at frame 40 (a block boundary), the
/// first grid point after it is 48, so the frames before 48 are what they are with no write
/// and the 16 from 48 are not.
#[test]
fn a_move_between_grid_points_starts_at_the_next_one() {
    check_a_move_between_grid_points_starts_at_the_next_one::<LadderFilterNode<f32>>();
    check_a_move_between_grid_points_starts_at_the_next_one::<CrossoverNode<f32>>();
}

fn check_a_clone_starts_silent_and_shares_the_cells<N: Cutoff>() {
    let mut node = prepared(N::build(), SR, 64);
    render(&mut node, &noise(2, 512), 64, |_| {});
    let mut clone = node.clone();
    let silent = render(&mut clone, &vec![0.0; 256], 64, |_| {});
    assert!(
        silent.iter().flatten().all(|&s| s == 0.0),
        "{}: silent",
        N::NAME
    );
    assert!(node.set_cutoff(1_234.0));
    assert_eq!(clone.cutoff(), Some(1_234.0), "{}: shared cell", N::NAME);
}

/// A clone shares the cells and starts silent: a zero input gives it exactly zero though the
/// node it was cloned from has been ringing, and a write through the original reads on the
/// clone.
#[test]
fn a_clone_starts_silent_and_shares_the_cells() {
    check_a_clone_starts_silent_and_shares_the_cells::<LadderFilterNode<f32>>();
    check_a_clone_starts_silent_and_shares_the_cells::<CrossoverNode<f32>>();
}

fn check_a_fork_starts_silent_and_shares_nothing<N: Cutoff>() {
    let mut node = prepared(N::build(), SR, 64);
    assert!(node.set_cutoff(777.0));
    render(&mut node, &noise(2, 512), 64, |_| {});
    let mut fork = node.fork_fresh();
    assert_eq!(fork.cutoff(), Some(777.0), "{}: the values set", N::NAME);
    let silent = render(&mut fork, &vec![0.0; 256], 64, |_| {});
    assert!(
        silent.iter().flatten().all(|&s| s == 0.0),
        "{}: silent",
        N::NAME
    );
    assert!(node.set_cutoff(4_000.0));
    assert_eq!(fork.cutoff(), Some(777.0), "{}: no shared cell", N::NAME);
}

/// A fork starts from the values last set and silent, and shares no cell.
#[test]
fn a_fork_starts_silent_and_shares_nothing() {
    check_a_fork_starts_silent_and_shares_nothing::<LadderFilterNode<f32>>();
    check_a_fork_starts_silent_and_shares_nothing::<CrossoverNode<f32>>();
}

/// The two nodes declare an unknown tail, and the node holds to it: after one impulse and 4 000
/// silent blocks it has never reported a tail, never been parked or cut (every status is
/// `Modified`), and the resonant ladder (a resonance of 1 sustains) is still ringing.
#[test]
fn an_unknown_tail_is_never_narrowed_cut_or_parked() {
    let rung = |node: &mut dyn Node| {
        assert_eq!(node.shape().tail, Tail::Unknown);
        let mut ring = 0.0f32;
        for block in 0..4_000u64 {
            let env = Env {
                frame: Frame::new(block * 64),
                sample_rate: SR,
                block_len: Samples::new(64),
                transport: TransportPosition::default(),
                changes: TransportChanges::NONE,
                play: PlayClock::ZERO,
            };
            let mut x = [0.0f32; 64];
            x[0] = if block == 0 { 0.01 } else { 0.0 };
            let d = drive_input(node, &env, Planes::new(&[&x, &x]).unwrap(), &[], &[]);
            assert_eq!(d.status, Some(Status::Modified), "block {block}");
            assert_eq!(d.reported_tail, None, "block {block}");
            ring = tutti_fixtures::signal::peak_planes(&d.audio);
        }
        ring
    };
    let mut resonant = prepared(
        LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                .freq(Hz::new(1_000.0))
                .res(Resonance::new(1.0)),
        ),
        SR,
        64,
    );
    assert!(rung(&mut resonant) > 1e-6, "a resonance of 1 rings on");
    rung(&mut prepared(CrossoverNode::build(), SR, 64));
}

/// The DTFT of `y` at `hz`, over `rate`.
fn dtft(y: &[f32], hz: f64) -> (f64, f64) {
    let w = 2.0 * core::f64::consts::PI * hz / SR.get();
    y.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &v)| {
        let p = w * n as f64;
        (re + f64::from(v) * p.cos(), im - f64::from(v) * p.sin())
    })
}

/// A crossover with more than 16 output planes writes every one of them: 6 bands over 3
/// channels (18 planes) and 3 bands over 8 (24). Channel `c` is fed an impulse of `c + 1`;
/// plane `band * channels + channel` has, at the split the band touches (and at each other
/// split and at 20 Hz and 20 kHz), the magnitude of the product of its sections: `x^4 / (1 + x^4)`
/// for each split below it, `1 / (1 + x^4)` for its own, 1 for each above (the squared
/// Butterworth magnitude, `x = tan(pi f / fs) / tan(pi fc / fs)`), times `c + 1`. The probe at a
/// band's own split has the band within a factor 2 of 1 (at least 0.25), so a plane
/// the driver left unwritten (zero) misses its figure by at least 0.25 and fails; the figures are
/// compared to 1e-4 (the output is `f32`, the response 16 000 frames long).
#[test]
fn every_band_plane_of_a_wide_crossover_is_written() {
    let tan = |hz: f64| (core::f64::consts::PI * hz / SR.get()).tan();
    for (channels, splits) in [
        (3u16, vec![100.0f64, 300.0, 1_000.0, 3_000.0, 9_000.0]),
        (8, vec![200.0, 2_000.0]),
    ] {
        let hz: Vec<Hz> = splits.iter().map(|&f| Hz::new(f)).collect();
        let bands = splits.len() + 1;
        let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
            ChannelTopology::default_for(ChannelLayout::from(channels)).unwrap(),
            CrossoverControls::try_from(&hz[..]).unwrap(),
        ))
        .unwrap();
        assert_eq!(
            usize::from(node.shape().audio_out().count()),
            usize::from(channels) * bands
        );
        let mut node = prepared(node, SR, 16_384);
        let inputs: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                let mut x = vec![0.0f32; 16_384];
                x[0] = f32::from(c) + 1.0;
                x
            })
            .collect();
        let refs: Vec<&[f32]> = inputs.iter().map(|x| &x[..]).collect();
        let out = drive(&mut node, SR, Planes::new(&refs).unwrap(), &[]);
        assert!(usize::from(channels) * bands > 16);
        assert_eq!(out.len(), usize::from(channels) * bands);
        let mut probes: Vec<f64> = vec![20.0, 20_000.0];
        probes.extend(splits.iter().copied());
        for band in 0..bands {
            for c in 0..usize::from(channels) {
                let plane = &out[band * usize::from(channels) + c];
                for &f in &probes {
                    let want: f64 = splits
                        .iter()
                        .enumerate()
                        .map(|(j, &fc)| {
                            let x4 = (tan(f) / tan(fc)).powi(4);
                            match j.cmp(&band) {
                                std::cmp::Ordering::Less => x4 / (1.0 + x4),
                                std::cmp::Ordering::Equal => 1.0 / (1.0 + x4),
                                std::cmp::Ordering::Greater => 1.0,
                            }
                        })
                        .product::<f64>()
                        * (c as f64 + 1.0);
                    let (re, im) = dtft(plane, f);
                    let got = re.hypot(im);
                    assert!(
                        (got - want).abs() < 1e-4,
                        "{bands} bands: band {band} channel {c} at {f} Hz: {got} against {want}"
                    );
                }
                // The band's own probe: the plane is not silent.
                let own = splits[band.min(splits.len() - 1)];
                let (re, im) = dtft(plane, own);
                assert!(re.hypot(im) > 0.2, "band {band} channel {c} is silent");
            }
        }
    }
}
