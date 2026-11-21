//! The envelope filter does not allocate per block, with its base cutoff and depth moved between
//! blocks.

use tutti_buffers::ChannelLayout;
use tutti_dsp::SvfType;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{EnvelopeFilterConfig, EnvelopeFilterNode};
use tutti_units::{Hz, SampleRate, Semitones, UnitParam};

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
fn envelope_filter_process_is_allocation_free_with_the_base_moving() {
    let config =
        EnvelopeFilterConfig::new(ChannelLayout::STEREO, Hz::new(400.0), SvfType::BandPass)
            .depth(Semitones::new(36.0));
    let (mut rig, controls) = BlockRig::new(
        EnvelopeFilterNode::<f32>::from(config),
        SampleRate::SR_48K,
        64,
    );
    feed(&mut rig);
    let base = |i: usize| if i.is_multiple_of(2) { 300.0 } else { 3_000.0 };
    for i in 0..16 {
        controls.set(UnitParam::Cutoff, base(i));
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
        controls.set(UnitParam::Cutoff, base(i));
        controls.set(
            UnitParam::Semitones,
            if i.is_multiple_of(3) { -12.0 } else { 36.0 },
        );
        rig.block();
    }
}
