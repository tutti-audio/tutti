//! Regression gate: the modal bank must not allocate per block, held and with the frequency and
//! decay moved between blocks.

use tutti_buffers::ChannelLayout;
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{ModalBankConfig, ModalBankNode, Mode, Modes};
use tutti_units::{Gain, Hz, SampleRate, Seconds, UnitParam};

/// `node` alone through `BlockRig`: a pulse train on every channel, 16 warm-up blocks, then 2 000
/// blocks under `assert_no_alloc`, `between` run before each.
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for ch in rig.inputs_mut().iter_mut() {
        let ch = ch.into_samples_mut();
        for (i, x) in ch.iter_mut().enumerate() {
            *x = if i.is_multiple_of(32) { 1.0 } else { 0.0 };
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
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            between(&controls, i);
            rig.block();
        }
    });
}

#[test]
fn modal_bank_process_is_allocation_free_with_the_frequency_and_decay_moving() {
    let modes = [(440.0, 0.4), (1_130.0, 0.2), (3_070.0, 0.1)].map(|(f, t)| Mode {
        frequency: Hz::new(f),
        decay: Seconds::new(t),
        gain: Gain::new(1.0),
    });
    gate(
        ModalBankNode::from(ModalBankConfig::new(
            ChannelLayout::STEREO,
            Modes::try_from(&modes[..]).unwrap(),
        )),
        |controls, i| {
            controls.set(
                UnitParam::Frequency,
                if i.is_multiple_of(2) { 220.0 } else { 880.0 },
            );
            controls.set(
                UnitParam::Decay,
                if i.is_multiple_of(3) { 0.2 } else { 1.5 },
            );
        },
    );
}
