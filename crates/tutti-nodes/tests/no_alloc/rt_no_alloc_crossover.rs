//! Regression gate: the crossover and the multiband compressor must not
//! allocate per block, including while every param moves each block.
//!
//! Each node runs alone in a graph through `tutti_graph::contract::BlockRig`.
//! The moving-param gates write every band's threshold, knee, ratio, attack,
//! release and make-up, every crossover and both toggles through the node's
//! `ParamSet` before each block, so they take the ramped path (a coefficient
//! solve every few samples), a solo's mute ramp and a bypass.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::contract::BlockRig;
use tutti_graph::{IntoNode, ParamNode};
use tutti_nodes::{
    BAND_PARAMS, CrossoverConfig, CrossoverControls, CrossoverNode, MultibandBands,
    MultibandCompressorConfig, MultibandCompressorNode,
};
use tutti_units::{Hz, SampleRate};
use tutti_units::{UnitAddr, UnitParam};

/// `node` through `BlockRig`: a 32-frame square (broadband, so every band is
/// driven), 16 warm-up blocks, then 240 blocks under `RtScope`,
/// `between` run before each.
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if (i / 16) % 2 == 0 { 0.8 } else { -0.8 };
        }
    }
    for i in 0..16 {
        between(&controls, i);
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
        between(&controls, i);
        rig.block();
    }
}

#[test]
fn crossover_node_process_is_allocation_free() {
    let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
        ChannelTopology::STEREO,
        CrossoverControls::try_from(&[Hz::new(120.0), Hz::new(1_000.0), Hz::new(6_000.0)][..])
            .unwrap(),
    ))
    .unwrap();
    gate(node, |_, _| {});
}

#[test]
fn crossover_node_with_a_moving_frequency_is_allocation_free() {
    let node = CrossoverNode::<f32>::try_from(CrossoverConfig::new(
        ChannelTopology::STEREO,
        CrossoverControls::try_from(&[Hz::new(200.0), Hz::new(3_000.0)][..]).unwrap(),
    ))
    .unwrap();
    let mut hz = 200.0f32;
    gate(node, move |controls, _| {
        hz = if hz > 2_000.0 { 200.0 } else { hz + 3.0 };
        assert!(controls.set(UnitAddr::new(UnitParam::Cutoff, 0), f64::from(hz)));
        assert!(controls.set(UnitAddr::new(UnitParam::Cutoff, 1), f64::from(3_000.0 + hz)));
    });
}

#[test]
fn multiband_compressor_process_is_allocation_free() {
    for bands in 2..=4 {
        let node = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            ChannelTopology::STEREO,
            MultibandBands::new(bands).unwrap(),
        ));
        let set = node.param_set();
        for b in 0..bands {
            assert!(set.set(UnitAddr::new(UnitParam::Threshold, b), -24.0));
            assert!(set.set(UnitAddr::new(UnitParam::Ratio, b), 4.0));
            assert!(set.set(UnitAddr::new(UnitParam::GainDb, b), 3.0));
        }
        gate(node, |_, _| {});
    }
}

#[test]
fn multiband_compressor_with_moving_params_is_allocation_free() {
    // Four bands: every band param, every crossover and both toggles are
    // written each block (27 ports and 8 toggle cells).
    let node = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
        ChannelTopology::default_for(ChannelLayout::from_count(6)).unwrap(),
        MultibandBands::of(4),
    ));
    let mut hz = 120.0f32;
    gate(node, move |controls, i| {
        let set = controls.params();
        hz = if hz > 400.0 { 120.0 } else { hz + 1.0 };
        for (c, scale) in [1.0, 4.0, 40.0].into_iter().enumerate() {
            assert!(set.set(
                UnitAddr::new(UnitParam::Cutoff, c as u8),
                f64::from(hz * scale)
            ));
        }
        for b in 0..4u8 {
            let k = i + usize::from(b);
            let values = [
                -30.0 + (k % 11) as f32,
                (k % 9) as f32,
                1.0 + (k % 6) as f32,
                0.001 + (k % 5) as f32 * 0.002,
                0.01 + (k % 7) as f32 * 0.01,
                (k % 5) as f32,
            ];
            for (kind, v) in BAND_PARAMS.into_iter().zip(values) {
                assert!(set.set(UnitAddr::new(kind, b), f64::from(v)));
            }
            assert!(set.set(UnitAddr::new(UnitParam::Bypass, b), f64::from(k % 7 == 0)));
            assert!(set.set(UnitAddr::new(UnitParam::Solo, b), f64::from(k % 13 == 0)));
        }
    });
}
