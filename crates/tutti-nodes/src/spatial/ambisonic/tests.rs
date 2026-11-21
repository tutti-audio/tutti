use tutti_buffers::Bed;
use tutti_dsp::Harmonics;
use tutti_graph::contract::{BlockRig, assert_param_fork};
use tutti_graph::{IntoNode, Node};
use tutti_units::{SampleRate, UnitParam};

use super::*;
use crate::test_support::prepared;

const THIRD: AmbisonicOrder = AmbisonicOrder::MAX;
const RATE: SampleRate = SampleRate::SR_48K;

/// The AmbiX bus of `order`: ACN, SN3D.
fn ambix(order: AmbisonicOrder) -> BusTopology {
    BusTopology::ambisonic(AmbisonicFormat::ambix(order))
}

fn aim(azimuth: f64, elevation: f64) -> Direction {
    Direction::new(Azimuth::new(azimuth), Elevation::new(elevation))
}

fn harmonics(order: AmbisonicOrder, n: HarmonicNormalization, d: Direction) -> Vec<f64> {
    Harmonics::new(order, n, d).iter().collect()
}

/// `node` in an `f64` graph over `frames`-frame blocks: each block is fed `inputs` (one
/// value per channel, held), and `blocks` are rendered; the last block's first frame per
/// output channel is returned. `set` moves the controls before the second block.
fn run(
    node: impl IntoNode<f64, Controls = tutti_graph::ParamSet>,
    inputs: &[f64],
    blocks: usize,
    set: impl FnOnce(&tutti_graph::ParamSet),
) -> Vec<f64> {
    let frames = 64;
    let (mut rig, controls) = BlockRig::<f64>::with_format(node, RATE, frames);
    let mut set = Some(set);
    for b in 0..blocks {
        if b == 1
            && let Some(set) = set.take()
        {
            set(&controls);
        }
        for (c, v) in inputs.iter().enumerate() {
            rig.inputs_mut().plane_mut(c).samples_mut().fill(*v);
        }
        rig.block();
    }
    let out = rig.output();
    (0..out.channels())
        .map(|c| out.plane(c).samples()[frames - 1])
        .collect()
}

/// A source at 90 degrees left, held, is in the `y` channel alone at first order, and the
/// controls move it: the channels settle on the harmonics of the new direction.
#[test]
fn the_encoder_scales_the_source_by_the_harmonics_of_its_direction() {
    let order = AmbisonicOrder::of(3);
    let node = AmbisonicEncoderNode::<f64>::try_from(
        AmbisonicEncoderConfig::new(ambix(order)).direction(aim(90.0, 0.0)),
    )
    .unwrap();
    let got = run(node, &[0.5], 3, |_| {});
    for (g, h) in got.iter().zip(harmonics(
        order,
        HarmonicNormalization::Sn3d,
        aim(90.0, 0.0),
    )) {
        assert!((g - 0.5 * h).abs() < 1e-12);
    }
    let node =
        AmbisonicEncoderNode::<f64>::try_from(AmbisonicEncoderConfig::new(ambix(order))).unwrap();
    let moved = run(node, &[0.5], 3, |controls| {
        assert!(controls.set(UnitParam::Azimuth, -120.0));
        assert!(controls.set(UnitParam::Elevation, 35.0));
    });
    for (g, h) in moved.iter().zip(harmonics(
        order,
        HarmonicNormalization::Sn3d,
        aim(-120.0, 35.0),
    )) {
        assert!((g - 0.5 * h).abs() < 1e-12);
    }
}

/// N3D channels are SN3D's times `sqrt(2 n + 1)` of their degree.
#[test]
fn the_n3d_encoder_is_the_sn3d_one_scaled_by_degree() {
    let make = |n| {
        AmbisonicEncoderNode::<f64>::try_from(
            AmbisonicEncoderConfig::new(bus(THIRD, n)).direction(aim(37.0, 25.0)),
        )
        .unwrap()
    };
    let sn3d = run(make(HarmonicNormalization::Sn3d), &[1.0], 2, |_| {});
    let n3d = run(make(HarmonicNormalization::N3d), &[1.0], 2, |_| {});
    for (acn, (a, b)) in sn3d.iter().zip(&n3d).enumerate() {
        let degree = (acn as f64).sqrt().floor();
        assert!((b - a * (2.0 * degree + 1.0).sqrt()).abs() < 1e-12);
    }
}

