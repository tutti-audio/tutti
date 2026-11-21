//! Regression gate: the pitch shifter must not allocate per block, held and with its pitch moved
//! across the whole range between blocks, a block of 480 frames among hops of 512.

use core::num::NonZeroU16;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{PitchShifterConfig, PitchShifterNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the pitch moved before each.
#[test]
fn pitch_shifter_process_is_allocation_free_with_the_pitch_moving() {
    let node = PitchShifterNode::<f32>::from(PitchShifterConfig::new(NonZeroU16::new(2).unwrap()));
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 480);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
            *x = ((i as f32) * 0.07 * (c as f32 + 1.0)).sin() * 0.8;
        }
    }
    let cents = |i: usize| match i % 4 {
        0 => -2_400.0,
        1 => 700.0,
        2 => 2_400.0,
        _ => 0.0,
    };
    for i in 0..16 {
        controls.set(UnitParam::Pitch, cents(i));
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            controls.set(UnitParam::Pitch, cents(i));
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
