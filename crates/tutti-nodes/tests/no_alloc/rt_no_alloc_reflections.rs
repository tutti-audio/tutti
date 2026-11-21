//! Regression gate: the early reflections and the decorrelator must not allocate per block.

use tutti_buffers::ChannelLayout;
use tutti_dsp::{Inline, Tap, TapSet};
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    DecorrelatorConfig, DecorrelatorNode, EarlyReflectionsConfig, EarlyReflectionsNode,
};
use tutti_units::{Gain, SampleRate, SampleSpan};

/// `node` alone through `BlockRig`: a signal on every channel, 16 warm-up blocks, then 240
/// blocks under `RtScope`.
fn gate<N: IntoNode>(node: N) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            *x = 0.25 + if (i / 8).is_multiple_of(2) { 0.5 } else { -0.5 } * (c as f32 + 1.0) * 0.5;
        }
    }
    for _ in 0..16 {
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
    for _ in 0..240 {
        rig.block();
    }
}

#[test]
fn early_reflections_process_is_allocation_free() {
    let tap = |delay: f64, gain: f64, channel: u8| Tap {
        delay: SampleSpan::new(delay).unwrap(),
        gain: Gain::new(gain),
        channel,
    };
    let taps = TapSet {
        taps: Inline::from_slice(&[
            tap(17.0, 0.6, 0),
            tap(101.5, -0.4, 1),
            tap(333.25, 0.3, 0),
            tap(1_000.0, -0.2, 1),
        ])
        .unwrap(),
    };
    gate(
        EarlyReflectionsNode::<f32>::try_from(EarlyReflectionsConfig::new(
            ChannelLayout::STEREO,
            taps,
        ))
        .unwrap(),
    );
}

#[test]
fn decorrelator_process_is_allocation_free() {
    gate(DecorrelatorNode::<f32>::from(DecorrelatorConfig::new(
        ChannelLayout::STEREO,
    )));
}
