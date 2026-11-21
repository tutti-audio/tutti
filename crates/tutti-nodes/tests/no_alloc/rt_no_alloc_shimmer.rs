//! Regression gate: the shimmer reverb must not allocate per block, held and with its shift and
//! send moved across their ranges between blocks, a block of 480 frames among chunks of 32 and
//! hops of 512.

use tutti_graph::contract::BlockRig;
use tutti_nodes::{ShimmerConfig, ShimmerNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 240 blocks under `RtScope`, the shift and the send moved
/// before each.
#[test]
fn shimmer_process_is_allocation_free_with_the_shift_and_send_moving() {
    let node = ShimmerNode::<f32>::from(ShimmerConfig::new());
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 480);
    for ch in rig.inputs_mut().iter_mut() {
        for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
            *x = ((i as f32) * 0.07).sin() * 0.8;
        }
    }
    let cents = |i: usize| match i % 4 {
        0 => -2_400.0,
        1 => 700.0,
        2 => 2_400.0,
        _ => 0.0,
    };
    let send = |i: usize| [0.0, 0.5, 5.0, 0.99][i % 4];
    for i in 0..16 {
        controls.set(UnitParam::Pitch, cents(i));
        controls.set(UnitParam::Feedback, send(i));
        rig.block();
    }
    for i in 0..240 {
        controls.set(UnitParam::Pitch, cents(i));
        controls.set(UnitParam::Feedback, send(i));
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
}
