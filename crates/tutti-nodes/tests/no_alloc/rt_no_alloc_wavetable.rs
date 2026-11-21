//! Regression gate: the wavetable node must not allocate per block, with its frequency swept
//! across the mip levels and its frame position moved between blocks.

use core::num::NonZeroU16;
use tutti_buffers::Mono;
use tutti_dsp::Wavetable;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{WavetableConfig, WavetableNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the frequency and the
/// position moved before each.
#[test]
fn wavetable_process_is_allocation_free_with_the_frequency_and_position_moving() {
    let saw: Vec<f32> = (0..1024).map(|i| i as f32 / 512.0 - 1.0).collect();
    let square: Vec<f32> = (0..1024)
        .map(|i| if i < 512 { 1.0 } else { -1.0 })
        .collect();
    let table = Wavetable::new(&[Mono::new(&saw), Mono::new(&square)]).expect("two frames");
    let node = WavetableNode::<f32>::from(WavetableConfig::new(NonZeroU16::new(2).unwrap(), table));
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    let hz = |i: usize| 30.0 * 1.0037f64.powi((i % 1_900) as i32);
    let at = |i: usize| (i % 7) as f64 / 6.0;
    for i in 0..16 {
        controls.set(UnitParam::Frequency, hz(i));
        controls.set(UnitParam::Position, at(i));
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            controls.set(UnitParam::Frequency, hz(i));
            controls.set(UnitParam::Position, at(i));
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
