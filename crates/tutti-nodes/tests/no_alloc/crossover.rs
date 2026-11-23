//! The splitter rows: the crossover and the multiband compressor on a broadband square, held
//! and with every param moving each block (a coefficient solve every few samples, a solo's
//! mute ramp and a bypass).

use crate::harness::{Gate, feeds};
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::ParamNode;
use tutti_nodes::{
    BAND_PARAMS, CrossoverConfig, CrossoverControls, CrossoverNode, MultibandBands,
    MultibandCompressorConfig, MultibandCompressorNode,
};
use tutti_units::{Hz, UnitAddr, UnitParam};

/// The rows of this file.
pub const ROWS: &[fn()] = &[crossover, multiband];

fn crossover() {
    let gate = Gate::new(feeds::slow_square(0.8));
    let node = |cutoffs: &[Hz]| {
        CrossoverNode::<f32>::try_from(CrossoverConfig::new(
            BusTopology::STEREO,
            CrossoverControls::try_from(cutoffs).unwrap(),
        ))
        .unwrap()
    };
    let held = node(&[Hz::new(120.0), Hz::new(1_000.0), Hz::new(6_000.0)]);
    gate.run("crossover", held, |_, _| {});
    let mut hz = 200.0f32;
    gate.run(
        "crossover moving",
        node(&[Hz::new(200.0), Hz::new(3_000.0)]),
        move |c, _| {
            hz = if hz > 2_000.0 { 200.0 } else { hz + 3.0 };
            assert!(c.set(UnitAddr::new(UnitParam::Cutoff, 0), f64::from(hz)));
            assert!(c.set(UnitAddr::new(UnitParam::Cutoff, 1), f64::from(3_000.0 + hz)));
        },
    );
}

fn multiband() {
    let gate = Gate::new(feeds::slow_square(0.8));
    for bands in 2..=4 {
        let node = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::STEREO,
            MultibandBands::new(bands).unwrap(),
        ));
        let set = node.param_set();
        for b in 0..bands {
            assert!(set.set(UnitAddr::new(UnitParam::Threshold, b), -24.0));
            assert!(set.set(UnitAddr::new(UnitParam::Ratio, b), 4.0));
            assert!(set.set(UnitAddr::new(UnitParam::GainDb, b), 3.0));
        }
        gate.run(&format!("multiband {bands} bands"), node, |_, _| {});
    }
    // Four bands, 6 wide: every band param, every crossover and both toggles are written each
    // block (27 ports and 8 toggle cells).
    let node = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
        BusTopology::default_for(ChannelLayout::from_count(6)).unwrap(),
        MultibandBands::of(4),
    ));
    let mut hz = 120.0f32;
    gate.run("multiband moving", node, move |controls, i| {
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
