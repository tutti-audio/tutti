use tutti_dsp::Harmonics;
use tutti_graph::contract::{BlockRig, assert_param_fork};
use tutti_graph::{IntoNode, Node};
use tutti_units::{SampleRate, Tail, UnitParam};

use super::*;
use crate::test_support::prepared;

const THIRD: AmbisonicOrder = AmbisonicOrder::MAX;
const RATE: SampleRate = SampleRate::SR_48K;

/// The AmbiX bus of `order`: ACN, SN3D.
fn ambix(order: AmbisonicOrder) -> ChannelTopology {
    ChannelTopology::Ambisonic(AmbisonicFormat::ambix(order))
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
    let node = AmbisonicEncoderNode::<f64>::from(
        AmbisonicEncoderConfig::new(order, HarmonicNormalization::Sn3d).direction(aim(90.0, 0.0)),
    );
    let got = run(node, &[0.5], 3, |_| {});
    for (g, h) in got.iter().zip(harmonics(
        order,
        HarmonicNormalization::Sn3d,
        aim(90.0, 0.0),
    )) {
        assert!((g - 0.5 * h).abs() < 1e-12);
    }
    let node = AmbisonicEncoderNode::<f64>::from(AmbisonicEncoderConfig::new(
        order,
        HarmonicNormalization::Sn3d,
    ));
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
        AmbisonicEncoderNode::<f64>::from(
            AmbisonicEncoderConfig::new(THIRD, n).direction(aim(37.0, 25.0)),
        )
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
    let node = AmbisonicRotatorNode::<f64>::from(AmbisonicRotatorConfig::new(
        order,
        HarmonicNormalization::Sn3d,
    ));
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
    let node = AmbisonicRotatorNode::<f64>::from(
        AmbisonicRotatorConfig::new(order, HarmonicNormalization::Sn3d).roll(ArcDegrees::new(90.0)),
    );
    let got = run(node, &above, 2, |_| {});
    // A positive roll takes `z` toward `-y`: the source above ends on the right.
    let expected = harmonics(order, HarmonicNormalization::Sn3d, aim(-90.0, 0.0));
    for (g, e) in got.iter().zip(&expected) {
        assert!((g - e).abs() < 1e-12);
    }
}

/// A decoder to a quad bed by sampling: the four feeds of a plane wave sum to the source
/// (a square is a design of the first harmonics), so the level of a source does not depend
/// on where it sits.
#[test]
fn the_sampling_decoder_to_a_quad_keeps_the_sum_of_the_feeds_at_the_source() {
    let order = AmbisonicOrder::of(1);
    let node = AmbisonicDecoderNode::<f64>::try_from(
        AmbisonicDecoderConfig::new(Bed::Quad)
            .input(ambix(order))
            .decoding(Decoding::Sampling),
    )
    .unwrap();
    let wave = harmonics(order, HarmonicNormalization::Sn3d, aim(30.0, 20.0));
    let got = run(node, &wave, 2, |_| {});
    assert_eq!(got.len(), 4);
    assert!((got.iter().sum::<f64>() - 1.0).abs() < 1e-12);
}

/// The LFE of a 5.1 bed is never fed; the other five channels sound.
#[test]
fn the_decoder_leaves_the_lfe_silent() {
    let order = AmbisonicOrder::of(3);
    let node = AmbisonicDecoderNode::<f64>::try_from(
        AmbisonicDecoderConfig::new(Bed::Surround51).input(ambix(order)),
    )
    .unwrap();
    let wave = harmonics(order, HarmonicNormalization::Sn3d, aim(10.0, 0.0));
    let got = run(node, &wave, 2, |_| {});
    assert_eq!(got.len(), 6);
    let lfe = Bed::Surround51
        .topology()
        .positions()
        .unwrap()
        .iter()
        .position(|&s| s == tutti_buffers::Speaker::LowFrequency)
        .unwrap();
    assert_eq!(got[lfe], 0.0);
    assert!(
        got.iter()
            .enumerate()
            .any(|(c, g)| c != lfe && g.abs() > 0.1)
    );
}

/// A decoder reads N3D channels as SN3D ones: the same sound field in the two scalings
/// decodes to the same feeds.
#[test]
fn a_decoder_reads_n3d_and_sn3d_encodings_of_one_field_to_the_same_feeds() {
    let order = AmbisonicOrder::of(2);
    let d = aim(-70.0, 15.0);
    let feeds = |n| {
        let node = AmbisonicDecoderNode::<f64>::try_from(
            AmbisonicDecoderConfig::new(Bed::Surround50).input(bus(order, n)),
        )
        .unwrap();
        run(node, &harmonics(order, n, d), 2, |_| {})
    };
    let a = feeds(HarmonicNormalization::Sn3d);
    let b = feeds(HarmonicNormalization::N3d);
    for (a, b) in a.iter().zip(&b) {
        assert!((a - b).abs() < 1e-12);
    }
}

