//! The dynamic EQ band does not allocate per block, held and with its centre and threshold
//! moved between blocks (a bell design every grid period and the glide of both filters).

use tutti_graph::contract::BlockRig;
use tutti_nodes::{DynamicEqConfig, DynamicEqNode, DynamicMode, KeySource};
use tutti_types::{ChannelLayout, CompressionRatio, Db, Hz, Q, SampleRate, UnitParam};

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the centre and the threshold
/// moved before each. Every input a square of eight frames a period, in the band of the
/// centres the test visits.
fn walk(node: DynamicEqNode) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, ch) in rig.inputs_mut().iter_mut().enumerate() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            let side = if (i / 4) % 2 == 0 { 0.5 } else { -0.5 };
            *x = side * (1.0 + 0.1 * c as f32);
        }
    }
    let centre = |i: usize| {
        if i.is_multiple_of(2) {
            3_000.0
        } else {
            6_000.0
        }
    };
    let threshold = |i: usize| -30.0 + (i % 7) as f32;
    for i in 0..16 {
        controls.set(UnitParam::Cutoff, centre(i));
        controls.set(UnitParam::Threshold, threshold(i));
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
            controls.set(UnitParam::Cutoff, centre(i));
            controls.set(UnitParam::Threshold, threshold(i));
            rig.block();
        }
    });
}

#[test]
fn dynamic_eq_stereo_process_is_allocation_free_with_the_band_moving() {
    walk(DynamicEqNode::from(DynamicEqConfig::de_esser(
        ChannelLayout::STEREO,
        Hz(6_000.0),
    )));
}

#[test]
fn dynamic_eq_upward_side_chain_process_is_allocation_free() {
    walk(DynamicEqNode::from(
        DynamicEqConfig::new(ChannelLayout::MONO, Hz(6_000.0), Q(1.5))
            .mode(DynamicMode::Upward)
            .ratio(CompressionRatio(3.0))
            .range(Db(9.0))
            .key(KeySource::Sidechain),
    ));
}
