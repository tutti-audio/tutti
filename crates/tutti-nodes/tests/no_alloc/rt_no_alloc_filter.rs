//! Regression gate: filter / EQ nodes must not allocate per-buffer.
//!
//! Covers `LadderFilterNode`, `EqBandNode`, `SvfFilterNode`, and the wide
//! `SvfFilterNode`, held, with their cells moved between blocks, and with their param
//! ports fed per frame: the paths that design coefficients on the audio thread and cut a
//! block at grid points and corners.

use tutti_buffers::ChannelLayout;
use tutti_graph::IntoNode;
use tutti_graph::contract::{BlockRig, Direct};
use tutti_nodes::{
    EqBandConfig, EqBandNode, LadderFilterConfig, LadderFilterNode, LadderType, SvfFilterConfig,
    SvfFilterNode, SvfType,
};
use tutti_units::Db;
use tutti_units::Hz;
use tutti_units::Q;
use tutti_units::Resonance;
use tutti_units::SampleRate;

/// A node through `BlockRig`: 16 warm-up blocks, then 240 blocks
/// under `RtScope`, `between` run before each (a control move).
fn gate<N: IntoNode>(node: N, mut between: impl FnMut(&N::Controls, usize)) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    // A 32-frame square: low enough that every filter here passes some of
    // it (a Nyquist square through a low-pass is silence, and the silence
    // check below would fail for the wrong reason).
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if (i / 16) % 2 == 0 { 0.5 } else { -0.5 };
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
fn ladder_filter_process_is_allocation_free() {
    gate(
        LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(ChannelLayout::MONO, LadderType::LP24)
                .freq(Hz::new(1_000.0))
                .res(Resonance::new(0.6)),
        ),
        |_, _| {},
    );
}

#[test]
fn eq_band_process_is_allocation_free() {
    gate(
        EqBandNode::<f32>::from(
            EqBandConfig::new(SvfType::Bell)
                .freq(Hz::new(1_000.0))
                .q(Q::new(1.0))
                .gain(Db::new(6.0)),
        ),
        |_, _| {},
    );
}

#[test]
fn svf_filter_mono_process_is_allocation_free() {
    gate(
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(800.0))
                .q(Q::new(0.707)),
        ),
        |_, _| {},
    );
}

#[test]
fn svf_filter_stereo_process_is_allocation_free() {
    gate(
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(tutti_buffers::ChannelLayout::STEREO, SvfType::HighPass)
                .freq(Hz::new(200.0))
                .q(Q::new(0.707)),
        ),
        |_, _| {},
    );
}

#[test]
fn svf_filter_wide_6ch_process_is_allocation_free() {
    // The per-channel integrator Vec must be built at construction — a
    // regression that (re)allocated it per buffer, or per-channel scratch in
    // `process`, would trip here.
    gate(
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(6u16, SvfType::LowPass)
                .freq(Hz::new(800.0))
                .q(Q::new(0.707)),
        ),
        |_, _| {},
    );
}

#[test]
fn ladder_filter_wide_6ch_process_is_allocation_free() {
    gate(
        LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(6u16, LadderType::LP24)
                .freq(Hz::new(1_000.0))
                .res(Resonance::new(0.6)),
        ),
        |_, _| {},
    );
}

#[test]
fn svf_filter_with_parameter_changes_is_allocation_free() {
    // Exercises the moved-control branch: every block nudges the cutoff
    // (through the node's `ParamSet`, as a host sets it), forcing a
    // coefficient solve ramped across the block.
    let mut hz = 500.0f32;
    gate(
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::Bell)
                .freq(Hz::new(1_000.0))
                .q(Q::new(1.0)),
        ),
        move |params, _| {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 1.0 };
            params.set(tutti_units::UnitParam::Cutoff, f64::from(hz));
        },
    );
}

#[test]
fn ladder_with_moving_cutoff_and_drive_is_allocation_free() {
    // A cutoff or drive that moved since the last block takes the ramped
    // path: a coefficient solve every 16 samples and a per-sample drive ramp.
    // Both must stay on the stack. Moved through the node's `ParamSet`, as a
    // host sets them.
    let mut hz = 500.0f32;
    gate(
        LadderFilterNode::<f32>::from(
            LadderFilterConfig::new(6u16, LadderType::LP24)
                .freq(Hz::new(1_000.0))
                .res(Resonance::new(0.6)),
        ),
        move |params, i| {
            hz = if hz > 4_000.0 { 500.0 } else { hz + 7.0 };
            params.set(tutti_units::UnitParam::Cutoff, f64::from(hz));
            params.set(
                tutti_units::UnitParam::Drive,
                f64::from(1.0 + (i % 7) as f32),
            );
        },
    );
}

/// The SVF with its cutoff and Q ports fed per frame, at widths 1, 2, 6, 9 and 16: every
/// grid point designs and retargets, every block is cut, and the bank runs its blended
/// and held frames in one lane group and in several. Through `Direct`, which feeds a
/// node's param ports (it copies the control signal, so the same 64 frames play each block).
#[test]
fn an_svf_with_its_ports_fed_is_allocation_free() {
    for width in [1u16, 2, 6, 9, 16] {
        let node = SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(width, SvfType::Bell)
                .freq(Hz::new(1_000.0))
                .q(Q::new(1.0)),
        );
        let mut rig = Direct::new(node, SampleRate::SR_48K, 64);
        for c in rig.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
            for (i, x) in c.iter_mut().enumerate() {
                *x = if (i / 16) % 2 == 0 { 0.5 } else { -0.5 };
            }
        }
        let tau = std::f32::consts::TAU / 64.0;
        let cutoff: Vec<f32> = (0..64)
            .map(|i| 1_000.0 + 800.0 * (tau * i as f32).sin())
            .collect();
        let q: Vec<f32> = (0..64)
            .map(|i| 1.0 + 0.5 * (tau * 3.0 * i as f32).sin())
            .collect();
        rig.feed(0, Some(&cutoff));
        rig.feed(1, Some(&q));
        for _ in 0..32 {
            rig.block();
        }
        assert!(
            rig.output()
                .plane(0)
                .samples()
                .iter()
                .any(|s| s.abs() > 1e-6),
            "the SVF rendered silence; the gate would walk no DSP"
        );
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..2_000 {
                rig.block();
            }
        });
    }
}
