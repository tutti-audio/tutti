//! The filter rows: the ladder, the EQ band and the SVF held and with their cells moved
//! (the paths that design coefficients on the audio thread).

use crate::harness::{Gate, feeds};
use tutti_buffers::ChannelLayout;
use tutti_nodes::{
    EqBandConfig, EqBandNode, LadderFilterConfig, LadderFilterNode, LadderType, SvfFilterConfig,
    SvfFilterNode, SvfType,
};
use tutti_units::{Db, Hz, Q, Resonance, UnitParam};

/// The rows of this file.
pub const ROWS: &[fn()] = &[held, svf_cutoff_moving, ladder_cutoff_and_drive_moving];

fn ladder(width: impl Into<ChannelLayout>) -> LadderFilterNode<f32> {
    LadderFilterNode::from(
        LadderFilterConfig::new(width, LadderType::LP24)
            .freq(Hz::new(1_000.0))
            .res(Resonance::new(0.6)),
    )
}

/// Held at widths 1, 2 and 6: the per-channel integrator state is built at construction.
fn held() {
    let gate = Gate::new(feeds::slow_square);
    gate.run("ladder mono", ladder(ChannelLayout::MONO), |_, _| {});
    gate.run("ladder 6ch", ladder(6u16), |_, _| {});
    let eq_band = EqBandConfig::new(SvfType::Bell)
        .freq(Hz::new(1_000.0))
        .q(Q::new(1.0))
        .gain(Db::new(6.0));
    gate.run("eq band", EqBandNode::<f32>::from(eq_band), |_, _| {});
    let svfs = [
        ("mono", ChannelLayout::MONO, SvfType::LowPass, 800.0),
        ("stereo", ChannelLayout::STEREO, SvfType::HighPass, 200.0),
        ("6ch", ChannelLayout::from(6u16), SvfType::LowPass, 800.0),
    ];
    for (name, layout, kind, hz) in svfs {
        let config = SvfFilterConfig::new(layout, kind)
            .freq(Hz::new(hz))
            .q(Q::new(0.707));
        gate.run(
            &format!("svf {name}"),
            SvfFilterNode::<f32>::from(config),
            |_, _| {},
        );
    }
}

/// Every block nudges the cutoff as a host sets it, forcing a coefficient solve ramped across
/// the block.
fn svf_cutoff_moving() {
    let config = SvfFilterConfig::new(ChannelLayout::MONO, SvfType::Bell)
        .freq(Hz::new(1_000.0))
        .q(Q::new(1.0));
    let mut hz = 500.0f32;
    Gate::new(feeds::slow_square).run(
        "svf moving",
        SvfFilterNode::<f32>::from(config),
        move |c, _| {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 1.0 };
            c.set(UnitParam::Cutoff, f64::from(hz));
        },
    );
}

/// A cutoff or drive that moved since the last block takes the ramped path: a coefficient
/// solve every 16 samples and a per-sample drive ramp, both on the stack.
fn ladder_cutoff_and_drive_moving() {
    let mut hz = 500.0f32;
    Gate::new(feeds::slow_square).run("ladder moving", ladder(6u16), move |c, i| {
        hz = if hz > 4_000.0 { 500.0 } else { hz + 7.0 };
        c.set(UnitParam::Cutoff, f64::from(hz));
        c.set(UnitParam::Drive, f64::from(1.0 + (i % 7) as f32));
    });
}
