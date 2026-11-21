use super::*;
use crate::test_support::{RATE, prepared, render_chunked};
use core::num::NonZeroU16;
use tutti_graph::Node;
use tutti_graph::contract::assert_declared_tail;
use tutti_units::{Hz, Samples, Seconds};

/// 2048 impulses a second over 62.5 ms: 128 impulses.
fn node(width: u16, seed: u64) -> DecorrelatorNode<f32> {
    prepared(DecorrelatorNode::from(
        DecorrelatorConfig::new(NonZeroU16::new(width).unwrap())
            .density(VelvetDensity::new(Hz::new(2_048.0)).unwrap())
            .length(VelvetLength::new(Seconds::new(0.0625)).unwrap())
            .seed(seed),
    ))
}

/// The shape is the width in and out with no latency, and the tail is the latest impulse,
/// inside the length.
#[test]
fn the_shape_is_the_width_with_no_latency_and_a_tail_inside_the_length() {
    let mut n = node(2, 1);
    let shape = n.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (2, 2)
    );
    assert_eq!(shape.latency, tutti_units::Latency::new(Samples::ZERO));
    let Tail::Finite(tail) = shape.tail else {
        panic!("a finite tail")
    };
    assert!((2_900..3_000).contains(&tail.get()), "{tail:?}");
    assert_declared_tail(&mut n, RATE);
    assert_eq!(
        DecorrelatorNode::<f32>::from(DecorrelatorConfig::new(NonZeroU16::MIN))
            .shape()
            .audio_in()
            .count(),
        1
    );
}

/// An impulse through the node is `floor(length * density)` taps on each channel, of unit
/// energy, and the channels' differ with the seed's stream: the sparse response is what the
/// config asked for.
#[test]
fn an_impulse_through_the_node_is_a_sparse_response_of_unit_energy() {
    let mut n = node(2, 9);
    let mut x = vec![vec![0.0f32; 3_100]; 2];
    x[0][0] = 1.0;
    x[1][0] = 1.0;
    let y = render_chunked(&mut n, &x, 64);
    for ch in &y {
        assert_eq!(ch.iter().filter(|v| **v != 0.0).count(), 128);
        let energy: f64 = ch.iter().map(|v| f64::from(*v).powi(2)).sum();
        // `f32` taps: 128 products at 2^-24 each.
        assert!((energy - 1.0).abs() < 128.0 * 2.0f64.powi(-23), "{energy}");
    }
    assert_ne!(y[0], y[1]);
}
