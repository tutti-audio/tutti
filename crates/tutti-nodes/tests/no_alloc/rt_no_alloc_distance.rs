//! Regression gate: the distance node must not allocate per block, held and with its distance
//! moved across the line between blocks, with air absorption on.

use core::num::NonZeroU16;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{DistanceConfig, DistanceNode};
use tutti_units::{Hz, Metres, SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the distance moved before each.
#[test]
fn distance_process_is_allocation_free_with_the_distance_moving() {
    let node = DistanceNode::<f32>::from(
        DistanceConfig::new(NonZeroU16::new(2).unwrap(), Metres::of(60.0))
            .air_absorption(Hz::new(6000.0)),
    );
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 480);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
            *x = ((i as f32) * 0.07 * (c as f32 + 1.0)).sin() * 0.8;
        }
    }
    let metres = |i: usize| match i % 4 {
        0 => 0.5,
        1 => 12.0,
        2 => 55.0,
        _ => 3.0,
    };
    for i in 0..16 {
        controls.set(UnitParam::Distance, metres(i));
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            controls.set(UnitParam::Distance, metres(i));
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
