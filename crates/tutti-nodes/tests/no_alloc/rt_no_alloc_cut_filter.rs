//! The cut filter does not allocate per block, held and with its cutoff moved between
//! blocks (a design into the held scratch and the coefficient ramp).

use tutti_buffers::ChannelLayout;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    ButterworthOrder, CutAlignment, CutFilterConfig, CutFilterNode, CutResponse,
    EllipticTolerances, PassbandRipple, StopbandAttenuation,
};
use tutti_units::{Db, SampleRate, UnitParam};

/// 16 warm-up blocks, then 240 blocks under `RtScope`, the cutoff moved before each.
#[test]
fn cut_filter_process_is_allocation_free_with_the_cutoff_moving() {
    cut_filter_is_allocation_free(CutAlignment::LinkwitzRiley(ButterworthOrder::of(8)));
}

/// The same for the elliptic alignment: the Jacobi functions of each design run on the stack.
#[test]
fn elliptic_cut_filter_process_is_allocation_free_with_the_cutoff_moving() {
    cut_filter_is_allocation_free(CutAlignment::Elliptic {
        order: ButterworthOrder::of(8),
        tolerances: EllipticTolerances::new(
            PassbandRipple::new(Db::new(0.5)).unwrap(),
            StopbandAttenuation::new(Db::new(60.0)).unwrap(),
        )
        .unwrap(),
    });
}

fn cut_filter_is_allocation_free(alignment: CutAlignment) {
    let config = CutFilterConfig::new(ChannelLayout::STEREO, CutResponse::LowPass, alignment);
    let (mut rig, controls) = BlockRig::new(CutFilterNode::from(config), SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
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
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    for i in 0..240 {
        controls.set(UnitParam::Cutoff, cutoff(i));
        rig.block();
    }
}
