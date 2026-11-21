//! Regression gate: the granular node must not allocate per block, with its density, pitch, grain
//! length and jitters moving and grains dropped for want of a slot.

use std::sync::Arc;
use tutti_buffers::{ChannelLayout, Interleaved, Wave};
use tutti_dsp::MaxGrains;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{GranularConfig, GranularNode};
use tutti_units::{Normalized, SampleRate, UnitAddr, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, every control moved before each.
#[test]
fn granular_process_is_allocation_free_with_the_controls_moving() {
    let mut source = Wave::<f32>::new(2, SampleRate::SR_48K);
    let frames: Vec<f32> = (0..2 * 9_600).map(|i| (i as f32 * 0.05).sin()).collect();
    source
        .append(Interleaved::new(&frames, ChannelLayout::STEREO).unwrap())
        .unwrap();
    // Eight slots under up to 400 grains a second of 50 ms: slots run out.
    let node = GranularNode::<f32>::try_from(
        GranularConfig::new(ChannelLayout::STEREO, Arc::new(source))
            .max_grains(MaxGrains::of(8))
            .seed(11)
            .position_jitter(Normalized::of(0.3))
            .timing_jitter(Normalized::of(0.5)),
    )
    .unwrap();
    let position = node.position();
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 480);
    let move_controls = |i: usize| {
        let k = (i % 4) as f64;
        controls.set(UnitParam::Rate, 20.0 + 130.0 * k);
        controls.set(UnitParam::Pitch, -700.0 + 500.0 * k);
        controls.set(UnitParam::GrainLength, 0.005 + 0.015 * k);
        controls.set(UnitAddr::new(UnitParam::Jitter, 0), 0.1 * k);
        controls.set(UnitAddr::new(UnitParam::Jitter, 1), 0.2 * k);
        position.store(Normalized::of(0.2 * k));
    };
    for i in 0..16 {
        move_controls(i);
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            move_controls(i);
            rig.block();
        }
    });
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
}
