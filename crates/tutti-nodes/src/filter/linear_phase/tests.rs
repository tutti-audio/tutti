use tutti_buffers::ChannelLayout;
use tutti_dsp::{LinearPhaseDesign, MatchedBell, Sos, Tilt};
use tutti_graph::Node;
use tutti_units::{Db, Hz, Q, SampleRate, Samples, Tail};

use super::*;
use crate::test_support::render_f64;

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn bands() -> [Sos; 2] {
    [
        MatchedBell::new(Q::new(1.0), Db::new(9.0))
            .design(Hz::new(1_000.0), RATE)
            .unwrap(),
        Tilt::new(Db::new(-6.0))
            .design(Hz::new(4_000.0), RATE)
            .unwrap(),
    ]
}

fn design(bands: &[Sos]) -> Fir {
    LinearPhaseDesign::new(Hz::new(200.0))
        .design(RATE, bands)
        .unwrap()
}

fn node(width: u8, fir: &Fir) -> LinearPhaseEqNode<f64> {
    LinearPhaseEqNode::from(
        LinearPhaseEqConfig::new(ChannelLayout::from_count(u16::from(width)), fir)
            .block_size(Samples::new(BLOCK)),
    )
}

/// The node's output for a unit impulse at frame 0 of every channel in `on`, over `frames`.
fn impulse_response(
    node: LinearPhaseEqNode<f64>,
    width: usize,
    on: &[usize],
    frames: usize,
) -> Vec<Vec<f64>> {
    let mut impulse = vec![0.0; frames];
    impulse[0] = 1.0;
    let silence = vec![0.0; frames];
    let inputs: Vec<&[f64]> = (0..width)
        .map(|c| {
            if on.contains(&c) {
                &impulse[..]
            } else {
                &silence[..]
            }
        })
        .collect();
    render_f64(node, RATE, &inputs, frames)
}

/// The declared latency is the convolver's block plus `(N - 1) / 2`, and the tail the
/// remaining `(N - 1) / 2`: the latency and the tail sum to the block and `N - 1`, where the
/// last frame of the response lands.
#[test]
fn the_declared_latency_is_the_block_plus_half_the_taps_less_one() {
    let fir = design(&bands());
    let n = fir.len().get();
    assert_eq!(n % 2, 1);
    let shape = node(1, &fir).shape();
    assert_eq!(shape.latency.samples().get(), BLOCK + (n - 1) / 2);
    assert_eq!(shape.tail, Tail::Finite(Samples::new((n - 1) / 2)));
    assert_eq!(
        shape.latency.samples().get() + (n - 1) / 2,
        BLOCK + n - 1,
        "the last frame of the response"
    );
}

/// A flat target is a unit impulse at the centre of the taps, so the node's impulse response
/// is exactly 0 everywhere but one frame, the declared latency, where it is 1.
#[test]
fn a_flat_target_peaks_at_exactly_the_declared_latency() {
    let none: [Sos; 0] = [];
    let fir = design(&none);
    let node = node(1, &fir);
    let latency = node.shape().latency.samples().get();
    let out = &impulse_response(node, 1, &[0], latency + fir.len().get() + BLOCK)[0];
    let peak = (0..out.len())
        .max_by(|&a, &b| out[a].abs().total_cmp(&out[b].abs()))
        .unwrap();
    assert_eq!(peak, latency);
    assert!((out[peak] - 1.0).abs() < 1e-13);
    for (n, &y) in out.iter().enumerate().filter(|&(n, _)| n != latency) {
        assert!(y.abs() < 1e-13, "frame {n}: {y}");
    }
}

/// An impulse through the node is the taps, from the block on: the convolution is exact up to
/// the FFT's rounding, `2^-40` of the taps' l1 norm (an `f64` FFT of a few thousand points
/// resolves its sums to a few `2^-53` relative per butterfly stage, far under the bound), and
/// the frames before the block are exactly silent.
#[test]
fn an_impulse_through_the_node_is_the_taps() {
    let fir = design(&bands());
    let taps: Vec<f64> = fir.taps().collect();
    let l1: f64 = taps.iter().map(|t| t.abs()).sum();
    let out = &impulse_response(node(1, &fir), 1, &[0], BLOCK + taps.len() + BLOCK)[0];
    assert!(out[..BLOCK].iter().all(|&y| y == 0.0));
    let bound = l1 * 2f64.powi(-40);
    for (k, &tap) in taps.iter().enumerate() {
        assert!(
            (out[BLOCK + k] - tap).abs() <= bound,
            "tap {k}: {} against {tap}",
            out[BLOCK + k]
        );
    }
    assert!(out[BLOCK + taps.len()..].iter().all(|y| y.abs() <= bound));
}

/// The channels are independent: an impulse on the left leaves the right exactly silent, and
/// the left is the same taps.
#[test]
fn channels_do_not_couple() {
    let fir = design(&bands());
    let frames = BLOCK + fir.len().get() + BLOCK;
    let out = impulse_response(node(2, &fir), 2, &[0], frames);
    assert!(out[1].iter().all(|&y| y == 0.0));
    let mono = &impulse_response(node(1, &fir), 1, &[0], frames)[0];
    assert_eq!(&out[0], mono);
}

/// A fork shares the taps' spectra and renders the same impulse response.
#[test]
fn a_fork_has_the_same_response() {
    let fir = design(&bands());
    let frames = BLOCK + fir.len().get() + BLOCK;
    let live = node(1, &fir);
    let fork = live.fork_fresh();
    assert_eq!(live.shape(), fork.shape());
    assert_eq!(
        impulse_response(live, 1, &[0], frames),
        impulse_response(fork, 1, &[0], frames)
    );
}
