//! The dynamics rows: each processor held on an alternating signal that keeps its envelope
//! follower active, at the widths and key paths it takes.

use crate::harness::{Gate, feeds};
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_nodes::{
    BrickwallLimiterConfig, BrickwallLimiterNode, CompressorConfig, CompressorControls,
    CompressorNode, GateConfig, GateControls, GateNode, KeyFilter, KeySource, LimiterConfig,
    LimiterControls, LimiterNode, PeakDetection,
};
use tutti_units::{CompressionRatio, Db, Seconds};

/// The rows of this file.
pub const ROWS: &[fn()] = &[compressors, limiters, brickwall_limiters, gates];

/// 2 000 blocks after 16 warm-up blocks, the sidechain wired.
fn keyed(amplitude: f32) -> Gate {
    Gate::new(feeds::alternating(amplitude)).blocks(2_000)
}

fn compressor(threshold: f64, ratio: f64, attack: f64, release: f64) -> CompressorControls {
    CompressorControls {
        threshold: Db::new(threshold),
        ratio: CompressionRatio::new(ratio),
        attack: Seconds::new(attack),
        release: Seconds::new(release),
        ..CompressorControls::DEFAULT
    }
}

fn compressors() {
    let mono = compressor(-20.0, 4.0, 0.005, 0.050);
    let node = |topology, controls| {
        CompressorNode::<f32>::from(
            CompressorConfig::new(topology)
                .key(KeySource::Sidechain)
                .controls(controls),
        )
    };
    keyed(0.7).run(
        "compressor mono",
        node(ChannelTopology::MONO, mono),
        |_, _| {},
    );
    let stereo = CompressorControls {
        knee: Db::new(6.0),
        ..compressor(-18.0, 3.0, 0.003, 0.080)
    };
    keyed(0.8).run(
        "compressor stereo",
        node(ChannelTopology::STEREO, stereo),
        |_, _| {},
    );
    // The key filter's stage: a high-pass on a mono compressor, a band-pass on a stereo one.
    for (topology, filter, amplitude) in [
        (ChannelTopology::MONO, KeyFilter::HighPass, 0.7),
        (ChannelTopology::STEREO, KeyFilter::BandPass, 0.6),
    ] {
        let node = CompressorNode::<f32>::from(
            CompressorConfig::new(topology)
                .key(KeySource::Sidechain)
                .key_filter(filter)
                .controls(mono),
        );
        keyed(amplitude).run(&format!("compressor {filter:?}"), node, |_, _| {});
    }
}

/// Over the ceiling, the warm-up fills the lookahead ring and primes the deque; the true-peak
/// up-samplers and the per-channel rings of the 6-wide path are built at prepare.
fn limiters() {
    let controls = LimiterControls {
        threshold: Db::new(-3.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    };
    let wide =
        ChannelTopology::default_for(ChannelLayout::from(6u16)).expect("at most one bus wide");
    let configs = [
        ("stereo", LimiterConfig::new(ChannelTopology::STEREO)),
        (
            "true peak",
            LimiterConfig::new(ChannelTopology::STEREO).detection(PeakDetection::TruePeak),
        ),
        ("6ch", LimiterConfig::new(wide)),
    ];
    for (name, config) in configs {
        let node = LimiterNode::<f32>::from(config.controls(controls));
        keyed(1.5)
            .warm(32)
            .run(&format!("limiter {name}"), node, |_, _| {});
    }
}

fn brickwall_limiters() {
    let wide = ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap();
    for (name, topology) in [("6ch", wide), ("stereo", ChannelTopology::STEREO)] {
        let node =
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(topology, Db::new(-0.3)));
        keyed(1.5)
            .warm(1)
            .blocks(5_000)
            .run(&format!("brickwall limiter {name}"), node, |_, _| {});
    }
}

fn gate_controls(threshold: f64, range: f64) -> GateControls {
    GateControls {
        threshold: Db::new(threshold),
        attack: Seconds::new(0.001),
        hold: Seconds::new(0.010),
        release: Seconds::new(0.100),
        range: Db::new(range),
        ..GateControls::DEFAULT
    }
}

fn gates() {
    let node = |topology, controls| {
        GateNode::<f32>::from(
            GateConfig::new(topology)
                .key(KeySource::Sidechain)
                .controls(controls),
        )
    };
    let gate = |amplitude, name: &str, node| keyed(amplitude).run(name, node, |_, _| {});
    gate(
        0.5,
        "gate mono",
        node(ChannelTopology::MONO, gate_controls(-40.0, -60.0)),
    );
    gate(
        0.6,
        "gate stereo",
        node(ChannelTopology::STEREO, gate_controls(-40.0, -80.0)),
    );
    // A downward expander (a finite ratio) is a gate with a level-dependent closed gain.
    let expander = GateControls {
        ratio: CompressionRatio::new(3.0),
        ..gate_controls(-10.0, -60.0)
    };
    gate(0.1, "expander", node(ChannelTopology::STEREO, expander));
    for (topology, filter, amplitude) in [
        (ChannelTopology::MONO, KeyFilter::HighPass, 0.5),
        (ChannelTopology::STEREO, KeyFilter::BandPass, 0.6),
    ] {
        let node = GateNode::<f32>::from(
            GateConfig::new(topology)
                .key(KeySource::Sidechain)
                .key_filter(filter)
                .controls(gate_controls(-60.0, -20.0)),
        );
        gate(amplitude, &format!("gate {filter:?}"), node);
    }
}
