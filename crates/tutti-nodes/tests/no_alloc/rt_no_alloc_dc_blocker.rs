//! Regression gate: the DC blocker must not allocate per block, held and with its corner
//! moved between blocks (the pole ramp).

use tutti_buffers::ChannelLayout;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{DcBlockerConfig, DcBlockerNode};
use tutti_units::{SampleRate, UnitParam};

/// 16 warm-up blocks, then 240 blocks under `RtScope`, the corner moved before each.
#[test]
fn dc_blocker_process_is_allocation_free_with_the_corner_moving() {
    let node = DcBlockerNode::<f32>::from(DcBlockerConfig::new(ChannelLayout::STEREO));
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 8) % 2 == 0 { 0.5 } else { -0.5 };
            *x = 0.25 + side * (c as f32 + 1.0) * 0.5;
        }
    }
    let corner = |i: usize| if i.is_multiple_of(2) { 5.0 } else { 90.0 };
    for i in 0..16 {
        controls.set(UnitParam::Cutoff, corner(i));
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
        controls.set(UnitParam::Cutoff, corner(i));
        rig.block();
    }
}
