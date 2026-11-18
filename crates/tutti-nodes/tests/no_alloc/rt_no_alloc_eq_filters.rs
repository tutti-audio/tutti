//! The bell and the tilt do not allocate per block, held and with every control moved
//! between blocks (a design into the held scratch and the coefficient glide).

use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{BellFilterConfig, BellFilterNode, TiltFilterConfig, TiltFilterNode};
use tutti_types::ChannelLayout;
use tutti_units::{Db, Hz, Q, SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, every control moved before each.
fn run<N: IntoNode>(node: N, moves: impl Fn(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 8) % 2 == 0 { 0.5 } else { -0.5 };
            *x = 0.25 + side * (c as f32 + 1.0) * 0.5;
        }
    }
    for i in 0..16 {
        moves(&controls, i);
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
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            moves(&controls, i);
            rig.block();
        }
    });
}

#[test]
fn bell_filter_process_is_allocation_free_with_every_control_moving() {
    let config = BellFilterConfig::new(ChannelLayout::STEREO, Hz(1_000.0), Q(1.0), Db(0.0));
    run(BellFilterNode::from(config), |c, i| {
        let even = i.is_multiple_of(2);
        c.set(UnitParam::Cutoff, if even { 80.0 } else { 15_000.0 });
        c.set(UnitParam::Q, if even { 0.5 } else { 12.0 });
        c.set(UnitParam::GainDb, if even { 15.0 } else { -15.0 });
    });
}

#[test]
fn tilt_filter_process_is_allocation_free_with_every_control_moving() {
    let config = TiltFilterConfig::new(ChannelLayout::STEREO, Hz(1_000.0), Db(0.0));
    run(TiltFilterNode::from(config), |c, i| {
        let even = i.is_multiple_of(2);
        c.set(UnitParam::Cutoff, if even { 80.0 } else { 15_000.0 });
        c.set(UnitParam::GainDb, if even { 15.0 } else { -15.0 });
    });
}
