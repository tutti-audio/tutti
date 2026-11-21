//! Regression gate: the FM and additive sources must not allocate per block, held and with
//! their frequency, level and index moved between blocks.

use tutti_dsp::{
    Algorithm, FmPatch, OperatorCount, OperatorFrequency, OperatorSpec, Partial, Partials,
};
use tutti_graph::contract::BlockRig;
use tutti_nodes::{AdditiveConfig, AdditiveNode, FmConfig, FmNode};
use tutti_units::{Amplitude, FrequencyRatio, ModulationIndex, SampleRate, UnitAddr, UnitParam};

fn ratio(x: f64) -> OperatorFrequency {
    OperatorFrequency::Ratio(FrequencyRatio::new(x).unwrap())
}

/// 16 warm-up blocks, then 2 000 blocks under `assert_no_alloc`, the frequency and the index
/// moved before each.
#[test]
fn fm_process_is_allocation_free_with_the_frequency_and_index_moving() {
    let patch = FmPatch::new(
        Algorithm::chain(OperatorCount::of(3)),
        &[
            OperatorSpec::new(ratio(1.0)),
            OperatorSpec::new(ratio(2.0)).level(Amplitude::new(0.5)),
            OperatorSpec::new(ratio(1.0)),
        ],
    )
    .unwrap();
    let node = FmNode::<f32>::from(FmConfig::new(patch).index(ModulationIndex::of(2.0)));
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 480);
    let set = |i: usize| {
        controls.set(UnitParam::Frequency, 110.0 * (1 + i % 5) as f64);
        controls.set(UnitAddr::new(UnitParam::Index, 0), (i % 7) as f64);
    };
    for i in 0..16 {
        set(i);
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            set(i);
            rig.block();
        }
    });
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-3),
        "the node rendered silence; the gate would walk no DSP"
    );
}

#[test]
fn additive_process_is_allocation_free_with_the_frequency_moving() {
    let partial = |ratio: f64, amplitude: f64| {
        Partial::new(
            FrequencyRatio::new(ratio).unwrap(),
            Amplitude::new(amplitude),
        )
    };
    let partials =
        Partials::new(&[partial(1.0, 0.5), partial(2.0, 0.25), partial(40.0, 0.1)]).unwrap();
    let (mut rig, controls) = BlockRig::new(
        AdditiveNode::<f32>::from(AdditiveConfig::new(partials)),
        SampleRate::SR_48K,
        480,
    );
    for i in 0..16 {
        controls.set(UnitParam::Frequency, 220.0 * (1 + i % 4) as f64);
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            controls.set(UnitParam::Frequency, 220.0 * (1 + i % 4) as f64);
            rig.block();
        }
    });
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-3),
        "the node rendered silence; the gate would walk no DSP"
    );
}
