//! The bell, the tilt and the graphic equalizer do not allocate per block, held and with every control moved
//! between blocks (a design into the held scratch and the coefficient ramp).

use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    BellFilterConfig, BellFilterNode, GraphicBands, GraphicEqConfig, GraphicEqNode,
    TiltFilterConfig, TiltFilterNode,
};
use tutti_units::{Db, Hz, Q, SampleRate, UnitAddr, UnitParam};

/// 16 warm-up blocks, then 240 blocks under `RtScope`, every control moved before each.
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
    for i in 0..240 {
        moves(&controls, i);
        rig.block();
    }
}

#[test]
fn bell_filter_process_is_allocation_free_with_every_control_moving() {
    let config = BellFilterConfig::new(
        ChannelLayout::STEREO,
        Hz::new(1_000.0),
        Q::new(1.0),
        Db::new(0.0),
    );
    run(BellFilterNode::from(config), |c, i| {
        let even = i.is_multiple_of(2);
        c.set(UnitParam::Cutoff, if even { 80.0 } else { 15_000.0 });
        c.set(UnitParam::Q, if even { 0.5 } else { 12.0 });
        c.set(UnitParam::GainDb, if even { 15.0 } else { -15.0 });
    });
}

#[test]
fn tilt_filter_process_is_allocation_free_with_every_control_moving() {
    let config = TiltFilterConfig::new(ChannelLayout::STEREO, Hz::new(1_000.0), Db::new(0.0));
    run(TiltFilterNode::from(config), |c, i| {
        let even = i.is_multiple_of(2);
        c.set(UnitParam::Cutoff, if even { 80.0 } else { 15_000.0 });
        c.set(UnitParam::GainDb, if even { 15.0 } else { -15.0 });
    });
}

#[test]
fn graphic_eq_process_is_allocation_free_with_every_band_moving() {
    for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
        let config = GraphicEqConfig::new(BusTopology::STEREO, bands);
        run(GraphicEqNode::from(config), |c, i| {
            for band in 0..bands.count() {
                let up = (i + band).is_multiple_of(2);
                let addr = UnitAddr::new(UnitParam::GainDb, u8::try_from(band).unwrap());
                c.set(addr, if up { 15.0 } else { -15.0 });
            }
        });
    }
}
