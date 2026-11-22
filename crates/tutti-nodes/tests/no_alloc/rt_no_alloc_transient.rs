//! The transient shaper does not allocate per block, with its amounts moved between blocks.

use tutti_buffers::ChannelLayout;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{TransientShaperConfig, TransientShaperNode};
use tutti_units::{Db, SampleRate, UnitAddr, UnitParam};

fn feed(rig: &mut BlockRig) {
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 8) % 2 == 0 { 0.5 } else { -0.5 };
            *x = side * (c as f32 + 1.0) * 0.5;
        }
    }
}

#[test]
fn transient_shaper_process_is_allocation_free_with_the_amounts_moving() {
    let config = TransientShaperConfig::new(ChannelLayout::STEREO).attack(Db::new(6.0));
    let (mut rig, controls) = BlockRig::new(
        TransientShaperNode::<f32>::from(config),
        SampleRate::SR_48K,
        64,
    );
    feed(&mut rig);
    let amount = |i: usize| if i.is_multiple_of(2) { -12.0 } else { 12.0 };
    for i in 0..16 {
        controls.set(UnitAddr::new(UnitParam::GainDb, 0), amount(i));
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
    for i in 0..256 {
        controls.set(UnitAddr::new(UnitParam::GainDb, 0), amount(i));
        controls.set(UnitAddr::new(UnitParam::GainDb, 1), amount(i + 1));
        rig.block();
    }
}
