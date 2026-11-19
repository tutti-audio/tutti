//! Regression gate: the bit crusher must not allocate per block, held and with its depth
//! and hold moved between blocks.

use tutti_buffers::ChannelLayout;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{BitCrusherConfig, BitCrusherNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, both controls moved before
/// each.
#[test]
fn bit_crusher_process_is_allocation_free_with_the_controls_moving() {
    let node = BitCrusherNode::<f32>::from(BitCrusherConfig::new(ChannelLayout::STEREO));
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            *x = ((i as f32) * 0.07 * (c as f32 + 1.0)).sin() * 0.8;
        }
    }
    let bits = |i: usize| if i.is_multiple_of(2) { 3.0 } else { 11.5 };
    let hold = |i: usize| 1.0 + (i % 5) as f32 * 1.3;
    for i in 0..16 {
        controls.set(UnitParam::BitDepth, bits(i));
        controls.set(UnitParam::HoldFactor, f64::from(hold(i)));
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
            controls.set(UnitParam::BitDepth, bits(i));
            controls.set(UnitParam::HoldFactor, f64::from(hold(i)));
            rig.block();
        }
    });
}
