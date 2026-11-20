//! Regression gate: the utility nodes (polarity, mid/side, stereo width) must
//! not allocate per block, held and with a control moved between blocks.

use tutti_buffers::{ChannelIndex, ChannelLayout};
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    MidSideConfig, MidSideNode, NoiseColour, NoiseConfig, NoiseNode, PolarityConfig, PolarityNode,
    StereoWidthConfig, StereoWidthNode,
};
use tutti_units::{SampleRate, UnitParam};

/// `node` alone through `BlockRig`: an offset, asymmetric pair of channels, 16 warm-up
/// blocks, then 2 000 blocks under `assert_no_alloc`, `between` run before each.
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            *x = 0.25 + if (i / 8).is_multiple_of(2) { 0.5 } else { -0.5 } * (c as f32 + 1.0) * 0.5;
        }
    }
    for i in 0..16 {
        between(&controls, i);
        rig.block();
    }
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            between(&controls, i);
            rig.block();
        }
    });
}

#[test]
fn polarity_process_is_allocation_free() {
    let config = PolarityConfig::new(ChannelLayout::STEREO).only([ChannelIndex::of(1)]);
    gate(PolarityNode::try_from(config).unwrap(), |_, _| {});
}

#[test]
fn mid_side_process_is_allocation_free() {
    gate(MidSideNode::from(MidSideConfig::encode()), |_, _| {});
    gate(MidSideNode::from(MidSideConfig::decode()), |_, _| {});
}

#[test]
fn stereo_width_process_is_allocation_free_with_the_width_moving() {
    gate(
        StereoWidthNode::<f32>::from(StereoWidthConfig::new()),
        |controls, i| {
            let w = if i.is_multiple_of(2) { 0.2 } else { 1.8 };
            controls.set(UnitParam::StereoWidth, w);
        },
    );
}

#[test]
fn noise_process_is_allocation_free_in_every_colour_with_the_level_moving() {
    for colour in [NoiseColour::White, NoiseColour::Pink, NoiseColour::Brown] {
        gate(
            NoiseNode::<f32>::from(NoiseConfig::new(colour).seed(1)),
            |controls, i| {
                controls.set(
                    UnitParam::Volume,
                    if i.is_multiple_of(2) { 0.2 } else { 0.9 },
                );
            },
        );
    }
}