/// A rotator whose front is carried to 90 degrees turns the encoding of a source ahead
/// into the encoding of a source at 90 degrees.
#[test]
fn the_rotator_carries_a_source_ahead_to_the_front_it_is_set_to() {
    let order = AmbisonicOrder::of(2);
    let ahead = harmonics(order, HarmonicNormalization::Sn3d, aim(0.0, 0.0));
    let node =
        AmbisonicRotatorNode::<f64>::try_from(AmbisonicRotatorConfig::new(ambix(order))).unwrap();
    let got = run(node, &ahead, 3, |controls| {
        assert!(controls.set(UnitParam::Azimuth, 90.0));
        assert!(controls.set(UnitParam::Elevation, 20.0));
    });
    let expected = harmonics(order, HarmonicNormalization::Sn3d, aim(90.0, 20.0));
    for (g, e) in got.iter().zip(&expected) {
        assert!((g - e).abs() < 1e-12);
    }
}

/// The configured roll turns the field about the front: a source above the front rolls to
/// its left by a quarter turn.
#[test]
fn the_rotator_rolls_about_the_front_by_the_configured_arc() {
    let order = AmbisonicOrder::of(1);
    let above = harmonics(order, HarmonicNormalization::Sn3d, aim(0.0, 90.0));
    let node = AmbisonicRotatorNode::<f64>::try_from(
        AmbisonicRotatorConfig::new(ambix(order)).roll(ArcDegrees::new(90.0)),
    )
    .unwrap();
    let got = run(node, &above, 2, |_| {});
    // A positive roll takes `z` toward `-y`: the source above ends on the right.
    let expected = harmonics(order, HarmonicNormalization::Sn3d, aim(-90.0, 0.0));
    for (g, e) in got.iter().zip(&expected) {
        assert!((g - e).abs() < 1e-12);
    }
}

/// The shapes: the encoder is mono in and `(order + 1)^2` out, the rotator keeps the
/// width.
#[test]
fn the_shapes_are_mono_to_the_order_and_through() {
    let encoder = prepared(
        AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(ambix(THIRD))).unwrap(),
    );
    let shape = encoder.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (1, 16)
    );
    assert_eq!(shape.tail, Tail::None);
    let rotator = prepared(
        AmbisonicRotatorNode::<f32>::try_from(AmbisonicRotatorConfig::new(ambix(
            AmbisonicOrder::of(2),
        )))
        .unwrap(),
    );
    let shape = rotator.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (9, 9)
    );
}

#[test]
fn forks_share_no_cell() {
    assert_param_fork(
        AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(ambix(THIRD))).unwrap(),
    );
    assert_param_fork(
        AmbisonicRotatorNode::<f32>::try_from(AmbisonicRotatorConfig::new(ambix(THIRD))).unwrap(),
    );
}

/// The nodes run in ACN with SN3D or N3D: a bus that is no ambisonic one, or an ambisonic
/// one of another order or scaling, is refused by name.
#[test]
fn the_nodes_refuse_a_bus_that_is_not_acn_in_sn3d_or_n3d() {
    let fuma = BusTopology::ambisonic(AmbisonicFormat::fuma(AmbisonicOrder::of(1)));
    let max_n = BusTopology::ambisonic(AmbisonicFormat::new(
        AmbisonicOrder::of(1),
        AmbisonicOrdering::Acn,
        AmbisonicNormalization::MaxN,
    ));
    for bus in [fuma, max_n] {
        assert!(matches!(
            AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(bus)),
            Err(AmbisonicError::Unsupported(_))
        ));
        assert!(matches!(
            AmbisonicRotatorNode::<f32>::try_from(AmbisonicRotatorConfig::new(bus)),
            Err(AmbisonicError::Unsupported(_))
        ));
    }
    assert!(matches!(
        AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(Bed::Quad)),
        Err(AmbisonicError::NotAmbisonic)
    ));
}
