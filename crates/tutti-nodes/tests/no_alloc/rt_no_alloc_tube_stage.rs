//! Regression gate: the tube stage must not allocate per block, at every oversampling
//! factor, with its drive moved between blocks (the ramp and the design).

use tutti_buffers::ChannelLayout;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{OversamplingFactor, TubeStageConfig, TubeStageNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 1 000 blocks under `assert_no_alloc`, the drive moved before each.
#[test]
fn tube_stage_process_is_allocation_free_with_the_drive_moving() {
    for factor in [
        OversamplingFactor::X2,
        OversamplingFactor::X4,
        OversamplingFactor::X8,
    ] {
        let node = TubeStageNode::<f32>::from(
            TubeStageConfig::new(ChannelLayout::STEREO).oversampling(factor),
        );
        let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
        for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
            for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
                *x = 0.9 * (((i + 7 * c) % 13) as f32 / 6.0 - 1.0);
            }
        }
        let drive = |i: usize| if i.is_multiple_of(3) { -6.0 } else { 24.0 };
        for i in 0..16 {
            controls.set(UnitParam::GainDb, drive(i));
            rig.block();
        }
        assert!(
            (0..2).all(|c| rig
                .output()
                .plane(c)
                .samples()
                .iter()
                .any(|s| s.abs() > 1e-6)),
            "{factor:?}: a silent channel; the gate would walk no solver"
        );
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..1_000 {
                controls.set(UnitParam::GainDb, drive(i));
                rig.block();
            }
        });
    }
}
