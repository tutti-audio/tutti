//! Regression gate: the plucked string and the waveguide string must not allocate
//! per block, held and with the pitch and the decay moved between blocks (a pitch move retunes
//! the loop on the audio thread).

use tutti_buffers::ChannelLayout;
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    KarplusStrongConfig, KarplusStrongNode, WaveguideStringConfig, WaveguideStringNode,
};
use tutti_units::{SampleRate, UnitParam};

/// `node` alone through `BlockRig`: a pulse train on every channel, 16 warm-up blocks, then 240
/// blocks under `RtScope`, `between` run before each.
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for ch in rig.inputs_mut().iter_mut() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            *x = if i.is_multiple_of(32) { 1.0 } else { 0.0 };
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
    for i in 0..240 {
        between(&controls, i);
        rig.block();
    }
}

#[test]
fn karplus_strong_process_is_allocation_free_with_the_pitch_and_decay_moving() {
    gate(
        KarplusStrongNode::from(KarplusStrongConfig::new(ChannelLayout::STEREO)),
        |controls, i| {
            controls.set(
                UnitParam::Frequency,
                if i.is_multiple_of(2) { 220.0 } else { 880.0 },
            );
            controls.set(
                UnitParam::Decay,
                if i.is_multiple_of(3) { 0.2 } else { 1.5 },
            );
        },
    );
}

#[test]
fn waveguide_string_process_is_allocation_free_with_the_pitch_and_decay_moving() {
    gate(
        WaveguideStringNode::from(WaveguideStringConfig::new(ChannelLayout::STEREO)),
        |controls, i| {
            controls.set(
                UnitParam::Frequency,
                if i.is_multiple_of(2) { 220.0 } else { 880.0 },
            );
            controls.set(
                UnitParam::Decay,
                if i.is_multiple_of(3) { 0.2 } else { 1.5 },
            );
        },
    );
}
