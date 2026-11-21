//! Regression gate: tremolo, auto-pan and the ring modulator must not allocate per block,
//! held and with their controls moved between blocks.

use tutti_buffers::ChannelLayout;
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    AutoPanConfig, AutoPanNode, RingModConfig, RingModNode, TremoloConfig, TremoloNode,
};
use tutti_units::{SampleRate, UnitParam};

/// `node` alone through `BlockRig`: a signal on every channel, 16 warm-up blocks, then 2 000
/// blocks under `assert_no_alloc`, `between` run before each.
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
fn tremolo_process_is_allocation_free_with_the_controls_moving() {
    gate(
        TremoloNode::from(TremoloConfig::new(ChannelLayout::STEREO)),
        |controls, i| {
            controls.set(
                UnitParam::Rate,
                if i.is_multiple_of(2) { 3.0 } else { 40.0 },
            );
            controls.set(
                UnitParam::Depth,
                if i.is_multiple_of(3) { 0.2 } else { 1.0 },
            );
        },
    );
}

#[test]
fn auto_pan_process_is_allocation_free_with_the_controls_moving() {
    gate(AutoPanNode::from(AutoPanConfig::new()), |controls, i| {
        controls.set(
            UnitParam::Rate,
            if i.is_multiple_of(2) { 3.0 } else { 40.0 },
        );
        controls.set(
            UnitParam::Depth,
            if i.is_multiple_of(3) { 0.2 } else { 1.0 },
        );
    });
}

#[test]
fn ring_mod_process_is_allocation_free_with_the_controls_moving() {
    gate(
        RingModNode::from(RingModConfig::new(ChannelLayout::STEREO)),
        |controls, i| {
            let hz = if i.is_multiple_of(2) { 220.0 } else { 1_700.0 };
            controls.set(UnitParam::Frequency, hz);
        },
    );
}
