//! The leveller does not allocate per block, with its target and gate moved between blocks and
//! its window turning over.

use tutti_buffers::ChannelTopology;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{LevellerConfig, LevellerNode};
use tutti_units::{SampleRate, UnitAddr, UnitParam};

fn feed(rig: &mut BlockRig) {
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 8) % 2 == 0 { 0.05 } else { -0.05 };
            *x = side * (c as f32 + 1.0);
        }
    }
}

#[test]
fn leveller_process_is_allocation_free_with_the_controls_moving() {
    let config = LevellerConfig::new(ChannelTopology::STEREO);
    let (mut rig, controls) = BlockRig::new(
        LevellerNode::<f32>::try_from(config).unwrap(),
        SampleRate::SR_48K,
        64,
    );
    feed(&mut rig);
    let target = |i: usize| if i.is_multiple_of(2) { -30.0 } else { -16.0 };
    for i in 0..16 {
        controls.set(UnitAddr::new(UnitParam::Threshold, 0), target(i));
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
    // 2 000 blocks of 64 frames cross the 4 800-frame hop 26 times.
    for i in 0..256 {
        controls.set(UnitAddr::new(UnitParam::Threshold, 0), target(i));
        controls.set(UnitAddr::new(UnitParam::Threshold, 1), target(i + 1) - 30.0);
        rig.block();
    }
}
