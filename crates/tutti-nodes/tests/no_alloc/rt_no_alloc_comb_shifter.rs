//! Regression gate: the comb filter and the frequency shifter must not allocate per block,
//! held and with their controls moved between blocks.

use tutti_buffers::ChannelLayout;
use tutti_dsp::CombDelay;
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    CombFilterConfig, CombFilterNode, CombForm, FrequencyShifterConfig, FrequencyShifterNode,
};
use tutti_units::{SampleRate, Seconds, UnitParam};

/// `node` alone through `BlockRig`: a signal on every channel, 16 warm-up blocks, then 240
/// blocks under `RtScope`, `between` run before each.
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
    for i in 0..240 {
        between(&controls, i);
        rig.block();
    }
}

#[test]
fn comb_filter_process_is_allocation_free_with_the_coefficient_moving() {
    for form in [CombForm::Feedforward, CombForm::Feedback] {
        gate(
            CombFilterNode::from(CombFilterConfig::new(
                ChannelLayout::STEREO,
                form,
                CombDelay::of(Seconds::new(0.003)),
            )),
            |controls, i| {
                controls.set(
                    UnitParam::Feedback,
                    if i.is_multiple_of(2) { 0.2 } else { 0.9 },
                );
            },
        );
    }
}

#[test]
fn frequency_shifter_process_is_allocation_free_with_the_shift_moving() {
    gate(
        FrequencyShifterNode::from(FrequencyShifterConfig::new(ChannelLayout::STEREO)),
        |controls, i| {
            let hz = if i.is_multiple_of(2) { -220.0 } else { 1_700.0 };
            controls.set(UnitParam::Frequency, hz);
        },
    );
}
