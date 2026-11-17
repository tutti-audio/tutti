//! The cut filter does not allocate per block, held and with its cutoff moved between
//! blocks (a design into the held scratch and the coefficient glide).

use tutti_graph::contract::BlockRig;
use tutti_nodes::{ButterworthOrder, CutAlignment, CutFilterConfig, CutFilterNode, CutResponse};
use tutti_types::{ChannelLayout, SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the cutoff moved before each.
#[test]
fn cut_filter_process_is_allocation_free_with_the_cutoff_moving() {
    let config = CutFilterConfig::new(
        ChannelLayout::STEREO,
        CutResponse::LowPass,
        CutAlignment::LinkwitzRiley(ButterworthOrder::of(8)),
    );
    let (mut rig, controls) = BlockRig::new(CutFilterNode::from(config), SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 8) % 2 == 0 { 0.5 } else { -0.5 };
            *x = 0.25 + side * (c as f32 + 1.0) * 0.5;
        }
    }
    let cutoff = |i: usize| if i.is_multiple_of(2) { 80.0 } else { 6_000.0 };
    for i in 0..16 {
        controls.set(UnitParam::Cutoff, cutoff(i));
        rig.block();
    }
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            controls.set(UnitParam::Cutoff, cutoff(i));
            rig.block();
        }
    });
}