#[test]
fn a_decoder_refuses_a_bus_that_is_not_a_bed_and_a_bed_with_one_speaker() {
    let order = AmbisonicOrder::of(1);
    let unassigned =
        ChannelTopology::unassigned(tutti_buffers::ChannelLayout::from_count(3)).unwrap();
    assert!(matches!(
        AmbisonicDecoderNode::<f32>::try_from(
            AmbisonicDecoderConfig::new(unassigned).input(ambix(order))
        ),
        Err(AmbisonicError::NotABed)
    ));
    assert!(matches!(
        AmbisonicDecoderNode::<f32>::try_from(
            AmbisonicDecoderConfig::new(Bed::Mono).input(ambix(order))
        ),
        Err(AmbisonicError::Geometry(_))
    ));
}

/// The shapes: the encoder is mono in and `(order + 1)^2` out, the rotator keeps the
/// width, and the decoder is the order's channels in and the bed's out.
#[test]
fn the_shapes_are_mono_to_the_order_through_and_the_order_to_the_bed() {
    let encoder = prepared(AmbisonicEncoderNode::<f32>::from(
        AmbisonicEncoderConfig::new(THIRD, HarmonicNormalization::Sn3d),
    ));
    let shape = encoder.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (1, 16)
    );
    assert_eq!(shape.tail, Tail::None);
    let rotator = prepared(AmbisonicRotatorNode::<f32>::from(
        AmbisonicRotatorConfig::new(AmbisonicOrder::of(2), HarmonicNormalization::Sn3d),
    ));
    let shape = rotator.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (9, 9)
    );
    let decoder = prepared(
        AmbisonicDecoderNode::<f32>::try_from(
            AmbisonicDecoderConfig::new(Bed::Surround51).input(ambix(AmbisonicOrder::of(1))),
        )
        .unwrap(),
    );
    let shape = decoder.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (4, 6)
    );
}

#[test]
fn forks_share_no_cell() {
    assert_param_fork(AmbisonicEncoderNode::<f32>::from(
        AmbisonicEncoderConfig::new(THIRD, HarmonicNormalization::Sn3d),
    ));
    assert_param_fork(AmbisonicRotatorNode::<f32>::from(
        AmbisonicRotatorConfig::new(THIRD, HarmonicNormalization::Sn3d),
    ));
}

/// The nodes run in ACN with SN3D or N3D: a bus that is no ambisonic one, or an ambisonic
/// one of another order or scaling, is refused by name.
#[test]
fn the_decoder_refuses_a_bus_that_is_not_acn_in_sn3d_or_n3d() {
    let fuma = ChannelTopology::Ambisonic(AmbisonicFormat::fuma(AmbisonicOrder::of(1)));
    let max_n = ChannelTopology::Ambisonic(AmbisonicFormat::new(
        AmbisonicOrder::of(1),
        AmbisonicOrdering::Acn,
        AmbisonicNormalization::MaxN,
    ));
    for bus in [fuma, max_n] {
        assert!(matches!(
            AmbisonicDecoderNode::<f32>::try_from(
                AmbisonicDecoderConfig::new(Bed::Quad).input(bus)
            ),
            Err(AmbisonicError::Unsupported(_))
        ));
    }
}

/// Each channel of the bed carries the harmonics sampled at its own speaker: feed `c` of a
/// plane wave is `(1 / L) sum_n (2 n + 1) Y_n(speaker c) Y_n(source)` over the `L` placed
/// speakers, with the LFE skipped in the count and its channel left out of the map.
#[test]
fn the_sampling_decoder_feeds_each_channel_the_harmonics_at_its_own_speaker() {
    let order = AmbisonicOrder::of(2);
    let bed = Bed::Surround51;
    let node = AmbisonicDecoderNode::<f64>::try_from(
        AmbisonicDecoderConfig::new(bed)
            .input(ambix(order))
            .decoding(Decoding::Sampling),
    )
    .unwrap();
    let source = harmonics(order, HarmonicNormalization::Sn3d, aim(30.0, 20.0));
    let got = run(node, &source, 2, |_| {});
    let speakers = bed.topology().positions().unwrap().to_vec();
    let placed = speakers
        .iter()
        .filter(|&&s| bed.direction(s).is_some())
        .count();
    for (channel, speaker) in speakers.iter().enumerate() {
        let expected = bed.direction(*speaker).map_or(0.0, |d| {
            let at_speaker = harmonics(order, HarmonicNormalization::Sn3d, d);
            let sum: f64 = at_speaker
                .iter()
                .zip(&source)
                .enumerate()
                .map(|(acn, (y, s))| {
                    let degree = (acn as f64).sqrt().floor();
                    (2.0 * degree + 1.0) * y * s
                })
                .sum();
            sum / placed as f64
        });
        assert!((got[channel] - expected).abs() < 1e-12, "channel {channel}");
    }
}
