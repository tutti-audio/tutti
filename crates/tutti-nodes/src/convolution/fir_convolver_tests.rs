//! [`FirConvolverConfig`]: a linear-phase filter through the convolver.

use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_dsp::{Fir, LinearPhaseDesign, MatchedBell, Sos, Tilt};
use tutti_graph::Node;
use tutti_units::{Db, Hz, Q, SampleRate, Samples, Tail};

use super::{ConvolverError, ConvolverNode, FirConvolverConfig};
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

/// `width` channels on the engine's placement, as a bus.
fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(ChannelLayout::from_count(width)).expect("a test width fits a bus")
}

fn node(width: u16, fir: &Fir) -> ConvolverNode<f64> {
    ConvolverNode::try_from(
        FirConvolverConfig::new(bus(width), fir).block_size(Samples::new(BLOCK)),
    )
    .unwrap()
}

/// The node's output for a unit impulse at frame 0 of each channel in `on`, over `frames`.
fn impulse_response(
    node: ConvolverNode<f64>,
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

/// The most an `f64` partitioned convolution of an impulse with `taps` may be off a tap: a
/// stated margin of 8 times `log2(n) · 2^-53` of the taps' l1 norm, the size of the rounding of
/// a transform of `n` points (the response is a sum of at most `n` products of an impulse).
fn rounding_bound(taps: &[f64]) -> f64 {
    let l1: f64 = taps.iter().map(|t| t.abs()).sum();
    8.0 * (taps.len() as f64).log2() * 2f64.powi(-53) * l1
}

/// The declared latency is the convolver's block plus `(N - 1) / 2` for `N` taps, and the
/// tail the remaining `(N - 1) / 2`.
#[test]
fn the_declared_latency_is_the_block_plus_half_the_taps_less_one() {
    let fir = design(&bands());
    let n = fir.len().get();
    let shape = node(1, &fir).shape();
    assert_eq!(shape.latency.samples().get(), BLOCK + (n - 1) / 2);
    assert_eq!(shape.tail, Tail::Finite(Samples::new((n - 1) / 2)));
}

/// A flat target is a unit impulse at the centre of the taps, so the node's impulse response
/// is 0 everywhere but one frame, the declared latency, where it is 1.
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
    let bound = rounding_bound(&fir.taps().collect::<Vec<_>>());
    assert!((out[peak] - 1.0).abs() <= bound);
    for (n, &y) in out.iter().enumerate().filter(|&(n, _)| n != latency) {
        assert!(y.abs() <= bound, "frame {n}: {y}");
    }
}

/// An impulse through the node is the taps, from the block on, within the rounding bound; the
/// frames before the block are exactly silent.
#[test]
fn an_impulse_through_the_node_is_the_taps() {
    let fir = design(&bands());
    let taps: Vec<f64> = fir.taps().collect();
    let out = &impulse_response(node(1, &fir), 1, &[0], BLOCK + taps.len() + BLOCK)[0];
    assert!(out[..BLOCK].iter().all(|&y| y == 0.0));
    let bound = rounding_bound(&taps);
    for (k, &tap) in taps.iter().enumerate() {
        assert!(
            (out[BLOCK + k] - tap).abs() <= bound,
            "tap {k}: {} against {tap}, bound {bound:e}",
            out[BLOCK + k]
        );
    }
    assert!(out[BLOCK + taps.len()..].iter().all(|y| y.abs() <= bound));
}

/// The channels are independent: an impulse on the left is the taps on the left and exactly
/// silence on the right.
#[test]
fn channels_do_not_couple() {
    let fir = design(&bands());
    let taps: Vec<f64> = fir.taps().collect();
    let out = impulse_response(node(2, &fir), 2, &[0], BLOCK + taps.len() + BLOCK);
    assert!(out[1].iter().all(|&y| y == 0.0));
    let bound = rounding_bound(&taps);
    for (k, &tap) in taps.iter().enumerate() {
        assert!((out[0][BLOCK + k] - tap).abs() <= bound, "tap {k}");
    }
}

/// A bus of no channels is refused by name, never a panic.
#[test]
fn a_bus_of_no_channels_is_refused_by_name() {
    let fir = design(&bands());
    assert_eq!(
        ConvolverNode::<f32>::try_from(
            FirConvolverConfig::new(BusTopology::EMPTY, &fir).block_size(Samples::new(BLOCK))
        )
        .unwrap_err(),
        ConvolverError::NoChannels
    );
}
